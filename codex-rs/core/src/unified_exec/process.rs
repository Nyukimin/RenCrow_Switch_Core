// Modified by RenCrow Switch Core, 2026-11-18: expose command-owned approval cancellation to canonical completion.
#![allow(clippy::module_inception)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::ProcessSignal as ExecServerProcessSignal;
use codex_exec_server::ReadResponse as ExecReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteStatus;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::TruncationPolicy;
use codex_sandboxing::SandboxType;
use codex_sandboxing::is_likely_sandbox_denied;
use codex_sandboxing::record_filesystem_sandbox_violation;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use codex_utils_pty::SpawnedPty;

use super::SharedPluginMetricsSidecar;
use super::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use super::UNIFIED_EXEC_OUTPUT_MAX_TOKENS;
use super::UnifiedExecError;
use super::head_tail_buffer::HeadTailBuffer;
use super::process_state::ProcessState;
use crate::shell_snapshot::ShellSnapshotFile;

const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);
pub(crate) trait SpawnLifecycle: std::fmt::Debug + Send + Sync {
    /// Returns file descriptors that must stay open across the child `exec()`.
    ///
    /// The returned descriptors must already be valid in the parent process and
    /// stay valid until `after_spawn()` runs, which is the first point where
    /// the parent may release its copies.
    fn inherited_fds(&self) -> Vec<i32> {
        Vec::new()
    }

    fn after_spawn(&mut self) {}

    fn cancellation_disposition(
        &self,
        _call_id: &str,
        _turn_id: &str,
    ) -> Option<ExecCancellationDisposition> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecCancellationDisposition {
    ExplicitUserCancellation,
    Interrupted,
}

pub(crate) type SpawnLifecycleHandle = Box<dyn SpawnLifecycle>;

pub(crate) enum StartupProcessResolution {
    Abandoned,
    Discarded,
}

pub(crate) struct StartupProcessResources {
    pub(crate) process: Arc<UnifiedExecProcess>,
    pub(crate) early_exit_rx: Option<oneshot::Receiver<i32>>,
    pub(crate) started_at: tokio::time::Instant,
    pub(crate) metrics_sidecar: Option<SharedPluginMetricsSidecar>,
}

pub(crate) trait StartupProcessOwner: std::fmt::Debug + Send + Sync {
    fn resolve(
        &self,
        resources: StartupProcessResources,
        resolution: StartupProcessResolution,
    ) -> Result<(), StartupProcessResources>;
}

pub(crate) struct StartupProcessGuard {
    process: Arc<UnifiedExecProcess>,
    early_exit_rx: Option<oneshot::Receiver<i32>>,
    owner: Option<Arc<dyn StartupProcessOwner>>,
    resolved: bool,
    started_at: tokio::time::Instant,
    metrics_sidecar: Option<SharedPluginMetricsSidecar>,
}

impl StartupProcessGuard {
    pub(crate) fn unowned(process: UnifiedExecProcess) -> Self {
        Self {
            process: Arc::new(process),
            early_exit_rx: None,
            owner: None,
            resolved: false,
            started_at: tokio::time::Instant::now(),
            metrics_sidecar: None,
        }
    }

    fn new(
        process: UnifiedExecProcess,
        early_exit_rx: oneshot::Receiver<i32>,
        owner: Option<Arc<dyn StartupProcessOwner>>,
        metrics_sidecar: Option<SharedPluginMetricsSidecar>,
    ) -> Self {
        Self {
            process: Arc::new(process),
            early_exit_rx: Some(early_exit_rx),
            owner,
            resolved: false,
            started_at: tokio::time::Instant::now(),
            metrics_sidecar,
        }
    }

    pub(crate) fn process(&self) -> &UnifiedExecProcess {
        self.process.as_ref()
    }

    pub(crate) fn process_arc(&self) -> Arc<UnifiedExecProcess> {
        Arc::clone(&self.process)
    }

    pub(crate) fn started_at(&self) -> tokio::time::Instant {
        self.started_at
    }

    pub(crate) fn set_shell_snapshot(
        &mut self,
        shell_snapshot: Option<Arc<ShellSnapshotFile>>,
    ) -> Result<(), Arc<UnifiedExecProcess>> {
        let Some(process) = Arc::get_mut(&mut self.process) else {
            return Err(Arc::clone(&self.process));
        };
        process._shell_snapshot = shell_snapshot;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn handoff(mut self) -> Arc<UnifiedExecProcess> {
        self.disarm();
        Arc::clone(&self.process)
    }

    pub(crate) fn disarm(&mut self) {
        self.resolved = true;
        self.early_exit_rx.take();
    }

    pub(crate) fn discard(mut self) {
        self.resolved = true;
        let resources = StartupProcessResources {
            process: Arc::clone(&self.process),
            early_exit_rx: self.early_exit_rx.take(),
            started_at: self.started_at,
            metrics_sidecar: self.metrics_sidecar.take(),
        };
        self.resolve_resources(resources, StartupProcessResolution::Discarded);
    }

    fn resolve_resources(
        &self,
        resources: StartupProcessResources,
        resolution: StartupProcessResolution,
    ) {
        let Some(owner) = self.owner.as_ref() else {
            self.resolve_without_owner(resources, resolution);
            return;
        };
        if let Err(resources) = owner.resolve(resources, resolution) {
            tracing::error!("unified exec startup completion owner rejected process handoff");
            resources.process.request_terminate_for_completion();
            let process = resources.process;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    process
                        .reap_after_completion_termination(resources.early_exit_rx)
                        .await;
                });
            }
        }
    }

    fn resolve_without_owner(
        &self,
        resources: StartupProcessResources,
        resolution: StartupProcessResolution,
    ) {
        match resolution {
            StartupProcessResolution::Abandoned => {
                resources.process.request_terminate_for_completion();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let process = Arc::clone(&resources.process);
                    runtime.spawn(async move {
                        process
                            .reap_after_completion_termination(resources.early_exit_rx)
                            .await;
                    });
                } else {
                    resources.process.terminate();
                }
            }
            StartupProcessResolution::Discarded => {
                resources.process.terminate();
            }
        }
    }
}

impl std::ops::Deref for StartupProcessGuard {
    type Target = UnifiedExecProcess;

    fn deref(&self) -> &Self::Target {
        self.process()
    }
}

impl Drop for StartupProcessGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        let resources = StartupProcessResources {
            process: Arc::clone(&self.process),
            early_exit_rx: self.early_exit_rx.take(),
            started_at: self.started_at,
            metrics_sidecar: self.metrics_sidecar.take(),
        };
        self.resolve_resources(resources, StartupProcessResolution::Abandoned);
    }
}

#[derive(Debug, Default)]
/// Spawn lifecycle that performs no extra setup around process launch.
pub(crate) struct NoopSpawnLifecycle;

impl SpawnLifecycle for NoopSpawnLifecycle {}

/// Shared output state exposed to polling and streaming consumers.
#[derive(Clone)]
pub(crate) struct OutputHandles<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    pub(crate) output_buffer: Arc<Mutex<HeadTailBuffer<MAX_BYTES>>>,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) output_closed: Arc<AtomicBool>,
    pub(crate) output_closed_notify: Arc<Notify>,
    pub(crate) cancellation_token: CancellationToken,
}

struct OutputTaskGuard {
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
}

impl Drop for OutputTaskGuard {
    fn drop(&mut self) {
        self.output_closed.store(true, Ordering::Release);
        self.output_closed_notify.notify_waiters();
    }
}

/// Transport-specific process handle used by unified exec.
enum ProcessHandle {
    Local(Box<ExecCommandSession>),
    ExecServer(Arc<dyn ExecProcess>),
}

/// Unified wrapper over directly spawned PTY sessions and exec-server-backed
/// processes.
pub(crate) struct UnifiedExecProcess {
    process_handle: ProcessHandle,
    output_tx: broadcast::Sender<Vec<u8>>,
    output: OutputHandles,
    // Owner-backed local runs retain a canonical bounded snapshot because the
    // initial response collector drains `output_buffer` before terminal emit.
    completion_output_buffer: Option<Arc<Mutex<HeadTailBuffer>>>,
    output_drained: Arc<Notify>,
    interaction_lock: Arc<Mutex<()>>,
    state_tx: watch::Sender<ProcessState>,
    state_rx: watch::Receiver<ProcessState>,
    output_task: Option<JoinHandle<()>>,
    sandbox_type: SandboxType,
    timed_out: AtomicBool,
    _spawn_lifecycle: Option<SpawnLifecycleHandle>,
    // The shell may still need to replay this file after process startup returns.
    pub(crate) _shell_snapshot: Option<Arc<ShellSnapshotFile>>,
}

impl std::fmt::Debug for UnifiedExecProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecProcess")
            .field("has_exited", &self.has_exited())
            .field("exit_code", &self.exit_code())
            .field("sandbox_type", &self.sandbox_type)
            .finish_non_exhaustive()
    }
}

impl UnifiedExecProcess {
    fn new(
        process_handle: ProcessHandle,
        sandbox_type: SandboxType,
        spawn_lifecycle: Option<SpawnLifecycleHandle>,
    ) -> Self {
        let output = OutputHandles {
            output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
            output_notify: Arc::new(Notify::new()),
            output_closed: Arc::new(AtomicBool::new(false)),
            output_closed_notify: Arc::new(Notify::new()),
            cancellation_token: CancellationToken::new(),
        };
        let output_drained = Arc::new(Notify::new());
        let (output_tx, _) = broadcast::channel(64);
        let (state_tx, state_rx) = watch::channel(ProcessState::default());

        Self {
            process_handle,
            output_tx,
            output,
            completion_output_buffer: None,
            output_drained,
            interaction_lock: Arc::new(Mutex::new(())),
            state_tx,
            state_rx,
            output_task: None,
            sandbox_type,
            timed_out: AtomicBool::new(false),
            _spawn_lifecycle: spawn_lifecycle,
            _shell_snapshot: None,
        }
    }

    pub(crate) fn cancellation_disposition(
        &self,
        call_id: &str,
        turn_id: &str,
    ) -> Option<ExecCancellationDisposition> {
        self._spawn_lifecycle
            .as_ref()?
            .cancellation_disposition(call_id, turn_id)
    }

    pub(super) async fn write(&self, data: &[u8]) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .writer_sender()
                .send(data.to_vec())
                .await
                .map_err(|_| UnifiedExecError::WriteToStdin),
            ProcessHandle::ExecServer(process_handle) => {
                match process_handle.write(data.to_vec()).await {
                    Ok(response) => match response.status {
                        WriteStatus::Accepted => Ok(()),
                        WriteStatus::UnknownProcess | WriteStatus::StdinClosed => {
                            let state = self.state_rx.borrow().clone();
                            let _ = self.state_tx.send_replace(state.exited(state.exit_code));
                            self.output.cancellation_token.cancel();
                            Err(UnifiedExecError::WriteToStdin)
                        }
                        WriteStatus::Starting => Err(UnifiedExecError::WriteToStdin),
                    },
                    Err(err) => Err(UnifiedExecError::process_failed(err.to_string())),
                }
            }
        }
    }

    pub(super) fn output_handles(&self) -> &OutputHandles {
        &self.output
    }

    pub(super) fn output_receiver(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.output.cancellation_token.clone()
    }

    pub(super) fn output_drained_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.output_drained)
    }

    pub(super) fn interaction_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.interaction_lock)
    }

    pub(super) fn has_exited(&self) -> bool {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => state.has_exited || process_handle.has_exited(),
            ProcessHandle::ExecServer(_) => state.has_exited,
        }
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        if self.timed_out() {
            return Some(124);
        }
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                state.exit_code.or_else(|| process_handle.exit_code())
            }
            ProcessHandle::ExecServer(_) => state.exit_code,
        }
    }

    pub(super) fn mark_timed_out(&self) {
        self.timed_out.store(true, Ordering::Release);
    }

    pub(super) fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }

    fn finish_termination(&self) {
        self.output.cancellation_token.cancel();
        if let Some(output_task) = &self.output_task {
            output_task.abort();
        }
    }

    pub(super) fn terminate(&self) {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                tokio::spawn(async move {
                    let _ = process_handle.terminate().await;
                });
            }
        }
        self.finish_termination();
    }

    pub(crate) fn request_terminate_for_completion(&self) {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.request_terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                tokio::spawn(async move {
                    let _ = process_handle.terminate().await;
                });
            }
        }
    }

    pub(crate) async fn reap_after_completion_termination(
        &self,
        early_exit_rx: Option<oneshot::Receiver<i32>>,
    ) {
        if let Some(exit_rx) = early_exit_rx {
            self.signal_exit(exit_rx.await.ok());
        } else {
            let mut state_rx = self.state_rx.clone();
            while !state_rx.borrow().has_exited {
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
        }

        if self.output.output_closed.load(Ordering::Acquire) {
            return;
        }
        let output_closed = self.output.output_closed_notify.notified();
        tokio::pin!(output_closed);
        output_closed.as_mut().enable();
        if self.output.output_closed.load(Ordering::Acquire) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), output_closed).await;
    }

    pub(super) async fn terminate_confirmed(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                process_handle
                    .terminate()
                    .await
                    .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
            }
        }
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(super) async fn interrupt(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .signal(PtyProcessSignal::Interrupt)
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
            ProcessHandle::ExecServer(process_handle) => process_handle
                .signal(ExecServerProcessSignal::Interrupt)
                .await
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
        }
    }

    pub(super) fn fail_and_terminate(&self, message: String) {
        let state = self.state_rx.borrow().clone();
        if state.failure_message.is_none() {
            let _ = self.state_tx.send_replace(state.failed(message));
        }
        self.terminate();
    }

    pub(crate) async fn snapshot_output(&self) -> Vec<u8> {
        let guard = self.output.output_buffer.lock().await;
        guard.to_bytes()
    }

    pub(crate) async fn snapshot_completion_output(&self) -> Option<Vec<u8>> {
        let output_buffer = self.completion_output_buffer.as_ref()?;
        let guard = output_buffer.lock().await;
        Some(guard.to_bytes_with_omission_marker())
    }

    pub(crate) fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    pub(super) fn failure_message(&self) -> Option<String> {
        self.state_rx.borrow().failure_message.clone()
    }

    pub(super) async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            self.output.output_notify.notified(),
        )
        .await;

        let aggregated = self.snapshot_output().await;
        let aggregated_text = String::from_utf8_lossy(&aggregated);
        self.check_for_sandbox_denial_with_text(aggregated_text.as_ref())
            .await?;

        Ok(())
    }

    pub(super) async fn check_for_sandbox_denial_with_text(
        &self,
        text: &str,
    ) -> Result<(), UnifiedExecError> {
        let executor_reported_denial = self.state_rx.borrow().sandbox_denied;
        let sandbox_type = self.sandbox_type();
        if !self.has_exited() || (!executor_reported_denial && sandbox_type == SandboxType::None) {
            return Ok(());
        }

        let exit_code = self.exit_code().unwrap_or(-1);
        let exec_output = ExecToolCallOutput {
            exit_code,
            stderr: StreamOutput::new(text.to_string()),
            aggregated_output: StreamOutput::new(text.to_string()),
            ..Default::default()
        };
        let likely_sandbox_denial = is_likely_sandbox_denied(sandbox_type, &exec_output);
        if likely_sandbox_denial {
            record_filesystem_sandbox_violation(sandbox_type, &exec_output);
        }
        if executor_reported_denial || likely_sandbox_denial {
            let snippet = formatted_truncate_text(
                text,
                TruncationPolicy::Tokens(UNIFIED_EXEC_OUTPUT_MAX_TOKENS),
            );
            let message = if snippet.is_empty() {
                format!("Process exited with code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message, exec_output));
        }
        Ok(())
    }

    pub(super) async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
        spawn_lifecycle: SpawnLifecycleHandle,
        owner: Option<Arc<dyn StartupProcessOwner>>,
        metrics_sidecar: Option<SharedPluginMetricsSidecar>,
    ) -> Result<StartupProcessGuard, UnifiedExecError> {
        let SpawnedPty {
            session: process_handle,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        let output_rx = codex_utils_pty::combine_output_receivers(stdout_rx, stderr_rx);
        let mut managed = Self::new(
            ProcessHandle::Local(Box::new(process_handle)),
            sandbox_type,
            Some(spawn_lifecycle),
        );
        let completion_output_buffer = owner
            .as_ref()
            .map(|_| Arc::new(Mutex::new(HeadTailBuffer::default())));
        managed.completion_output_buffer = completion_output_buffer.clone();
        managed.output_task = Some(Self::spawn_local_output_task(
            output_rx,
            managed.output_handles().clone(),
            managed.output_tx.clone(),
            completion_output_buffer,
        ));
        let mut guard = StartupProcessGuard::new(managed, exit_rx, owner, metrics_sidecar);

        let early_exit = match guard.early_exit_rx.as_mut() {
            Some(exit_rx) => exit_rx.try_recv(),
            None => {
                return Err(UnifiedExecError::process_failed(
                    "spawned process lost its startup exit receiver".to_string(),
                ));
            }
        };
        match early_exit {
            Ok(exit_code) => {
                guard.early_exit_rx.take();
                guard.process().signal_exit(Some(exit_code));
                if let Err(error) = guard.process().check_for_sandbox_denial().await {
                    guard.discard();
                    return Err(error);
                }
                return Ok(guard);
            }
            Err(TryRecvError::Closed) => {
                guard.early_exit_rx.take();
                guard.process().signal_exit(/*exit_code*/ None);
                if let Err(error) = guard.process().check_for_sandbox_denial().await {
                    guard.discard();
                    return Err(error);
                }
                return Ok(guard);
            }
            Err(TryRecvError::Empty) => {}
        }

        let early_exit_result = match guard.early_exit_rx.as_mut() {
            Some(exit_rx) => tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, exit_rx).await,
            None => {
                return Err(UnifiedExecError::process_failed(
                    "spawned process lost its startup exit receiver".to_string(),
                ));
            }
        };
        if let Ok(exit_result) = early_exit_result {
            guard.early_exit_rx.take();
            guard.process().signal_exit(exit_result.ok());
            if let Err(error) = guard.process().check_for_sandbox_denial().await {
                guard.discard();
                return Err(error);
            }
            return Ok(guard);
        }

        let Some(exit_rx) = guard.early_exit_rx.take() else {
            return Err(UnifiedExecError::process_failed(
                "spawned process lost its startup exit receiver".to_string(),
            ));
        };
        tokio::spawn({
            let state_tx = guard.process().state_tx.clone();
            let cancellation_token = guard.process().output.cancellation_token.clone();
            async move {
                let exit_code = exit_rx.await.ok();
                let state = state_tx.borrow().clone();
                let _ = state_tx.send_replace(state.exited(exit_code));
                cancellation_token.cancel();
            }
        });

        Ok(guard)
    }

    pub(super) async fn from_exec_server_started(
        started: StartedExecProcess,
    ) -> Result<Self, UnifiedExecError> {
        let process_handle = ProcessHandle::ExecServer(Arc::clone(&started.process));
        // Older peers do not report this field. In that case, skip local
        // classification rather than attributing a violation to a guessed backend.
        let sandbox_type = started.sandbox_type.unwrap_or(SandboxType::None);
        let mut managed = Self::new(process_handle, sandbox_type, /*spawn_lifecycle*/ None);
        let output_handles = managed.output_handles().clone();
        managed.output_task = Some(Self::spawn_exec_server_output_task(
            started,
            output_handles,
            managed.output_tx.clone(),
            managed.state_tx.clone(),
        ));

        let mut state_rx = managed.state_rx.clone();
        if tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, async {
            loop {
                let state = state_rx.borrow().clone();
                if state.has_exited || state.failure_message.is_some() {
                    break;
                }
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
        {
            managed.check_for_sandbox_denial().await?;
        }

        Ok(managed)
    }

    fn spawn_exec_server_output_task(
        started: StartedExecProcess,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        state_tx: watch::Sender<ProcessState>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
        } = output_handles;
        let process = started.process;
        let mut events = process.subscribe_events();
        tokio::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            let mut last_seq: u64 = 0;
            loop {
                let event = match events.recv().await {
                    Ok(event) => Some(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(
                            state.failed("exec-server process event stream closed".to_string()),
                        );
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                };
                let event_seq = event.as_ref().and_then(|event| match event {
                    ExecProcessEvent::Output(chunk) => Some(chunk.seq),
                    ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq } => {
                        Some(*seq)
                    }
                    ExecProcessEvent::Failed(_) => None,
                });
                let missing_sandbox_denial = matches!(
                    event.as_ref(),
                    Some(ExecProcessEvent::Exited {
                        sandbox_denied: None,
                        ..
                    })
                );
                if event.is_none()
                    || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1))
                    || missing_sandbox_denial
                {
                    let response = match process
                        .read(
                            Some(last_seq),
                            /*max_bytes*/ None,
                            /*wait_ms*/ Some(0),
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(err) => {
                            let state = state_tx.borrow().clone();
                            let _ = state_tx.send_replace(state.failed(err.to_string()));
                            output_closed.store(true, Ordering::Release);
                            output_closed_notify.notify_waiters();
                            cancellation_token.cancel();
                            break;
                        }
                    };
                    let ExecReadResponse {
                        chunks,
                        next_seq,
                        exited,
                        exit_code,
                        closed,
                        failure,
                        sandbox_denied,
                    } = response;
                    for chunk in chunks.into_iter().filter(|chunk| chunk.seq > last_seq) {
                        let bytes = chunk.chunk.into_inner();
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        drop(guard);
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    last_seq = last_seq.max(next_seq.saturating_sub(1));
                    if let Some(message) = failure {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    if sandbox_denied || exited {
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= sandbox_denied;
                        let _ = state_tx.send_replace(if exited {
                            state.exited(exit_code)
                        } else {
                            state
                        });
                    }
                    if closed {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    continue;
                }

                let Some(event) = event else {
                    continue;
                };
                match event {
                    ExecProcessEvent::Output(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        last_seq = chunk.seq;
                        let bytes = chunk.chunk.into_inner();
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&bytes);
                        drop(guard);
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    ExecProcessEvent::Exited {
                        seq,
                        exit_code,
                        sandbox_denied,
                    } => {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= sandbox_denied.unwrap_or(false);
                        let _ = state_tx.send_replace(state.exited(Some(exit_code)));
                    }
                    ExecProcessEvent::Closed { seq } => {
                        if seq <= last_seq {
                            continue;
                        }
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    ExecProcessEvent::Failed(message) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
        })
    }

    fn spawn_local_output_task(
        mut receiver: tokio::sync::broadcast::Receiver<Vec<u8>>,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        completion_output_buffer: Option<Arc<Mutex<HeadTailBuffer>>>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            ..
        } = output_handles;
        tokio::spawn(async move {
            let _output_task_guard = OutputTaskGuard {
                output_closed: Arc::clone(&output_closed),
                output_closed_notify: Arc::clone(&output_closed_notify),
            };
            loop {
                match receiver.recv().await {
                    Ok(chunk) => {
                        if let Some(completion_output_buffer) = completion_output_buffer.as_ref() {
                            completion_output_buffer.lock().await.push_chunk(&chunk);
                        }
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(&chunk);
                        drop(guard);
                        let _ = output_tx.send(chunk);
                        output_notify.notify_waiters();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        break;
                    }
                };
            }
        })
    }

    pub(crate) fn signal_exit(&self, exit_code: Option<i32>) {
        let state = self.state_rx.borrow().clone();
        let _ = self.state_tx.send_replace(state.exited(exit_code));
        self.output.cancellation_token.cancel();
    }
}

impl Drop for UnifiedExecProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

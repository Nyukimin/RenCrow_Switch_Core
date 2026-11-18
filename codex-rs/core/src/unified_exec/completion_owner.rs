// Modified by RenCrow Switch Core, 2026-11-18: own unified exec startup and terminal completion.
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio::time::Instant;

use super::SharedPluginMetricsSidecar;
use super::UnifiedExecContext;
use super::async_watcher::emit_exec_end_for_unified_exec;
use super::async_watcher::emit_failed_exec_end_for_unified_exec;
use super::async_watcher::start_streaming_output;
use super::head_tail_buffer::HeadTailBuffer;
use super::process::ExecCancellationDisposition;
use super::process::StartupProcessOwner;
use super::process::StartupProcessResolution;
use super::process::StartupProcessResources;
use super::process::UnifiedExecProcess;
use super::take_plugin_metrics_sidecar;
use crate::plugins::metrics::finish_and_track_measurements;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::network_approval::DeferredNetworkApproval;
use codex_core_plugins::PluginCommandAttribution;
use codex_core_plugins::PluginMetricsSidecar;
use codex_utils_path_uri::PathUri;

pub(crate) struct UnifiedExecCompletionOwner {
    tx: mpsc::UnboundedSender<CompletionOwnerMessage>,
    context: Arc<CompletionOwnerContext>,
}

#[derive(Debug)]
pub(crate) enum CompletionResult {
    Success {
        fallback_output: String,
        exit_code: i32,
        duration: Duration,
        timed_out: bool,
        task_evidence: Option<crate::task_check::TaskCheckExecution>,
        output_drained: bool,
    },
    Failure {
        fallback_output: String,
        message: String,
        duration: Duration,
        task_evidence: Option<crate::task_check::TaskCheckExecution>,
        output_drained: bool,
    },
}

enum CompletionOwnerMessage {
    Begin {
        process: Arc<UnifiedExecProcess>,
        started_at: tokio::time::Instant,
        metrics_sidecar: Option<SharedPluginMetricsSidecar>,
        response: oneshot::Sender<()>,
    },
    Complete {
        process: Arc<UnifiedExecProcess>,
        result: Box<CompletionResult>,
        response: oneshot::Sender<()>,
    },
    ResolveStartup {
        resources: StartupProcessResources,
        resolution: StartupProcessResolution,
    },
}

pub(crate) struct CompletionOwnerContext {
    pub(crate) context: Arc<UnifiedExecContext>,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: PathUri,
    pub(crate) process_id: i32,
    pub(crate) plugin_attribution: Option<PluginCommandAttribution>,
    pub(crate) transcript: Arc<Mutex<HeadTailBuffer>>,
    pub(crate) task_evidence: Option<crate::task_check::TaskCheckExecution>,
    pub(crate) network_cleanup: std::sync::Mutex<Option<NetworkCleanup>>,
}

impl std::fmt::Debug for UnifiedExecCompletionOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecCompletionOwner")
            .field("call_id", &self.context.context.call_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct NetworkCleanup {
    deferred: Option<DeferredNetworkApproval>,
    monitor: Option<JoinHandle<()>>,
}

impl UnifiedExecCompletionOwner {
    pub(crate) fn new(context: CompletionOwnerContext) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let context = Arc::new(context);
        tokio::spawn(run_completion_owner(rx, Arc::clone(&context)));
        Arc::new(Self { tx, context })
    }

    pub(crate) async fn begin(
        &self,
        process: Arc<UnifiedExecProcess>,
        started_at: tokio::time::Instant,
        metrics_sidecar: Option<SharedPluginMetricsSidecar>,
    ) -> Result<(), &'static str> {
        let (response, wait) = oneshot::channel();
        let message = CompletionOwnerMessage::Begin {
            process: Arc::clone(&process),
            started_at,
            metrics_sidecar,
            response,
        };
        if self.tx.send(message).is_err() {
            tracing::error!("unified exec completion owner was unavailable before Begin");
            self.reap_after_owner_failure(process);
            return Err("completion owner unavailable before Begin");
        }
        if wait.await.is_err() {
            tracing::error!("unified exec completion owner stopped during Begin handling");
            self.reap_after_owner_failure(process);
            return Err("completion owner stopped before Begin acknowledgement");
        }
        Ok(())
    }

    pub(crate) async fn complete(
        &self,
        process: Arc<UnifiedExecProcess>,
        result: CompletionResult,
    ) -> Result<(), &'static str> {
        let (response, wait) = oneshot::channel();
        let message = CompletionOwnerMessage::Complete {
            process: Arc::clone(&process),
            result: Box::new(result),
            response,
        };
        if self.tx.send(message).is_err() {
            tracing::error!(
                "unified exec completion owner was unavailable before terminal submission"
            );
            self.reap_after_owner_failure(process);
            return Err("completion owner unavailable before terminal submission");
        }
        if wait.await.is_err() {
            tracing::error!("unified exec completion owner stopped during terminal handling");
            self.reap_after_owner_failure(process);
            return Err("completion owner stopped before terminal acknowledgement");
        }
        Ok(())
    }

    fn reap_after_owner_failure(&self, process: Arc<UnifiedExecProcess>) {
        process.request_terminate_for_completion();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let context = Arc::clone(&self.context);
            runtime.spawn(async move {
                process.reap_after_completion_termination(None).await;
                if let Some(message) = finish_network_cleanup(&context).await {
                    process.fail_and_terminate(message);
                }
            });
        } else {
            tracing::error!(
                "unified exec owner failure occurred without a runtime for network cleanup"
            );
            process.terminate();
        }
    }

    pub(crate) fn set_network_cleanup(
        &self,
        deferred: Option<DeferredNetworkApproval>,
        monitor: Option<JoinHandle<()>>,
    ) {
        let mut current = self
            .context
            .network_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Some(NetworkCleanup { deferred, monitor });
    }
}

impl StartupProcessOwner for UnifiedExecCompletionOwner {
    fn resolve(
        &self,
        resources: StartupProcessResources,
        resolution: StartupProcessResolution,
    ) -> Result<(), StartupProcessResources> {
        let message = CompletionOwnerMessage::ResolveStartup {
            resources,
            resolution,
        };
        match self.tx.send(message) {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::error!(
                    "unified exec completion owner was unavailable during startup handoff"
                );
                let (resources, resolution) = match error.0 {
                    CompletionOwnerMessage::ResolveStartup {
                        resources,
                        resolution,
                    } => (resources, resolution),
                    CompletionOwnerMessage::Begin {
                        process,
                        started_at,
                        metrics_sidecar,
                        ..
                    } => (
                        StartupProcessResources {
                            process,
                            early_exit_rx: None,
                            started_at,
                            metrics_sidecar,
                        },
                        StartupProcessResolution::Abandoned,
                    ),
                    CompletionOwnerMessage::Complete { process, .. } => (
                        StartupProcessResources {
                            process,
                            early_exit_rx: None,
                            started_at: Instant::now(),
                            metrics_sidecar: None,
                        },
                        StartupProcessResolution::Abandoned,
                    ),
                };
                let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                    return Err(resources);
                };
                let context = Arc::clone(&self.context);
                runtime.spawn(async move {
                    if !resources.process.has_exited() {
                        resources.process.request_terminate_for_completion();
                    }
                    resources
                        .process
                        .reap_after_completion_termination(resources.early_exit_rx)
                        .await;
                    if matches!(resolution, StartupProcessResolution::Abandoned) {
                        let _ = finish_network_cleanup(&context).await;
                        if resources.process.failure_message().is_none()
                            && let Some(sidecar) = resources.metrics_sidecar.as_ref()
                            && let Some(metrics) = take_plugin_metrics_sidecar(sidecar)
                        {
                            let model_context = context.context.step_context.model_context();
                            finish_and_track_measurements(
                                Some(metrics),
                                resources.process.exit_code().unwrap_or(-1),
                                &context.context.session,
                                &context.context.step_context.turn,
                                &model_context,
                                &context.context.call_id,
                            )
                            .await;
                        }
                    }
                });
                Ok(())
            }
        }
    }
}

async fn run_completion_owner(
    mut rx: mpsc::UnboundedReceiver<CompletionOwnerMessage>,
    context: Arc<CompletionOwnerContext>,
) {
    let mut process: Option<Arc<UnifiedExecProcess>> = None;
    let mut began = false;
    let mut terminal = false;
    let mut streaming_started = false;
    let mut started_at = None;
    let mut metrics_sidecar: Option<SharedPluginMetricsSidecar> = None;
    while let Some(message) = rx.recv().await {
        match message {
            CompletionOwnerMessage::Begin {
                process: current,
                started_at: command_started_at,
                metrics_sidecar: command_metrics_sidecar,
                response,
            } => {
                if !terminal {
                    process = Some(Arc::clone(&current));
                    started_at = Some(command_started_at);
                    metrics_sidecar = command_metrics_sidecar;
                    ensure_completion_begin(&current, &context, &mut began, &mut streaming_started)
                        .await;
                }
                let _ = response.send(());
            }
            CompletionOwnerMessage::Complete {
                process: current,
                result,
                response,
            } => {
                if !terminal {
                    let result = *result;
                    ensure_completion_begin(&current, &context, &mut began, &mut streaming_started)
                        .await;
                    let output_drained = match &result {
                        CompletionResult::Success { output_drained, .. }
                        | CompletionResult::Failure { output_drained, .. } => *output_drained,
                    };
                    if current.has_exited() && !output_drained {
                        current.output_drained_notify().notified().await;
                    }
                    if let Some(message) = finish_network_cleanup(&context).await {
                        current.fail_and_terminate(message);
                    }
                    let result = match (current.failure_message(), result) {
                        (
                            Some(message),
                            CompletionResult::Success {
                                fallback_output,
                                duration,
                                task_evidence,
                                output_drained,
                                ..
                            },
                        ) => CompletionResult::Failure {
                            fallback_output,
                            message,
                            duration,
                            task_evidence,
                            output_drained,
                        },
                        (_, result) => result,
                    };
                    match &result {
                        CompletionResult::Success { exit_code, .. } => {
                            let metrics = take_owner_metrics_sidecar(&mut metrics_sidecar);
                            let model_context = context.context.step_context.model_context();
                            finish_and_track_measurements(
                                metrics,
                                *exit_code,
                                &context.context.session,
                                &context.context.step_context.turn,
                                &model_context,
                                &context.context.call_id,
                            )
                            .await;
                        }
                        CompletionResult::Failure { .. } => {
                            drop(take_owner_metrics_sidecar(&mut metrics_sidecar));
                        }
                    }
                    emit_owned_completion(&current, &context, result).await;
                    terminal = true;
                    process = None;
                }
                let _ = response.send(());
            }
            CompletionOwnerMessage::ResolveStartup {
                resources,
                resolution,
            } => {
                metrics_sidecar = resources.metrics_sidecar.clone();
                if terminal {
                    drop(take_owner_metrics_sidecar(&mut metrics_sidecar));
                    resources.process.request_terminate_for_completion();
                    resources
                        .process
                        .reap_after_completion_termination(resources.early_exit_rx)
                        .await;
                    continue;
                }
                let has_explicit_cancel = resources
                    .process
                    .cancellation_disposition(
                        &context.context.call_id,
                        &context.context.step_context.turn.sub_id,
                    )
                    .is_some();
                let was_abandoned = matches!(&resolution, StartupProcessResolution::Abandoned);
                match resolution {
                    StartupProcessResolution::Discarded if !has_explicit_cancel => {
                        resources.process.terminate();
                        drop(take_owner_metrics_sidecar(&mut metrics_sidecar));
                        process = None;
                        began = false;
                        streaming_started = false;
                        terminal = false;
                    }
                    StartupProcessResolution::Discarded | StartupProcessResolution::Abandoned => {
                        let command_started_at = resources.started_at;
                        started_at = Some(command_started_at);
                        let current = resources.process;
                        ensure_completion_begin(
                            &current,
                            &context,
                            &mut began,
                            &mut streaming_started,
                        )
                        .await;
                        if !current.has_exited() {
                            current.request_terminate_for_completion();
                        }
                        current
                            .reap_after_completion_termination(resources.early_exit_rx)
                            .await;
                        current.output_drained_notify().notified().await;
                        if let Some(message) = finish_network_cleanup(&context).await {
                            current.fail_and_terminate(message);
                        }
                        let duration = command_started_at.elapsed();
                        let cancellation_disposition = current
                            .cancellation_disposition(
                                &context.context.call_id,
                                &context.context.step_context.turn.sub_id,
                            )
                            .or_else(|| {
                                was_abandoned.then_some(ExecCancellationDisposition::Interrupted)
                            });
                        let result = if let Some(message) = current.failure_message() {
                            drop(take_owner_metrics_sidecar(&mut metrics_sidecar));
                            CompletionResult::Failure {
                                fallback_output: String::new(),
                                message,
                                duration,
                                task_evidence: context.task_evidence.clone(),
                                output_drained: true,
                            }
                        } else {
                            let metrics_sidecar = take_owner_metrics_sidecar(&mut metrics_sidecar);
                            let model_context = context.context.step_context.model_context();
                            finish_and_track_measurements(
                                metrics_sidecar,
                                current.exit_code().unwrap_or(-1),
                                &context.context.session,
                                &context.context.step_context.turn,
                                &model_context,
                                &context.context.call_id,
                            )
                            .await;
                            CompletionResult::Success {
                                fallback_output: String::new(),
                                exit_code: current.exit_code().unwrap_or(-1),
                                duration,
                                timed_out: current.timed_out(),
                                task_evidence: context.task_evidence.clone(),
                                output_drained: true,
                            }
                        };
                        emit_owned_completion_with_disposition(
                            &current,
                            &context,
                            result,
                            cancellation_disposition,
                        )
                        .await;
                        terminal = true;
                        process = None;
                    }
                }
            }
        }
    }
    if let Some(process) = process
        && !terminal
    {
        ensure_completion_begin(&process, &context, &mut began, &mut streaming_started).await;
        if !process.has_exited() {
            process.request_terminate_for_completion();
        }
        process.reap_after_completion_termination(None).await;
        process.output_drained_notify().notified().await;
        if let Some(message) = finish_network_cleanup(&context).await {
            process.fail_and_terminate(message);
        }
        let duration = started_at.unwrap_or_else(Instant::now).elapsed();
        let cancellation_disposition = process
            .cancellation_disposition(
                &context.context.call_id,
                &context.context.step_context.turn.sub_id,
            )
            .or(Some(ExecCancellationDisposition::Interrupted));
        let result = if let Some(message) = process.failure_message() {
            drop(take_owner_metrics_sidecar(&mut metrics_sidecar));
            CompletionResult::Failure {
                fallback_output: String::new(),
                message,
                duration,
                task_evidence: context.task_evidence.clone(),
                output_drained: true,
            }
        } else {
            let metrics_sidecar = take_owner_metrics_sidecar(&mut metrics_sidecar);
            let model_context = context.context.step_context.model_context();
            finish_and_track_measurements(
                metrics_sidecar,
                process.exit_code().unwrap_or(-1),
                &context.context.session,
                &context.context.step_context.turn,
                &model_context,
                &context.context.call_id,
            )
            .await;
            CompletionResult::Success {
                fallback_output: String::new(),
                exit_code: process.exit_code().unwrap_or(-1),
                duration,
                timed_out: process.timed_out(),
                task_evidence: context.task_evidence.clone(),
                output_drained: true,
            }
        };
        emit_owned_completion_with_disposition(
            &process,
            &context,
            result,
            cancellation_disposition,
        )
        .await;
    }
}

fn take_owner_metrics_sidecar(
    metrics_sidecar: &mut Option<SharedPluginMetricsSidecar>,
) -> Option<PluginMetricsSidecar> {
    metrics_sidecar
        .take()
        .as_ref()
        .and_then(take_plugin_metrics_sidecar)
}

async fn finish_network_cleanup(context: &CompletionOwnerContext) -> Option<String> {
    let cleanup = context
        .network_cleanup
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()?;
    if let Some(monitor) = cleanup.monitor
        && let Err(error) = monitor.await
    {
        tracing::error!(%error, "unified exec network-denial monitor failed");
    }
    super::process_manager::finish_deferred_network_approval_for_session(
        Some(&context.context.session),
        cleanup.deferred,
    )
    .await
    .err()
}

async fn ensure_completion_begin(
    process: &Arc<UnifiedExecProcess>,
    context: &CompletionOwnerContext,
    began: &mut bool,
    streaming_started: &mut bool,
) {
    if !*began {
        if let Some(evidence) = context.task_evidence.as_ref() {
            context
                .context
                .session
                .begin_task_check_with_obligation(
                    context.context.call_id.clone(),
                    evidence.request.clone(),
                    evidence.workspace_root.clone(),
                    evidence.before.clone(),
                    evidence.obligation.clone(),
                )
                .await;
        }
        let model_context = context.context.step_context.model_context();
        let mut event_ctx = ToolEventCtx::new(
            context.context.session.as_ref(),
            context.context.step_context.turn.as_ref(),
            &context.context.step_context.settings.model_info,
            &context.context.call_id,
            /*turn_diff_tracker*/ None,
        );
        event_ctx.model_context = Some(&model_context);
        let emitter = ToolEmitter::unified_exec(
            &context.command,
            context.cwd.clone(),
            codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
            Some(context.process_id.to_string()),
            context.plugin_attribution.clone(),
            context.task_evidence.clone(),
            process.cancellation_disposition(
                &context.context.call_id,
                &context.context.step_context.turn.sub_id,
            ),
        );
        emitter.emit(event_ctx, ToolEventStage::Begin).await;
        *began = true;
    }
    if !*streaming_started {
        start_streaming_output(process, &context.context, Arc::clone(&context.transcript));
        *streaming_started = true;
    }
}

async fn emit_owned_completion(
    process: &Arc<UnifiedExecProcess>,
    context: &CompletionOwnerContext,
    result: CompletionResult,
) {
    let cancellation_disposition = process.cancellation_disposition(
        &context.context.call_id,
        &context.context.step_context.turn.sub_id,
    );
    emit_owned_completion_with_disposition(process, context, result, cancellation_disposition)
        .await;
}

async fn emit_owned_completion_with_disposition(
    process: &Arc<UnifiedExecProcess>,
    context: &CompletionOwnerContext,
    result: CompletionResult,
    cancellation_disposition: Option<ExecCancellationDisposition>,
) {
    let (transcript, authoritative_fallback) = match process.snapshot_completion_output().await {
        Some(bytes) => (
            Arc::new(Mutex::new(HeadTailBuffer::default())),
            Some(String::from_utf8_lossy(&bytes).into_owned()),
        ),
        None => (Arc::clone(&context.transcript), None),
    };
    match result {
        CompletionResult::Success {
            fallback_output,
            exit_code,
            duration,
            timed_out,
            task_evidence,
            ..
        } => {
            emit_exec_end_for_unified_exec(
                Arc::clone(&context.context.session),
                Arc::clone(&context.context.step_context.turn),
                Arc::clone(&context.context.step_context.settings.model_info),
                context.context.call_id.clone(),
                context.command.clone(),
                context.cwd.clone(),
                Some(context.process_id.to_string()),
                context.plugin_attribution.clone(),
                Arc::clone(&transcript),
                authoritative_fallback.unwrap_or(fallback_output),
                exit_code,
                duration,
                timed_out,
                task_evidence,
                cancellation_disposition,
            )
            .await;
        }
        CompletionResult::Failure {
            fallback_output,
            message,
            duration,
            task_evidence,
            ..
        } => {
            emit_failed_exec_end_for_unified_exec(
                Arc::clone(&context.context.session),
                Arc::clone(&context.context.step_context.turn),
                Arc::clone(&context.context.step_context.settings.model_info),
                context.context.call_id.clone(),
                context.command.clone(),
                context.cwd.clone(),
                Some(context.process_id.to_string()),
                context.plugin_attribution.clone(),
                Arc::clone(&transcript),
                authoritative_fallback.unwrap_or(fallback_output),
                message,
                duration,
                task_evidence,
                cancellation_disposition,
            )
            .await;
        }
    }
}

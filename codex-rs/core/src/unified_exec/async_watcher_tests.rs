use std::sync::Arc;

use super::Buffer;
use super::Emitter;
use super::TRAILING_OUTPUT_GRACE;
use super::spawn_exit_watcher;
use super::start_streaming_output;
use super::utf8_boundary;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::CompletionOwnerContext;
use crate::unified_exec::StartupProcessGuard;
use crate::unified_exec::StartupProcessOwner;
use crate::unified_exec::UnifiedExecCompletionOwner;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::NoopSpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;

use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::Instant;

async fn process_for_completion_test(
    owner: Option<Arc<dyn StartupProcessOwner>>,
) -> anyhow::Result<(
    StartupProcessGuard,
    tokio::sync::broadcast::Sender<Vec<u8>>,
    tokio::sync::oneshot::Sender<i32>,
)> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        SandboxType::None,
        Box::new(NoopSpawnLifecycle),
        owner,
        None,
    )
    .await?;
    Ok((process, stdout_tx, exit_tx))
}

async fn completed_command_event(
    rx_event: &async_channel::Receiver<Event>,
) -> anyhow::Result<codex_protocol::items::CommandExecutionItem> {
    loop {
        let event = rx_event.recv().await?;
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::CommandExecution(item) = completed.item
        {
            return Ok(item);
        }
    }
}

fn assert_no_duplicate_command_execution_completion(
    rx_event: &async_channel::Receiver<Event>,
    call_id: &str,
) {
    let mut remaining_events = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        remaining_events.push(event);
    }
    let has_duplicate_completion = remaining_events.iter().any(|event| {
        let EventMsg::ItemCompleted(completed) = &event.msg else {
            return false;
        };
        let TurnItem::CommandExecution(item) = &completed.item else {
            return false;
        };
        item.id == call_id
    });
    assert!(
        !has_duplicate_completion,
        "received another CommandExecution completion for {call_id}; remaining events: {remaining_events:?}"
    );
}

fn completion_owner_for_test(
    context: &UnifiedExecContext,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
) -> Arc<UnifiedExecCompletionOwner> {
    let cwd = context
        .step_context
        .environments
        .primary()
        .expect("turn environment")
        .cwd()
        .clone();
    UnifiedExecCompletionOwner::new(CompletionOwnerContext {
        context: Arc::new(UnifiedExecContext::new(
            Arc::clone(&context.session),
            Arc::clone(&context.step_context),
            context.cancellation_token.clone(),
            context.call_id.clone(),
        )),
        command: vec!["test-command".to_string()],
        cwd,
        process_id: 456,
        plugin_attribution: None,
        transcript,
        task_evidence: None,
        network_cleanup: std::sync::Mutex::new(None),
    })
}

struct StreamingOutputHarness {
    process: Arc<UnifiedExecProcess>,
    stdout_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_tx: tokio::sync::oneshot::Sender<i32>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    context: UnifiedExecContext,
    rx_event: async_channel::Receiver<Event>,
}

async fn streaming_output_harness() -> anyhow::Result<StreamingOutputHarness> {
    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        SandboxType::None,
        Box::new(NoopSpawnLifecycle),
        None,
        None,
    )
    .await?
    .handoff();
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "streaming-output-test".to_string(),
    );
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    start_streaming_output(&process, &context, Arc::clone(&transcript));

    Ok(StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        context,
        rx_event,
    })
}

#[tokio::test]
async fn startup_guard_drop_transfers_child_to_completion_owner() -> anyhow::Result<()> {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "startup-owner-drop".to_string(),
    );
    let owner = completion_owner_for_test(
        &context,
        Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
    );
    let startup_owner: Arc<dyn StartupProcessOwner> = owner.clone();
    let (process_guard, stdout_tx, exit_tx) =
        process_for_completion_test(Some(startup_owner)).await?;

    let output_notify = Arc::clone(&process_guard.process().output_handles().output_notify);
    let output_ready = output_notify.notified();
    tokio::pin!(output_ready);
    output_ready.as_mut().enable();
    stdout_tx.send(b"prefix before Begin".to_vec())?;
    tokio::time::timeout(Duration::from_secs(1), &mut output_ready).await?;
    assert_eq!(
        process_guard
            .process()
            .snapshot_completion_output()
            .await
            .as_deref(),
        Some(b"prefix before Begin".as_slice()),
    );

    drop(process_guard);
    exit_tx.send(7).expect("signal child exit");
    drop(stdout_tx);

    let item = completed_command_event(&rx_event).await?;
    assert_eq!(item.status, CommandExecutionStatus::Failed);
    assert_eq!(item.exit_code, Some(7));
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("prefix before Begin"),
    );
    assert_eq!(item.id, context.call_id);
    assert_no_duplicate_command_execution_completion(&rx_event, &context.call_id);

    Ok(())
}

#[tokio::test]
async fn startup_guard_drop_with_closed_exit_channel_finishes_terminal_once() -> anyhow::Result<()>
{
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "startup-owner-closed-exit".to_string(),
    );
    let owner = completion_owner_for_test(
        &context,
        Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
    );
    let startup_owner: Arc<dyn StartupProcessOwner> = owner;

    let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: None,
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let mut startup = Box::pin(UnifiedExecProcess::from_spawned(
        spawned,
        SandboxType::None,
        Box::new(NoopSpawnLifecycle),
        Some(startup_owner),
        None,
    ));
    assert!(futures::poll!(startup.as_mut()).is_pending());

    drop(startup);
    drop(exit_tx);
    drop(stdout_tx);

    let item =
        tokio::time::timeout(Duration::from_secs(1), completed_command_event(&rx_event)).await??;
    assert_eq!(item.status, CommandExecutionStatus::Failed);
    assert_eq!(item.exit_code, Some(-1));
    assert_eq!(item.id, context.call_id);
    assert_no_duplicate_command_execution_completion(&rx_event, &context.call_id);

    Ok(())
}

#[tokio::test]
async fn completion_owner_channel_close_finishes_active_child_once() -> anyhow::Result<()> {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "completion-owner-close".to_string(),
    );
    let (process_guard, stdout_tx, exit_tx) = process_for_completion_test(None).await?;
    let process = process_guard.handoff();
    let owner = completion_owner_for_test(
        &context,
        Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default())),
    );
    owner
        .begin(Arc::clone(&process), Instant::now(), None)
        .await
        .map_err(anyhow::Error::msg)?;
    drop(owner);
    exit_tx.send(7).expect("signal child exit");
    drop(stdout_tx);

    let item = completed_command_event(&rx_event).await?;
    assert_eq!(item.status, CommandExecutionStatus::Failed);
    assert_eq!(item.exit_code, Some(7));
    assert_eq!(item.id, context.call_id);
    assert_no_duplicate_command_execution_completion(&rx_event, &context.call_id);

    Ok(())
}

#[tokio::test]
async fn owner_completion_does_not_wait_for_consumed_output_drain_notification()
-> anyhow::Result<()> {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        session,
        crate::session::step_context::StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        "completion-owner-drain".to_string(),
    );
    let (process_guard, stdout_tx, exit_tx) = process_for_completion_test(None).await?;
    let started_at = process_guard.started_at();
    let process = process_guard.handoff();
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let owner = completion_owner_for_test(&context, Arc::clone(&transcript));
    owner
        .begin(Arc::clone(&process), started_at, None)
        .await
        .map_err(anyhow::Error::msg)?;
    let cwd = context
        .step_context
        .environments
        .primary()
        .expect("turn environment")
        .cwd()
        .clone();
    spawn_exit_watcher(
        Arc::clone(&process),
        &context,
        vec!["test-command".to_string()],
        cwd,
        456,
        None,
        transcript,
        Instant::now(),
        None,
        None,
        None,
        Some(owner),
    );

    exit_tx.send(0).expect("signal child exit");
    drop(stdout_tx);
    let item = completed_command_event(&rx_event).await?;
    assert_eq!(item.status, CommandExecutionStatus::Completed);
    assert_eq!(item.exit_code, Some(0));
    assert_eq!(item.id, context.call_id);
    assert_no_duplicate_command_execution_completion(&rx_event, &context.call_id);

    Ok(())
}

#[tokio::test]
async fn streaming_output_preserves_multibyte_characters_across_chunks() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    stdout_tx
        .send(vec![0xa9])
        .expect("send UTF-8 continuation byte");
    drop(stdout_tx);
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;

    let event = rx_event.recv().await.expect("receive output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: "é".as_bytes().to_vec(),
        }
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        "é".as_bytes()
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn streaming_output_finishes_on_close_without_waiting_for_grace() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stdout_tx
            .send(b"LATE-OUTPUT-MARKER\xc3".to_vec())
            .expect("send late output");
    });

    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < TRAILING_OUTPUT_GRACE,
        "output close should finish before the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        b"LATE-OUTPUT-MARKER\xc3"
    );

    Ok(())
}

#[tokio::test]
async fn streaming_output_keeps_grace_as_fallback_without_close() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        rx_event,
        ..
    } = streaming_output_harness().await?;
    let output_drained = process.output_drained_notify();
    let drained = output_drained.notified();
    tokio::pin!(drained);

    tokio::time::pause();
    let exited_at = Instant::now();
    stdout_tx.send(vec![0xc3]).expect("send UTF-8 lead byte");
    exit_tx.send(0).expect("send exit");
    (&mut drained).await;
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();

    assert!(
        elapsed >= TRAILING_OUTPUT_GRACE
            && elapsed <= TRAILING_OUTPUT_GRACE + Duration::from_millis(10),
        "missing output close should use the grace fallback: {elapsed:?}"
    );
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        vec![0xc3]
    );
    let event = rx_event.try_recv().expect("receive final output delta");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected ExecCommandOutputDelta");
    };
    assert_eq!(
        delta,
        ExecCommandOutputDeltaEvent {
            call_id: "streaming-output-test".to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: vec![0xc3],
        }
    );
    assert!(rx_event.try_recv().is_err());

    Ok(())
}

#[tokio::test]
async fn exit_watcher_waits_for_late_network_denial_before_classifying_end() -> anyhow::Result<()> {
    let StreamingOutputHarness {
        process,
        stdout_tx,
        exit_tx,
        transcript,
        mut context,
        rx_event,
    } = streaming_output_harness().await?;

    tokio::time::pause();
    let process_for_late_denial = Arc::clone(&process);
    let (late_denial_armed_tx, late_denial_armed_rx) = tokio::sync::oneshot::channel();
    let network_denial_monitor = tokio::spawn(async move {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        late_denial_armed_tx.send(()).expect("arm late denial");
        sleep.await;
        process_for_late_denial.fail_and_terminate("LATE_DENIAL".to_string());
    });
    late_denial_armed_rx.await.expect("late denial armed");

    #[allow(deprecated)]
    let cwd = context.step_context.turn.cwd.clone().into();
    let step = Arc::get_mut(&mut context.step_context).expect("unshared test step");
    let model_info = Arc::make_mut(&mut Arc::make_mut(&mut step.settings).model_info);
    model_info.truncation_policy = codex_protocol::openai_models::TruncationPolicyConfig {
        mode: codex_protocol::openai_models::TruncationMode::Bytes,
        limit: 4,
    };
    spawn_exit_watcher(
        Arc::clone(&process),
        &context,
        vec!["proof".to_string()],
        cwd,
        /*process_id*/ 123,
        /*plugin_attribution*/ None,
        transcript,
        Instant::now(),
        Some(network_denial_monitor),
        /*plugin_metrics_sidecar*/ None,
        /*task_evidence*/ None,
        /*completion_owner*/ None,
    );

    let exited_at = Instant::now();
    exit_tx.send(0).expect("send exit");
    drop(stdout_tx);

    let event = rx_event.recv().await.expect("command end event");
    let elapsed = Instant::now().saturating_duration_since(exited_at);
    tokio::time::resume();
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution");
    };
    assert_eq!(
        (
            item.status,
            item.exit_code,
            item.aggregated_output.as_deref()
        ),
        (
            CommandExecutionStatus::Failed,
            Some(-1),
            Some("LATE_DENIAL")
        )
    );
    assert_eq!(
        item.formatted_output,
        Some(codex_utils_output_truncation::formatted_truncate_text(
            "LATE_DENIAL",
            codex_utils_output_truncation::TruncationPolicy::Bytes(4),
        ))
    );
    assert!(
        elapsed >= Duration::from_millis(10) && elapsed < TRAILING_OUTPUT_GRACE,
        "completion should wait for denial without falling back to the output grace: {elapsed:?}"
    );

    Ok(())
}

#[test]
fn utf8_boundary_preserves_complete_characters() {
    assert_eq!(utf8_boundary(b"hello"), 5);

    let bytes = "ééé".as_bytes();
    assert_eq!(utf8_boundary(&bytes[..3]), 2);

    let bytes = "😀".as_bytes();
    assert_eq!(utf8_boundary(bytes), bytes.len());
    for len in 1..bytes.len() {
        assert_eq!(utf8_boundary(&bytes[..len]), 0);
    }
    assert_eq!(utf8_boundary(&[0xf0, 0x9f, 0x98, 0x80, 0xc3]), 4);
}

#[test]
fn utf8_boundary_batches_malformed_output() {
    assert_eq!(utf8_boundary(&[0xff, b'a', b'b']), 3);
    assert_eq!(utf8_boundary(&[0xff, 0xc3]), 1);
    assert_eq!(utf8_boundary(&[0xff, 0xc3, 0xa9]), 3);
    assert_eq!(utf8_boundary(&[0xe0, 0x80]), 2);

    assert_eq!(utf8_boundary(b"a\xffbbb"), 5);
}

#[tokio::test]
async fn streaming_output_bounds_invalid_bytes_and_keeps_the_full_transcript() {
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let mut output = Buffer::<8> {
        pending: Vec::new(),
        transcript: Arc::clone(&transcript),
        emitter: Emitter {
            remaining_deltas: 2,
            session,
            turn,
            call_id: "bounded-output-test".to_string(),
        },
    };

    // The first frame splits 😀; the last allowed frame leaves é incomplete.
    let bytes = b"\xff\xff\xff\xff\xff\xff\xf0\x9f\x98\x80\xff\xff\xff\xc3\xa9";
    output.push(bytes.to_vec()).await;
    output.push(vec![0xfe, 0xfe]).await;
    output.finish().await;

    let mut chunks = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
            panic!("expected ExecCommandOutputDelta");
        };
        chunks.push(delta.chunk);
    }
    assert_eq!(
        chunks,
        vec![
            b"\xff\xff\xff\xff\xff\xff".to_vec(),
            b"\xf0\x9f\x98\x80\xff\xff\xff".to_vec(),
        ]
    );

    let mut expected_transcript = bytes.to_vec();
    expected_transcript.extend([0xfe, 0xfe]);
    assert_eq!(
        transcript.lock().await.to_bytes_with_omission_marker(),
        expected_transcript
    );
}

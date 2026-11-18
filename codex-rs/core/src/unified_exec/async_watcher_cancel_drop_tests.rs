use std::sync::Arc;

use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ExplicitExecCancelReceipt;
use crate::state::PendingApproval;
use crate::state::TurnState;
use crate::unified_exec::CompletionOwnerContext;
use crate::unified_exec::ExecCancellationDisposition;
use crate::unified_exec::StartupProcessOwner;
use crate::unified_exec::UnifiedExecCompletionOwner;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::SpawnLifecycle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxType;

#[derive(Debug)]
struct ReceiptSpawnLifecycle {
    receipt: Arc<ExplicitExecCancelReceipt>,
}

impl SpawnLifecycle for ReceiptSpawnLifecycle {
    fn cancellation_disposition(
        &self,
        call_id: &str,
        turn_id: &str,
    ) -> Option<ExecCancellationDisposition> {
        self.receipt
            .was_cancelled_for_command(call_id, turn_id)
            .then_some(ExecCancellationDisposition::ExplicitUserCancellation)
    }
}

async fn wait_for_event(
    rx_event: &async_channel::Receiver<Event>,
    matches: impl Fn(&EventMsg) -> bool,
) -> anyhow::Result<()> {
    loop {
        let event = rx_event.recv().await?;
        if matches(&event.msg) {
            return Ok(());
        }
    }
}

async fn wait_for_command_completion(
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

#[tokio::test]
async fn dropping_from_spawned_during_grace_preserves_exact_approval_cancel() -> anyhow::Result<()>
{
    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let call_id = "cancel-during-startup-grace".to_string();
    let context = UnifiedExecContext::new(
        session,
        StepContext::for_test(turn),
        tokio_util::sync::CancellationToken::new(),
        call_id,
    );
    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let cwd = context
        .step_context
        .environments
        .primary()
        .expect("turn environment")
        .cwd()
        .clone();
    let owner = UnifiedExecCompletionOwner::new(CompletionOwnerContext {
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
    });
    let startup_owner: Arc<dyn StartupProcessOwner> = owner.clone();

    let turn_id = context.step_context.turn.sub_id.clone();
    let receipt = Arc::new(ExplicitExecCancelReceipt::new(
        context.call_id.clone(),
        turn_id.clone(),
    ));
    let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<ReviewDecision>();
    let mut turn_state = TurnState::default();
    turn_state.insert_pending_approval(
        "approval-during-startup-grace".to_string(),
        PendingApproval {
            tx: approval_tx,
            call_id: context.call_id.clone(),
            turn_id: turn_id.clone(),
            explicit_exec_cancel_receipt: Some(Arc::clone(&receipt)),
        },
    );

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
        Box::new(ReceiptSpawnLifecycle {
            receipt: Arc::clone(&receipt),
        }),
        Some(startup_owner),
        None,
    ));

    assert!(futures::poll!(startup.as_mut()).is_pending());
    assert_eq!(
        turn_state.record_explicit_exec_cancel(
            "approval-during-startup-grace",
            Some(&turn_id),
            &turn_id,
        ),
        Some(turn_id.clone())
    );
    turn_state.clear_pending_waiters();
    assert!(receipt.was_cancelled_for_command(&context.call_id, &turn_id));

    drop(startup);
    wait_for_event(&rx_event, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(
                    &started.item,
                    TurnItem::CommandExecution(item) if item.id == context.call_id
                )
        )
    })
    .await?;

    let output_prefix = b"approval-cancelled-during-startup".to_vec();
    stdout_tx
        .send(output_prefix.clone())
        .expect("startup process output receiver remains open");
    wait_for_event(&rx_event, |event| {
        matches!(
            event,
            EventMsg::ExecCommandOutputDelta(delta) if delta.call_id == context.call_id
        )
    })
    .await?;

    let exit_code = 23;
    exit_tx.send(exit_code).expect("signal startup child exit");
    drop(stdout_tx);

    let item = wait_for_command_completion(&rx_event).await?;
    assert_eq!(item.id, context.call_id);
    assert_eq!(item.status, CommandExecutionStatus::Declined);
    assert_eq!(item.exit_code, Some(exit_code));
    assert_eq!(
        item.stdout.as_deref(),
        Some("approval-cancelled-during-startup")
    );
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("approval-cancelled-during-startup")
    );

    tokio::task::yield_now().await;
    let mut matching_completions = 1;
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::CommandExecution(item) = completed.item
            && item.id == context.call_id
        {
            matching_completions += 1;
        }
    }
    assert_eq!(
        matching_completions, 1,
        "expected exactly one terminal completion"
    );

    drop(approval_rx);
    Ok(())
}

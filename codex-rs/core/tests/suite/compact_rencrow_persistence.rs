// Modified by RenCrow Switch Core, 2026-09-24: Level 5 persistence and replay coverage.
//! Uncertain checkpoint persistence requires a restart, and replay adopts only committed
//! checkpoints (COMPACTION_SPEC.md Part 2 §7, §58, §73 G04, §74).

use super::compact::non_openai_model_provider;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_thread_store::AppendThreadItemsParams;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::DeleteThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::PersistContext;
use codex_thread_store::ReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::StoredModelContext;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredThreadHistory;
use codex_thread_store::ThreadPage;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::ThreadStoreFuture;
use codex_thread_store::UpdateThreadMetadataParams;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const SUMMARY_REQUEST: &str = "This is a read-only compaction summary request.";
const SUMMARY_TEXT: &str = "PERSISTED-SUMMARY: the first task produced its result.";
const WORK_RESULT: &str = "FIRST-TASK-RESULT";

/// How the compaction commit batch (the append that contains the commit marker) fails.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Persist every row before the commit marker, then fail: an interrupted append.
    InterruptedAppend,
    /// Persist the whole batch, then fail the flush that follows it.
    UncertainFlush,
    /// Block the commit append until the turn is cancelled.
    HangingAppend,
}

/// An in-memory store that injects one fault into the compaction commit.
struct FaultStore {
    inner: InMemoryThreadStore,
    fault: Fault,
    committed_batch: AtomicBool,
    append_started: mpsc::UnboundedSender<()>,
}

macro_rules! delegate_store_methods {
    ($(fn $name:ident($param:ident: $params:ty) -> $result:ty;)*) => {
        $(fn $name(&self, $param: $params) -> ThreadStoreFuture<'_, $result> {
            ThreadStore::$name(&self.inner, $param)
        })*
    };
}

impl ThreadStore for FaultStore {
    fn as_any(&self) -> &dyn Any {
        self
    }

    delegate_store_methods! {
        fn create_thread(params: CreateThreadParams) -> ();
        fn resume_thread(params: ResumeThreadParams) -> ();
        fn discard_thread(thread_id: ThreadId) -> ();
        fn load_history(params: LoadThreadHistoryParams) -> StoredThreadHistory;
        fn load_latest_model_context(params: LoadThreadHistoryParams) -> StoredModelContext;
        fn read_thread(params: ReadThreadParams) -> StoredThread;
        fn read_thread_by_rollout_path(params: ReadThreadByRolloutPathParams) -> StoredThread;
        fn list_threads(params: ListThreadsParams) -> ThreadPage;
        fn archive_thread(params: ArchiveThreadParams) -> ();
        fn unarchive_thread(params: ArchiveThreadParams) -> StoredThread;
        fn delete_thread(params: DeleteThreadParams) -> ();
        fn shutdown_thread(thread_id: ThreadId) -> ();
        fn update_thread_metadata(params: UpdateThreadMetadataParams) -> Option<StoredThread>;
        fn record_thread_metadata(params: UpdateThreadMetadataParams) -> ();
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        self.inner.persist_thread(thread_id, context)
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let Some(marker) = params
                .items
                .iter()
                .position(|item| matches!(item, RolloutItem::RenCrowCompactionCommit { .. }))
            else {
                return self.inner.append_items(params).await;
            };
            match self.fault {
                Fault::InterruptedAppend => {
                    let mut prefix = params;
                    prefix.items.truncate(marker);
                    self.inner.append_items(prefix).await?;
                    Err(interrupted("append interrupted before the commit marker"))
                }
                Fault::UncertainFlush => {
                    self.inner.append_items(params).await?;
                    self.committed_batch.store(true, Ordering::SeqCst);
                    Ok(())
                }
                Fault::HangingAppend => {
                    let _ = self.append_started.send(());
                    std::future::pending().await
                }
            }
        })
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if self.committed_batch.swap(false, Ordering::SeqCst) {
                return Err(interrupted("flush result unknown"));
            }
            self.inner.flush_thread(thread_id).await
        })
    }
}

fn interrupted(message: &str) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: message.into(),
    }
}

async fn mount_responses(server: &wiremock::MockServer) -> Arc<Mutex<Vec<Value>>> {
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let responses = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            let summary = body["input"].to_string().contains(SUMMARY_REQUEST);
            let reply = if summary { SUMMARY_TEXT } else { WORK_RESULT };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_assistant_message(&id, reply),
                    ev_completed("response-fixture"),
                ]))
        })
        .mount(server)
        .await;
    seen
}

async fn build(
    server: &wiremock::MockServer,
    fault: Fault,
) -> Result<(TestCodex, mpsc::UnboundedReceiver<()>)> {
    let (append_started, started) = mpsc::unbounded_channel();
    let store = Arc::new(FaultStore {
        inner: InMemoryThreadStore::default(),
        fault,
        committed_batch: AtomicBool::new(false),
        append_started,
    });
    let provider = non_openai_model_provider(server);
    let test = test_codex()
        .with_thread_store(store)
        .with_config(move |config| {
            config.model_provider = provider;
            config.rencrow_compaction = true;
        })
        .build(server)
        .await?;
    Ok((test, started))
}

/// Wait for the error event of a failed turn and return its message.
async fn next_error(codex: &CodexThread) -> String {
    let EventMsg::Error(error) =
        wait_for_event(codex, |event| matches!(event, EventMsg::Error(_))).await
    else {
        unreachable!();
    };
    error.message
}

/// Restart from the durable store, replaying only committed checkpoints.
async fn restart(test: &TestCodex) -> Result<Arc<CodexThread>> {
    test.codex.shutdown_and_wait().await?;
    let context = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: context.thread_id,
                history: Arc::new(context.items),
                rollout_path: None,
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    Ok(resumed.thread)
}

async fn run_turn(codex: &CodexThread, text: &str) -> Result<()> {
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

async fn stored_items(test: &TestCodex) -> Result<Vec<RolloutItem>> {
    Ok(test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?
        .items)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_interrupted_commit_requires_restart_and_replay_discards_it() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = mount_responses(&server).await;
    let (test, _) = build(&server, Fault::InterruptedAppend).await?;
    run_turn(&test.codex, "first task").await?;

    test.codex.submit(Op::Compact).await?;
    assert!(next_error(&test.codex).await.contains("restart required"));
    // Let the failed compaction turn finish so the next input starts a new turn.
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    // The prepared checkpoint was written without its commit marker.
    let stored = stored_items(&test).await?;
    assert!(
        stored
            .iter()
            .any(|item| matches!(item, RolloutItem::Compacted(_)))
    );
    assert!(
        !stored
            .iter()
            .any(|item| matches!(item, RolloutItem::RenCrowCompactionCommit { .. }))
    );
    // The live session no longer samples until it restarts.
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "blocked turn".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    assert!(next_error(&test.codex).await.contains("restart"));
    let sent = seen.lock().expect("requests lock").len();

    let resumed = restart(&test).await?;
    run_turn(&resumed, "after restart").await?;

    // Replay kept the last committed state: the original work, not the uncommitted summary.
    let requests = seen.lock().expect("requests lock");
    assert_eq!(requests.len(), sent + 1);
    let recovered = requests.last().expect("recovered turn")["input"].to_string();
    assert!(recovered.contains(WORK_RESULT));
    assert!(!recovered.contains(SUMMARY_TEXT));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_uncertain_flush_requires_restart_and_replay_adopts_the_committed_checkpoint()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = mount_responses(&server).await;
    let (test, _) = build(&server, Fault::UncertainFlush).await?;
    run_turn(&test.codex, "first task").await?;

    test.codex.submit(Op::Compact).await?;
    assert!(next_error(&test.codex).await.contains("restart required"));

    let resumed = restart(&test).await?;
    run_turn(&resumed, "after restart").await?;

    // The whole transaction, including its marker, is durable, so replay adopts it.
    let requests = seen.lock().expect("requests lock");
    let recovered = requests.last().expect("recovered turn")["input"].to_string();
    assert!(recovered.contains(SUMMARY_TEXT));
    assert!(!recovered.contains(WORK_RESULT));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_cancel_during_persistence_requires_restart_and_keeps_the_original_history()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = mount_responses(&server).await;
    let (test, mut append_started) = build(&server, Fault::HangingAppend).await?;
    run_turn(&test.codex, "first task").await?;

    test.codex.submit(Op::Compact).await?;
    append_started.recv().await.expect("commit append started");
    test.codex.submit(Op::Interrupt).await?;
    assert!(
        next_error(&test.codex)
            .await
            .contains("canceled during persistence")
    );

    let resumed = restart(&test).await?;
    run_turn(&resumed, "after restart").await?;
    let requests = seen.lock().expect("requests lock");
    let recovered = requests.last().expect("recovered turn")["input"].to_string();
    assert!(recovered.contains(WORK_RESULT));
    assert!(!recovered.contains(SUMMARY_TEXT));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_replay_of_corrupt_transaction_is_integrity_blocked_without_restart_request()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_responses(&server).await;
    let (test, _) = build(&server, Fault::UncertainFlush).await?;
    run_turn(&test.codex, "first task").await?;
    test.codex.submit(Op::Compact).await?;
    next_error(&test.codex).await;
    test.codex.shutdown_and_wait().await?;

    // Corrupt the durable commit marker; every replay of this data fails the same way.
    let mut items = stored_items(&test).await?;
    let marker = items
        .iter_mut()
        .find(|item| matches!(item, RolloutItem::RenCrowCompactionCommit { .. }))
        .expect("commit marker");
    *marker = RolloutItem::RenCrowCompactionCommit {
        checkpoint_hash: "not-a-hash".into(),
    };
    for _ in 0..2 {
        let error = test
            .thread_manager
            .resume_thread_with_history(
                test.config.clone(),
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: test.session_configured.thread_id,
                    history: Arc::new(items.clone()),
                    rollout_path: None,
                }),
                test.thread_manager.auth_manager(),
                /*parent_trace*/ None,
                ClientMcpExtensions::default(),
            )
            .await
            .err()
            .expect("corrupt replay must not resume")
            .to_string();
        assert!(error.contains("deterministic integrity conflict"));
        assert!(error.contains("restarting cannot repair it"));
        assert!(!error.contains("restart required"));
    }
    Ok(())
}

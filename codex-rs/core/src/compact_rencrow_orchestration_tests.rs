//! Integration regressions for the V2 original-loop connection.
//!
//! These cases target the pure `compact_rencrow_summary` boundary, not the model/session
//! transport. `ServerReasoningIncluded` is websocket metadata, so its drain dispatch boundary needs
//! a transport-level regression in addition to the pure policy cases below.

use crate::compact::SUMMARY_PREFIX;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ArchiveReference;
use codex_history::archive_reference::ArchiveTerminalStatus;
use codex_history::archive_reference::ObservationReference;
use codex_history::archive_reference::content_sha256;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_checkpoint_metadata::RenCrowCompactionMetadataV2;
use codex_history::compaction_pipeline::InstructionSelection;
use codex_history::compaction_pipeline::ProposedOperation;
use codex_history::compaction_pipeline::ProposedPlan;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_history::compaction_preprocess::InstructionPruning;
use codex_history::compaction_preprocess::collect_instruction_candidates;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_history::compaction_selection::validate_and_apply_selection;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceRef;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::BoundaryCandidate;
use codex_history::task_context::BoundaryMode;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionReferenceKind;
use codex_rollout::PreparedCompactionSourcePair;
use codex_rollout::PreparedCompactionSources;
use serde_json::json;
use std::borrow::Cow;

const THREAD_ID: &str = "thread-v2-orchestration";

fn message(role: &str, text: &str, id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn human(text: &str, id: &str) -> ResponseItemEnvelope {
    let mut item = message("user", text, id);
    item.metadata.get_or_insert_default().rencrow_input = Some(json!({
        "version": 1,
        "author": "human",
        "thread_id": THREAD_ID,
        "selected_text": text,
        "receipt_hash": format!("receipt-{id}"),
    }));
    item
}

fn checkpoint_summary(text: &str) -> ResponseItemEnvelope {
    let summary_body = format!("{SUMMARY_PREFIX}\n{text}");
    let mut summary = message("user", &summary_body, "old-summary");
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut summary.item
    else {
        unreachable!();
    };
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
        ..Default::default()
    });
    summary.metadata.get_or_insert_default().rencrow_compaction = Some(json!({
        "version": 2,
        "snapshot_hash": "a".repeat(64),
        "summary_hash": content_sha256(&summary_body),
        "semantic_summary_hash": content_sha256(&summary_body),
        "selection_mode": "no_candidates",
        "applied_refs": [],
        "results": [],
        "observations": [],
        "summary_covered_observations": [],
        "important_refs": [],
        "model": "orchestration-test-model",
        "responses": [{
            "stage": "summary",
            "response_id": "adopted-summary-response",
            "seconds": 0.1,
            "usage": null,
        }],
        "committed_transaction_hash": "b".repeat(64),
    }));
    summary
}

fn stored_task_state_fixture() -> serde_json::Value {
    let boundary = BoundaryCandidate {
        version: 1,
        mode: BoundaryMode::Task,
        thread_id: THREAD_ID.into(),
        root_task_id: "root".into(),
        closed_task_id: "task-a".into(),
        revision: 5,
        closed_task_revision: 2,
        goal: "complete A".into(),
        tasks: vec![TaskState {
            id: "task-a".into(),
            purpose: "A".into(),
            status: TaskStatus::Completed,
            depends_on: vec![],
        }],
        current_task_id: None,
        artifact_ref: "artifact:a".into(),
        verification_ref: "check:a".into(),
        decisions: vec![],
        constraints: vec![],
        completion_state: None,
    };
    let completed = [(
        "task-a".into(),
        AcceptedTaskReceipt {
            revision: 2,
            result: "A done".into(),
            artifact_ref: "artifact:a".into(),
            verification_ref: "check:a".into(),
        },
    )]
    .into_iter()
    .collect();
    let candidate_hash = content_sha256(&serde_json::to_string(&boundary).unwrap());
    let state = StoredTaskState {
        boundary,
        completed,
        parent_review_ref: "review:a".into(),
        previous_revision: None,
        previous_state_hash: None,
        candidate_hash,
        boundary_nonce: "nonce-a".into(),
    };
    state.validate(THREAD_ID).unwrap();
    serde_json::to_value(state).unwrap()
}

fn task_boundary_summary(text: &str) -> ResponseItemEnvelope {
    let mut summary = message("user", text, "task-boundary-json");
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut summary.item
    else {
        unreachable!();
    };
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
        ..Default::default()
    });
    let summary_text = item_text(&summary);
    let checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
        &summary_text,
        stored_task_state_fixture(),
        TaskBoundarySourceFence {
            history_hash: "a".repeat(64),
            removed_refs: vec![TaskBoundarySourceRef {
                id: "work-a".into(),
                hash: "b".repeat(64),
            }],
        },
    );
    summary.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint).unwrap());
    summary
}

fn candidate_input(
    originals: &[ResponseItemEnvelope],
    completed_work: &[super::history::CompletedWorkProjection],
) -> CandidateInput {
    super::history::capture(
        originals,
        "snapshot-binding".into(),
        THREAD_ID,
        vec![],
        completed_work,
    )
    .unwrap()
}

fn application(input: &CandidateInput) -> InstructionSelectionApplication {
    InstructionSelectionApplication {
        pruning: prune_known_obsolete(input, &[]).unwrap(),
        plan_hash: None,
        results: vec![],
    }
}

fn item_text(envelope: &ResponseItemEnvelope) -> String {
    match &envelope.item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn model_items(envelopes: &[ResponseItemEnvelope]) -> Vec<&ResponseItem> {
    envelopes.iter().map(|envelope| &envelope.item).collect()
}

fn item_id(envelope: &ResponseItemEnvelope) -> Option<&str> {
    envelope
        .item
        .id()
        .map(codex_protocol::ResponseItemId::as_str)
}

#[test]
fn task_boundary_pairs_are_partitioned_before_normal_and_emergency_summaries() {
    let ordinary_output = "ordinary output ".repeat(100);
    let originals = vec![
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "task_context_boundary".into(),
            namespace: None,
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: "begin-call".into(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("begin-call".into()),
            name: Some("task_context_boundary".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("begun".into()),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".into(),
            namespace: None,
            arguments: "{\"cmd\":\"verify\"}".into(),
            encrypted_function_args: None,
            call_id: "verify-call".into(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("verify-call".into()),
            name: Some("exec_command".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("passed".into()),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".into(),
            namespace: None,
            arguments: "{\"cmd\":\"ordinary\"}".into(),
            encrypted_function_args: None,
            call_id: "ordinary-call".into(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("ordinary-call".into()),
            name: Some("exec_command".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text(ordinary_output),
            internal_chat_message_metadata_passthrough: None,
        }),
    ];
    fn source_pair<'a>(
        call_index: usize,
        call_id: &str,
        call_text: &'a str,
        output: &'a str,
    ) -> PreparedCompactionSourcePair<'a> {
        PreparedCompactionSourcePair {
            call_index,
            output_index: call_index + 1,
            reference: ObservationReference::new(THREAD_ID, call_id, content_sha256(output)),
            reference_kind: PreparedCompactionReferenceKind::Fresh,
            output_total_bytes: output.len(),
            tool_name: "exec_command",
            canonical_call_input: Some(call_text),
            canonical_output_text: Some(Cow::Borrowed(output)),
            terminal_reference: None,
        }
    }
    let mut sources = PreparedCompactionSources {
        pairs: vec![
            source_pair(0, "begin-call", "{}", "begun"),
            source_pair(2, "verify-call", "{\"cmd\":\"verify\"}", "passed"),
            source_pair(
                4,
                "ordinary-call",
                "{\"cmd\":\"ordinary\"}",
                "ordinary output",
            ),
        ],
        protected_indices: Vec::new(),
    };
    let protected_ids = ["begin-call".to_owned(), "verify-call".to_owned()]
        .into_iter()
        .collect();
    super::stages::partition_task_boundary_pairs(&mut sources, &protected_ids);

    assert_eq!(
        sources
            .pairs
            .iter()
            .map(|pair| pair.reference.call_id.as_str())
            .collect::<Vec<_>>(),
        ["ordinary-call"]
    );
    assert_eq!(sources.protected_indices, [0, 1, 2, 3]);
    let input = super::history::capture(
        &originals,
        "task-boundary-observation-test".into(),
        THREAD_ID,
        vec![],
        &super::history::verified_pair_projections(&sources),
    )
    .unwrap();
    let projections =
        super::observation::project_unhandled_observations(&originals, &sources, &[]).unwrap();
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0].2.coverage.reference.call_id, "ordinary-call");

    let native = super::native::filter_retained_instructions_with_task_context(
        &originals,
        &input,
        application(&input),
        None,
        &protected_ids,
    )
    .unwrap();
    let excerpts = projections
        .iter()
        .map(|(call, output, projection)| (*call, *output, projection.summary.clone()))
        .collect::<Vec<_>>();
    let normal_history = super::summary::build_summary_history_with_task_context(
        &originals, &input, &native, &excerpts, None, None, None,
    )
    .expect("normal summary accepts only ordinary observation excerpts");
    assert!(normal_history.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == "begin-call")
    }));
    assert!(normal_history.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == "verify-call")
    }));

    let markers =
        super::emergency::select_observation_markers(&originals, &sources, &projections, None)
            .unwrap();
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].1.coverage.reference.call_id, "ordinary-call");
    let emergency_text = super::emergency::emergency_summary_text(&originals, None)
        .unwrap()
        .0;
    let emergency_plan = super::emergency::EmergencyPlan {
        originals: &originals,
        input: &input,
        native: &native,
        unhandled_pairs: projections
            .iter()
            .map(|(call, output, _)| (*call, *output))
            .collect(),
        markers: markers
            .iter()
            .map(|(index, projection)| (*index, projection))
            .collect(),
        work_boundary: None,
        summary_text: emergency_text,
    };
    let emergency_candidate = emergency_plan.build(&[]).unwrap();
    emergency_plan.validate(&[], &emergency_candidate).unwrap();
    assert!(emergency_candidate.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == "begin-call")
    }));
    assert!(emergency_candidate.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == "verify-call")
    }));
}

#[tokio::test]
async fn task_boundary_projection_preserves_scope_and_receipt_through_compaction_and_commit() {
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::state::AutoCompactWindowIds;
    use crate::task_check::TaskCheckExecution;
    use crate::task_check::TaskCheckKind;
    use crate::task_check::TaskEvidenceRequest;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::task_context_boundary::TaskContextBoundaryHandler;
    use crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME;
    use crate::tools::registry::ToolExecutor;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::protocol::SessionSource;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let (mut session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn.config).rencrow_compaction = true;
    let turn = Arc::new(turn);
    let store = Arc::new(codex_thread_store::InMemoryThreadStore::default());
    let thread_store: Arc<dyn codex_thread_store::ThreadStore> = store.clone();
    let config = session.get_config().await;
    let live_thread = codex_thread_store::LiveThread::create(
        Arc::clone(&thread_store),
        codex_thread_store::CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.session_id(),
            thread_id: session.thread_id(),
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Cli,
            thread_source: None,
            originator: "test_originator".into(),
            base_instructions: codex_protocol::models::BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: Default::default(),
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: codex_thread_store::ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: codex_protocol::protocol::ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("create test thread store");
    session.services.thread_store = thread_store;
    session.services.live_thread = Some(live_thread);
    static ACTIVE_TURNS: codex_diagnostics::Gauge =
        codex_diagnostics::Gauge::new("task_boundary_compaction_test_active_turns");
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        task: Some(crate::state::RunningTask {
            done: Arc::new(tokio::sync::Notify::new()),
            kind: crate::state::TaskKind::Regular,
            task: Arc::new(crate::tasks::RegularTask::new()),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            handle: tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
                std::future::pending::<()>().await;
            })),
            turn_context: Arc::clone(&turn),
            _agent_execution_guard: None,
            _diagnostics_guard: ACTIVE_TURNS.track(),
            _timer: None,
        }),
        ..Default::default()
    });
    let session = Arc::new(session);

    let begin_call_id = "scope-begin";
    session
        .begin_task_scope(
            "root".into(),
            "task-a".into(),
            1,
            begin_call_id.into(),
            false,
        )
        .await
        .expect("begin task scope");
    for item in [
        ResponseItem::FunctionCall {
            id: None,
            name: TASK_CONTEXT_BOUNDARY_TOOL_NAME.into(),
            namespace: None,
            arguments: serde_json::json!({
                "action":"begin", "root_task_id":"root", "task_id":"task-a", "task_revision":1
            })
            .to_string(),
            encrypted_function_args: None,
            call_id: begin_call_id.into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(begin_call_id.into()),
            name: Some(TASK_CONTEXT_BOUNDARY_TOOL_NAME.into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("begun".into()),
            internal_chat_message_metadata_passthrough: None,
        },
    ] {
        session
            .record_annotated_conversation_items(
                &turn,
                turn.model_info().as_ref(),
                vec![ResponseItemEnvelope::new(item)],
            )
            .await;
    }

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "verified").unwrap();
    let request = TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision: 1,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let execution = TaskCheckExecution::new(request.clone(), workspace.path()).unwrap();
    let verify_call_id = "verify-call";
    session
        .begin_task_check(
            verify_call_id.into(),
            request,
            execution.workspace_root.clone(),
            execution.before.clone(),
        )
        .await;
    session
        .finish_task_check(verify_call_id, 0, true, execution.hash_artifacts().unwrap())
        .await
        .expect("host verification receipt");
    for item in [
        ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".into(),
            namespace: None,
            arguments: "{}".into(),
            encrypted_function_args: None,
            call_id: verify_call_id.into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(verify_call_id.into()),
            name: Some("exec_command".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("exit 0".into()),
            internal_chat_message_metadata_passthrough: None,
        },
    ] {
        session
            .record_annotated_conversation_items(
                &turn,
                turn.model_info().as_ref(),
                vec![ResponseItemEnvelope::new(item)],
            )
            .await;
    }

    let large_ordinary_output = "ordinary verified work ".repeat(1_000);
    for item in [
        ResponseItem::FunctionCall {
            id: None,
            name: "read_file".into(),
            namespace: None,
            arguments: "{\"cmd\":\"ordinary work\"}".into(),
            encrypted_function_args: None,
            call_id: "ordinary-work-call".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("ordinary-work-call".into()),
            name: Some("read_file".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text(large_ordinary_output),
            internal_chat_message_metadata_passthrough: None,
        },
    ] {
        session
            .record_annotated_conversation_items(
                &turn,
                turn.model_info().as_ref(),
                vec![ResponseItemEnvelope::new(item)],
            )
            .await;
    }

    let snapshot = session.clone_history().await;
    let originals = snapshot.annotated_items().to_vec();
    let source_thread = session.thread_id();
    let active_call_ids = std::collections::HashSet::new();
    let mut canonical = originals
        .iter()
        .cloned()
        .map(codex_history::RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    canonical.push(codex_history::RolloutItem::EventMsg(
        codex_protocol::protocol::EventMsg::ItemCompleted(
            codex_protocol::protocol::ItemCompletedEvent {
                thread_id: source_thread,
                turn_id: "task-boundary-test-turn".into(),
                item: codex_protocol::items::TurnItem::CommandExecution(
                    codex_protocol::items::CommandExecutionItem {
                        model_context: None,
                        id: "verify-call".into(),
                        plugin_id: None,
                        script_path: None,
                        process_id: Some("verify-process".into()),
                        command: vec!["test".into()],
                        cwd: serde_json::from_value(serde_json::json!("file:///tmp")).unwrap(),
                        parsed_cmd: Vec::new(),
                        source: codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
                        interaction_input: None,
                        status: codex_protocol::items::CommandExecutionStatus::Completed,
                        stdout: Some("passed".into()),
                        stderr: None,
                        aggregated_output: Some("passed".into()),
                        exit_code: Some(0),
                        duration: Some(std::time::Duration::from_millis(1)),
                        formatted_output: Some("passed".into()),
                    },
                ),
                started_at_ms: Some(1),
                completed_at_ms: 2,
            },
        ),
    ));
    let source_check = codex_rollout::prepare_compaction_sources(
        &originals,
        &canonical,
        &source_thread,
        &active_call_ids,
    )
    .expect("canonical source proof before task pair partition");
    let source_ids = source_check
        .pairs
        .iter()
        .map(|pair| pair.reference.call_id.as_str())
        .collect::<Vec<_>>();
    assert!(
        source_ids.contains(&"verify-call"),
        "verification source missing: {source_ids:?}"
    );
    assert!(
        source_ids.contains(&"ordinary-work-call"),
        "ordinary comparison source missing: {source_ids:?}"
    );
    let prepared = super::stages::prepare_v2(
        &session,
        &turn,
        &snapshot,
        &canonical,
        &source_thread,
        &active_call_ids,
        "task-boundary-compaction-history-hash",
        &session.thread_settings_snapshot().await,
        Vec::new(),
    )
    .await
    .unwrap_or_else(|error| panic!("actual shared V2 preparation: {}", error.into_error()));
    super::stages::validate_normal_history_for_test(&prepared)
        .expect("actual prepare_v2 output must form a valid Normal summary history");
    let replacement = super::stages::build_emergency_history_for_test(&prepared)
        .expect("actual prepare_v2 Emergency candidate and checkpoint");
    session
        .replace_compacted_history(
            replacement,
            None,
            None,
            crate::compact::CompactedHistoryMetadata {
                message: String::new(),
                window_number: 1,
                window_ids: AutoCompactWindowIds::new_initial(),
                compaction_response_id: None,
                compaction_model_hash: None,
                reviewer_compaction_hash: None,
            },
        )
        .await;
    let compacted = session.clone_history().await;
    let compacted_items = compacted.annotated_items();
    assert!(compacted_items.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == begin_call_id)
    }));
    assert!(compacted_items.iter().any(|item| {
        matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == verify_call_id)
    }));
    assert_eq!(
        session.active_task_scope().await.unwrap().start_history_cursor,
        compacted_items
            .iter()
            .position(|item| matches!(&item.item, ResponseItem::FunctionCall { call_id, .. } if call_id == begin_call_id))
            .unwrap()
    );

    let accept_call_id = "accept-call";
    let accept_args = serde_json::json!({
        "action":"accept",
        "goal":"Finish task A",
        "tasks":[{"id":"task-a", "purpose":"Verify task A", "status":"completed"}],
        "current_task_id":null,
        "result":"Task A verification passed"
    });
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: TASK_CONTEXT_BOUNDARY_TOOL_NAME.into(),
                namespace: None,
                arguments: accept_args.to_string(),
                encrypted_function_args: None,
                call_id: accept_call_id.into(),
                internal_chat_message_metadata_passthrough: None,
            })],
        )
        .await;
    TaskContextBoundaryHandler::new(false)
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: accept_call_id.into(),
            tool_name: codex_tools::ToolName::plain(TASK_CONTEXT_BOUNDARY_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: accept_args.to_string(),
            },
        })
        .await
        .expect("accept after actual compaction projection");
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(accept_call_id.into()),
                    name: Some(TASK_CONTEXT_BOUNDARY_TOOL_NAME.into()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text("prepared".into()),
                    internal_chat_message_metadata_passthrough: None,
                },
            )],
        )
        .await;
    let cancellation_token = tokio_util::sync::CancellationToken::new();
    assert!(!cancellation_token.is_cancelled());
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &cancellation_token)
            .await
            .expect("commit after compaction")
    );
}

#[test]
fn v2_summary_projection_uses_one_adopted_summary_and_only_later_ordinary_work() {
    let old_work = message("assistant", "covered before checkpoint", "old-work");
    let adopted = checkpoint_summary("prior verified state");
    let request = human("keep this request", "human-request");
    let later_work = message("assistant", "new work after checkpoint", "new-work");
    let originals = vec![old_work, adopted, request.clone(), later_work.clone()];
    let input = candidate_input(&originals, &[]);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();

    let projected =
        super::summary::build_summary_history(&originals, &input, &native, &[], Some(1), Some(1))
            .unwrap();

    let ids = projected.iter().filter_map(item_id).collect::<Vec<_>>();
    assert_eq!(ids.iter().filter(|id| **id == "old-summary").count(), 1);
    assert!(ids.contains(&"new-work"));
    assert!(ids.contains(&"human-request"));
    assert!(!ids.contains(&"old-work"));
    assert!(projected.iter().any(|item| item == &request));
    assert!(projected.iter().any(|item| item == &later_work));
}

#[test]
fn v2_first_compaction_shows_the_latest_legacy_summary_once() {
    let legacy_body = format!("{SUMMARY_PREFIX}\nlegacy verified state");
    let mut legacy = message("user", &legacy_body, "legacy-summary");
    legacy.metadata.get_or_insert_default().rencrow_compaction =
        Some(json!({"version": 1, "invalidated_instructions": []}));
    let request = human("keep this request", "human-request");
    let later_work = message("assistant", "new work after legacy checkpoint", "new-work");
    let originals = vec![legacy, request, later_work];
    assert_eq!(
        super::summary::find_adopted_v2_checkpoint(&originals, THREAD_ID),
        Ok(None)
    );
    let context = super::summary::semantic_context(&originals, /*adopted*/ None);
    assert_eq!(
        context,
        super::summary::SemanticContext {
            previous_summary: Some(0),
            task_context: None,
            work_boundary: Some(0),
            selection_boundary: None,
        }
    );
    let input = candidate_input(&originals, &[]);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();

    let projected = super::summary::build_summary_history(
        &originals,
        &input,
        &native,
        &[],
        context.previous_summary,
        context.work_boundary,
    )
    .unwrap();

    assert_eq!(
        projected.iter().filter_map(item_id).collect::<Vec<_>>(),
        vec!["legacy-summary", "human-request", "new-work"]
    );
    let replacement = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();
    assert!(
        !replacement
            .iter()
            .any(|item| item_id(item) == Some("legacy-summary"))
    );
}

#[test]
fn v2_summary_projection_replaces_both_verified_pair_slots_with_one_bounded_observation() {
    let old_work = message("assistant", "covered earlier", "old-work");
    let adopted = checkpoint_summary("prior facts");
    let request = human("check the file", "human-request");
    let call_text = r#"{"path":"large.txt"}"#;
    let output_text = format!("HEAD:{}:TAIL", "x".repeat(5_000));
    let call_item: ResponseItem = serde_json::from_value(json!({
        "type": "function_call",
        "id": "verified-call-item",
        "call_id": "verified-call",
        "name": "read_file",
        "arguments": call_text,
    }))
    .unwrap();
    let call = ResponseItemEnvelope::new(call_item);
    let output = ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server("verified-output-item".into())),
        call_id: Some("verified-call".into()),
        name: Some("read_file".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(output_text.clone()),
        internal_chat_message_metadata_passthrough: None,
    });
    let active_call = ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "active-call-item",
            "call_id": "active-call",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"still-running\"}",
        }))
        .unwrap(),
    );
    let opaque_media = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("opaque-media-item".into())),
        call_id: "media-call".into(),
        name: Some("image_tool".into()),
        output: FunctionCallOutputPayload::from_content_items(vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText {
                text: "media metadata".into(),
            },
            codex_protocol::models::FunctionCallOutputContentItem::InputImage {
                image: codex_protocol::models::ImageReference::Inline {
                    image_url: "data:image/png;base64,AQ==".into(),
                },
                detail: None,
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    });
    let later_work = message("assistant", "checked the file header", "new-work");
    let originals = vec![
        old_work,
        adopted,
        request.clone(),
        call,
        output,
        later_work.clone(),
        active_call.clone(),
        opaque_media.clone(),
    ];
    let completed = [super::history::CompletedWorkProjection {
        call_index: 3,
        output_index: 4,
        call_text: call_text.into(),
        output_text: output_text.clone(),
    }];
    let input = candidate_input(&originals, &completed);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();
    let reference =
        ObservationReference::new(THREAD_ID, "verified-call", content_sha256(&output_text));
    let observation =
        codex_history::project_observation(&reference, "read_file", call_text, &output_text)
            .unwrap()
            .summary;

    let projected = super::summary::build_summary_history(
        &originals,
        &input,
        &native,
        &[(3, 4, observation)],
        Some(1),
        Some(1),
    )
    .unwrap();

    assert!(projected.iter().any(|item| item == &request));
    assert!(projected.iter().any(|item| item == &later_work));
    assert!(projected.iter().any(|item| item == &active_call));
    assert!(projected.iter().any(|item| item == &opaque_media));
    assert!(projected.iter().any(|item| {
        match &item.item {
            ResponseItem::Message {
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            } => metadata
                .content_item_kinds
                .as_ref()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.0 == "compaction.observation")),
            _ => false,
        }
    }));
    let serialized = serde_json::to_string(&model_items(&projected)).unwrap();
    assert!(!serialized.contains("verified-call-item"));
    assert!(!serialized.contains("verified-output-item"));
    assert!(!serialized.contains(&"x".repeat(5_000)));
    assert!(serialized.contains("verified-call"));
    assert!(serialized.contains("HEAD:"));
    assert!(serialized.contains(":TAIL"));
}

#[test]
fn v2_generic_capture_projects_large_custom_tool_input_from_canonical_raw_pair() {
    let request = human("inspect the large custom result", "human-request");
    let canonical_call = format!("HEAD:{}:CANONICAL-CALL-TAIL", "c".repeat(5_000));
    let canonical_output = format!("RESULT-HEAD:{}:CANONICAL-OUTPUT-TAIL", "o".repeat(5_000));
    // The selected history item was already truncated by normal intake. F01 supplies the
    // verified canonical call/output text from the existing rollout to this projection.
    let current_custom_call = ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server("custom-call-item".into())),
        status: Some("completed".into()),
        call_id: "custom-call".into(),
        name: "custom_read".into(),
        namespace: None,
        input: "HEAD:[normal-history-truncated]".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let current_custom_output = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("custom-output-item".into())),
        call_id: "custom-call".into(),
        name: Some("custom_read".into()),
        output: FunctionCallOutputPayload::from_text("[normal-history-truncated]".into()),
        internal_chat_message_metadata_passthrough: None,
    });
    let active_custom_call = ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server(
            "active-custom-call-item".into(),
        )),
        status: Some("in_progress".into()),
        call_id: "active-custom-call".into(),
        name: "custom_write".into(),
        namespace: None,
        input: "must remain opaque".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let originals = vec![
        request.clone(),
        current_custom_call,
        current_custom_output,
        active_custom_call.clone(),
    ];
    let reference =
        ObservationReference::new(THREAD_ID, "custom-call", content_sha256(&canonical_output));
    let prepared = PreparedCompactionSources {
        pairs: vec![PreparedCompactionSourcePair {
            call_index: 1,
            output_index: 2,
            reference: reference.clone(),
            reference_kind: PreparedCompactionReferenceKind::Fresh,
            output_total_bytes: canonical_output.len(),
            tool_name: "custom_read",
            canonical_call_input: Some(&canonical_call),
            canonical_output_text: Some(Cow::Borrowed(canonical_output.as_str())),
            terminal_reference: None,
        }],
        protected_indices: vec![3],
    };

    // The host-proven custom pair becomes ordinary Work so the same native projection removes
    // it. Its large raw bodies stay in the rollout; CandidateInput does not copy them (Annex A F07).
    let input = super::history::capture(
        &originals,
        "snapshot-binding".into(),
        THREAD_ID,
        vec![],
        &super::history::verified_pair_projections(&prepared),
    )
    .unwrap();
    for index in [1, 2] {
        assert_eq!(input.records[index].origin, Origin::Work);
        assert!(input.records[index].opaque.is_none());
        assert!(input.records[index].text.is_empty());
    }

    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();
    let observation = codex_history::project_observation(
        &reference,
        "custom_read",
        &canonical_call,
        &canonical_output,
    )
    .unwrap()
    .summary;
    let summary_items = super::summary::build_summary_history(
        &originals,
        &input,
        &native,
        &[(1, 2, observation)],
        None,
        None,
    )
    .unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();

    assert!(summary_items.iter().any(|item| item == &request));
    assert!(summary_items.iter().any(|item| item == &active_custom_call));
    assert!(candidate.iter().any(|item| item == &active_custom_call));
    for projected in [&summary_items, &candidate] {
        let ids = projected.iter().filter_map(item_id).collect::<Vec<_>>();
        assert!(!ids.contains(&"custom-call-item"));
        assert!(!ids.contains(&"custom-output-item"));
        let serialized = serde_json::to_string(&model_items(projected)).unwrap();
        assert!(!serialized.contains(&"c".repeat(5_000)));
        assert!(!serialized.contains(&"o".repeat(5_000)));
    }
    let summary_json = serde_json::to_string(&model_items(&summary_items)).unwrap();
    assert!(summary_json.contains("CANONICAL-CALL-TAIL"));
    assert!(summary_json.contains("CANONICAL-OUTPUT-TAIL"));
    assert!(!summary_json.contains("normal-history-truncated"));
}

#[test]
fn v2_verified_pair_capture_keeps_full_text_only_for_bounded_pairs() {
    let small_call = r#"{"cmd":"cargo test"}"#;
    let small_output = "test result: ok";
    let large_call = r#"{"path":"large.txt"}"#;
    let large_output = "L".repeat(2_049);
    let reference =
        |output: &str| ObservationReference::new(THREAD_ID, "call", content_sha256(output));
    let prepared = PreparedCompactionSources {
        pairs: vec![
            PreparedCompactionSourcePair {
                call_index: 0,
                output_index: 1,
                reference: reference(small_output),
                reference_kind: PreparedCompactionReferenceKind::Fresh,
                output_total_bytes: small_output.len(),
                tool_name: "exec_command",
                canonical_call_input: Some(small_call),
                canonical_output_text: Some(Cow::Borrowed(small_output)),
                terminal_reference: None,
            },
            PreparedCompactionSourcePair {
                call_index: 2,
                output_index: 3,
                reference: reference(&large_output),
                reference_kind: PreparedCompactionReferenceKind::Fresh,
                output_total_bytes: large_output.len(),
                tool_name: "read_file",
                canonical_call_input: Some(large_call),
                canonical_output_text: Some(Cow::Borrowed(large_output.as_str())),
                terminal_reference: None,
            },
            PreparedCompactionSourcePair {
                call_index: 4,
                output_index: 5,
                reference: reference("archived output"),
                reference_kind: PreparedCompactionReferenceKind::Existing,
                output_total_bytes: "archived output".len(),
                tool_name: "exec_command",
                canonical_call_input: None,
                canonical_output_text: None,
                terminal_reference: None,
            },
        ],
        protected_indices: vec![],
    };

    assert_eq!(
        super::history::verified_pair_projections(&prepared),
        vec![
            super::history::CompletedWorkProjection {
                call_index: 0,
                output_index: 1,
                call_text: small_call.into(),
                output_text: small_output.into(),
            },
            super::history::CompletedWorkProjection {
                call_index: 2,
                output_index: 3,
                call_text: String::new(),
                output_text: String::new(),
            },
            super::history::CompletedWorkProjection {
                call_index: 4,
                output_index: 5,
                call_text: String::new(),
                output_text: String::new(),
            },
        ]
    );
}

#[test]
fn v2_summary_projection_reuses_the_same_pruned_human_envelope_as_native_replacement() {
    let mut request = human("remove this; keep🙂", "human-request");
    let ResponseItem::Message { content, .. } = &mut request.item else {
        unreachable!();
    };
    *content = vec![
        ContentItem::InputText {
            text: "remove this; ".into(),
        },
        ContentItem::InputText {
            text: "keep🙂".into(),
        },
    ];
    request
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_input
        .as_mut()
        .unwrap()["selected_text"] = json!("remove this; keep🙂");
    let originals = vec![request.clone()];
    let input = candidate_input(&originals, &[]);
    let source = input
        .snapshot()
        .unwrap()
        .reference(
            "human-request",
            ByteRange {
                start: 0,
                end: "remove this; ".len(),
            },
        )
        .unwrap();
    let application = InstructionSelectionApplication {
        pruning: prune_known_obsolete(&input, &[source]).unwrap(),
        plan_hash: Some("host-plan".into()),
        results: vec![],
    };
    let native =
        super::native::filter_retained_instructions(&originals, &input, application).unwrap();
    let summary_items =
        super::summary::build_summary_history(&originals, &input, &native, &[], None, None)
            .unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();
    let summary_human = summary_items
        .iter()
        .find(|item| item_id(item) == Some("human-request"))
        .unwrap();
    let candidate_human = candidate
        .iter()
        .find(|item| item_id(item) == Some("human-request"))
        .unwrap();

    assert_eq!(summary_human, candidate_human);
    assert_eq!(item_text(summary_human), "keep🙂");
    assert_eq!(
        summary_human
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_input
            .as_ref()
            .unwrap()["selected_text"],
        json!("keep🙂")
    );
}

#[test]
fn v2_summary_response_validation_accepts_assistant_text_with_reasoning() {
    let valid = [
        ResponseItem::Reasoning {
            id: None,
            summary: vec![],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "Current work and unresolved checks.".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(
        super::summary::summary_suffix_from_staged_output(&valid).unwrap(),
        "Current work and unresolved checks."
    );
}

#[test]
fn v2_summary_response_validation_rejects_nonassistant_or_tool_output_even_with_text() {
    let user = [ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::OutputText {
            text: "not an assistant summary".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    assert!(super::summary::summary_suffix_from_staged_output(&user).is_err());

    let assistant = ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "Current work and unresolved checks.".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let tool = [
        assistant,
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "unexpected-tool",
            "call_id": "unexpected",
            "name": "exec_command",
            "arguments": "{}",
        }))
        .unwrap(),
    ];
    assert!(super::summary::summary_suffix_from_staged_output(&tool).is_err());
    assert!(super::summary::summary_suffix_from_staged_output(&[]).is_err());
}

#[test]
fn v2_important_refs_resolve_only_explicit_json_string_markers_and_deduplicate() {
    let call_id = "call:\"quoted\"/path";
    let reference =
        ObservationReference::new(THREAD_ID, call_id, content_sha256("historical output"));
    let marker = format!(
        "Keep this citation observation:{} and repeat observation:{}.",
        serde_json::to_string(call_id).unwrap(),
        serde_json::to_string(call_id).unwrap(),
    );

    assert_eq!(
        super::summary::important_refs_from_summary(&marker, std::slice::from_ref(&reference)),
        super::summary::ImportantRefResolution {
            refs: vec![reference],
            ignored_markers: 0,
        }
    );
    assert_eq!(
        super::summary::important_refs_from_summary("No archive citation.", &[]),
        super::summary::ImportantRefResolution::default()
    );
}

#[test]
fn v2_important_refs_ignore_malformed_unknown_or_ambiguous_markers_without_failing() {
    let known = ObservationReference::new(THREAD_ID, "known-call", content_sha256("known output"));
    let ambiguous = [
        known.clone(),
        ObservationReference::new(THREAD_ID, "known-call", content_sha256("different output")),
    ];
    let ignored = |ignored_markers| super::summary::ImportantRefResolution {
        refs: vec![],
        ignored_markers,
    };

    assert_eq!(
        super::summary::important_refs_from_summary(
            "key observation: useful; observation:not-json; observation: maybe",
            std::slice::from_ref(&known),
        ),
        ignored(0)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "observation:\"unterminated",
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"observation:"bad\x" then text"#,
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "observation:\"unknown-call\"",
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary("observation:\"known-call\"", &ambiguous),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "Use observation:\"known-call\", skip observation:\"missing\", reuse observation:\"known-call\".",
            std::slice::from_ref(&known),
        ),
        super::summary::ImportantRefResolution {
            refs: vec![known],
            ignored_markers: 1,
        }
    );
}

#[test]
fn v2_important_refs_do_not_promote_an_inventory_without_explicit_markers() {
    let inventory = (0..256)
        .map(|index| {
            let call_id = format!("call-{index}");
            ObservationReference::new(
                THREAD_ID,
                call_id,
                content_sha256(&format!("output-{index}")),
            )
        })
        .collect::<Vec<_>>();
    let marker = format!(
        "Important source: observation:{}",
        serde_json::to_string("call-197").unwrap(),
    );

    assert_eq!(
        super::summary::important_refs_from_summary(&marker, &inventory),
        super::summary::ImportantRefResolution {
            refs: vec![inventory[197].clone()],
            ignored_markers: 0,
        }
    );
}

#[test]
fn v2_important_refs_resolve_ids_listed_directly_after_one_marker() {
    let reference =
        |call_id: &str| ObservationReference::new(THREAD_ID, call_id, content_sha256(call_id));
    let inventory = [
        reference("call_a"),
        reference("call_b"),
        reference("call_c"),
    ];
    let resolution = |indexes: &[usize], ignored_markers| super::summary::ImportantRefResolution {
        refs: indexes
            .iter()
            .map(|&index| inventory[index].clone())
            .collect(),
        ignored_markers,
    };

    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"Evidence: observation:"call_a", "call_b" , "missing","call_c"."#,
            &inventory,
        ),
        resolution(&[0, 1, 2], 1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"observation:"call_a" then observation:"call_b", "call_c""#,
            &inventory,
        ),
        resolution(&[0, 1, 2], 0)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"observation:"call_a". Edited file, "call_b" too."#,
            &inventory,
        ),
        resolution(&[0], 0)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"observation:"call_a", "unterminated observation:"call_b""#,
            &inventory,
        ),
        resolution(&[0, 1], 0)
    );
}

#[test]
fn fork_auxiliary_server_reasoning_metadata_does_not_change_active_context_policy() {
    // This pure dispatch-policy case does not claim an SSE fixture exercises the actual
    // Responses websocket ServerReasoningIncluded event. The drain integration still needs that
    // event or a direct test of the dispatch boundary after the policy is wired.
    assert!(!super::summary::should_apply_server_reasoning_included(
        /*rencrow_compaction*/ true
    ));
}

#[test]
fn disabled_compaction_keeps_server_reasoning_sideband_policy() {
    assert!(super::summary::should_apply_server_reasoning_included(
        /*rencrow_compaction*/ false
    ));
}

fn with_compaction_metadata(
    mut summary: ResponseItemEnvelope,
    update: impl FnOnce(&mut serde_json::Value),
) -> ResponseItemEnvelope {
    update(
        summary
            .metadata
            .as_mut()
            .unwrap()
            .rencrow_compaction
            .as_mut()
            .unwrap(),
    );
    summary
}

fn adopted_checkpoint(
    summary: &ResponseItemEnvelope,
    index: usize,
) -> super::summary::AdoptedV2Checkpoint {
    let summary_text = item_text(summary);
    let metadata = summary
        .metadata
        .as_ref()
        .unwrap()
        .rencrow_compaction
        .clone()
        .unwrap();
    super::summary::AdoptedV2Checkpoint {
        index,
        metadata: Some(
            RenCrowCompactionMetadataV2::parse_and_validate(metadata, THREAD_ID, &summary_text)
                .unwrap(),
        ),
        inherited_capacity_metadata: None,
        task_state: None,
        task_context_index: None,
        task_boundary: false,
        summary_text,
    }
}

#[test]
fn v2_adopted_checkpoint_accepts_live_and_replayed_transaction_lifecycles() {
    let replayed = checkpoint_summary("replayed state");
    let live = with_compaction_metadata(checkpoint_summary("live state"), |metadata| {
        metadata
            .as_object_mut()
            .unwrap()
            .remove("committed_transaction_hash");
        metadata["transaction_following_items"] = json!(1);
    });

    for summary in [replayed, live] {
        let history = vec![
            message("assistant", "earlier work", "work"),
            summary.clone(),
        ];
        assert_eq!(
            super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID),
            Ok(Some(adopted_checkpoint(&summary, 1)))
        );
    }
}

#[test]
fn v2_adopted_checkpoint_rejects_invalid_typed_metadata_instead_of_falling_back() {
    let valid = checkpoint_summary("older valid state");
    let both_lifecycles = with_compaction_metadata(checkpoint_summary("ambiguous"), |metadata| {
        metadata["transaction_following_items"] = json!(1);
    });
    let tampered = with_compaction_metadata(checkpoint_summary("tampered"), |metadata| {
        metadata["summary_hash"] = json!("c".repeat(64));
    });
    let mut untyped = message("assistant", "not a summary", "assistant-with-v2");
    untyped.metadata.get_or_insert_default().rencrow_compaction = checkpoint_summary("state")
        .metadata
        .unwrap()
        .rencrow_compaction;

    for broken in [both_lifecycles, tampered, untyped] {
        let history = vec![valid.clone(), message("assistant", "work", "work"), broken];
        assert!(super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID).is_err());
    }
}

#[test]
fn v2_adopted_checkpoint_selects_the_latest_v2_and_ignores_legacy_summaries() {
    let older = checkpoint_summary("older state");
    let latest = checkpoint_summary("latest state");
    let legacy = with_compaction_metadata(checkpoint_summary("legacy state"), |metadata| {
        *metadata = json!({"version": 1, "input_hash": "legacy"});
    });
    let history = vec![
        older,
        message("assistant", "first work", "work-1"),
        latest.clone(),
        message("assistant", "second work", "work-2"),
        legacy.clone(),
    ];

    assert_eq!(
        super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID),
        Ok(Some(adopted_checkpoint(&latest, 2)))
    );
    assert_eq!(
        super::summary::find_adopted_v2_checkpoint(
            &[message("assistant", "work", "work"), legacy],
            THREAD_ID,
        ),
        Ok(None)
    );
}

#[test]
fn v3_task_boundary_capacity_and_next_boundary_preserve_typed_task_state() {
    let boundary = task_boundary_summary("task A accepted; task B remains");
    let boundary_metadata = boundary
        .metadata
        .as_ref()
        .unwrap()
        .rencrow_compaction
        .as_ref()
        .unwrap();
    let saved_state = boundary_metadata["task_state"].clone();
    let mut capacity = checkpoint_summary("capacity summary");
    let capacity_text = item_text(&capacity);
    let v2 = RenCrowCompactionMetadataV2::parse_and_validate(
        capacity
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_compaction
            .clone()
            .unwrap(),
        THREAD_ID,
        &capacity_text,
    )
    .unwrap();
    capacity.metadata.get_or_insert_default().rencrow_compaction = Some(
        serde_json::to_value(RenCrowTaskCheckpointMetadataV3::capacity(
            &capacity_text,
            Some(saved_state.clone()),
            v2,
        ))
        .unwrap(),
    );

    let carried_capacity = capacity.clone();
    let after_capacity = vec![
        boundary.clone(),
        message("assistant", "unrelated old work", "work"),
        capacity,
    ];
    let adopted = super::summary::find_adopted_v2_checkpoint(&after_capacity, THREAD_ID)
        .unwrap()
        .unwrap();
    assert!(!adopted.task_boundary);
    assert_eq!(adopted.task_state, Some(saved_state));
    assert_eq!(
        super::summary::semantic_context(&after_capacity, Some(&adopted)),
        super::summary::SemanticContext {
            previous_summary: Some(2),
            task_context: Some(0),
            work_boundary: Some(2),
            selection_boundary: Some(2),
        }
    );
    let context = super::summary::semantic_context(&after_capacity, Some(&adopted));
    let input = candidate_input(&after_capacity, &[]);
    let native = super::native::filter_retained_instructions_with_task_context(
        &after_capacity,
        &input,
        application(&input),
        context.task_context,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    let summary_input = super::summary::build_summary_history_with_task_context(
        &after_capacity,
        &input,
        &native,
        &[],
        context.previous_summary,
        context.work_boundary,
        context.task_context,
    )
    .unwrap();
    assert_eq!(
        summary_input
            .iter()
            .filter(|item| item_id(item) == Some("old-summary"))
            .count(),
        1
    );
    assert_eq!(
        summary_input
            .iter()
            .filter(|item| item_id(item) == Some("task-boundary-json"))
            .count(),
        1
    );
    assert!(
        summary_input
            .iter()
            .find(|item| item_id(item) == Some("task-boundary-json"))
            .is_some_and(|item| item_text(item) == "task A accepted; task B remains")
    );

    let next_boundary = task_boundary_summary("task B accepted");
    let after_next_boundary = vec![
        boundary,
        message("assistant", "old", "old"),
        carried_capacity,
        next_boundary,
    ];
    let adopted = super::summary::find_adopted_v2_checkpoint(&after_next_boundary, THREAD_ID)
        .unwrap()
        .unwrap();
    assert!(adopted.task_boundary);
    assert_eq!(adopted.task_state, Some(stored_task_state_fixture()));
    assert!(adopted.metadata.is_none());
    assert_eq!(
        adopted
            .inherited_capacity_metadata
            .as_ref()
            .unwrap()
            .selection_mode,
        codex_history::CompactionSelectionMode::NoCandidates
    );
    assert_eq!(
        super::summary::semantic_context(&after_next_boundary, Some(&adopted)),
        super::summary::SemanticContext {
            previous_summary: Some(2),
            task_context: Some(3),
            work_boundary: None,
            selection_boundary: None,
        }
    );
}

#[test]
fn emergency_carries_the_prior_semantic_summary_across_a_task_boundary() {
    let capacity = checkpoint_summary("accepted semantic state");
    let task_body = r#"{"format":"task_context_v1","goal":"continue task B","tasks":[{"id":"task-b","status":"pending"}]}"#;
    let mut task_boundary = message("user", task_body, "task-boundary-json");
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut task_boundary.item
    else {
        unreachable!();
    };
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
        ..Default::default()
    });
    let task_checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
        task_body,
        stored_task_state_fixture(),
        TaskBoundarySourceFence {
            history_hash: "a".repeat(64),
            removed_refs: vec![],
        },
    );
    task_boundary
        .metadata
        .get_or_insert_default()
        .rencrow_compaction = Some(serde_json::to_value(task_checkpoint).unwrap());
    let history = vec![capacity.clone(), task_boundary];
    let adopted = super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID)
        .unwrap()
        .unwrap();
    let saved_task_state = adopted.task_state.clone().unwrap();
    let semantic = super::summary::semantic_context(&history, Some(&adopted));

    let (carried, is_semantic) =
        super::emergency::emergency_summary_text(&history, semantic.previous_summary).unwrap();

    assert_eq!(carried, item_text(&capacity));
    assert!(is_semantic);
    assert_eq!(semantic.previous_summary, Some(0));
    assert_eq!(semantic.task_context, Some(1));
    assert_eq!(saved_task_state, stored_task_state_fixture());

    let input = candidate_input(&history, &[]);
    let projection = super::native::filter_retained_instructions_with_task_context(
        &history,
        &input,
        application(&input),
        semantic.task_context,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    let projected = super::summary::build_summary_history_with_task_context(
        &history,
        &input,
        &projection,
        &[],
        semantic.previous_summary,
        semantic.work_boundary,
        semantic.task_context,
    )
    .unwrap();
    assert_eq!(
        projected
            .iter()
            .filter(|item| item_id(item) == Some("old-summary"))
            .count(),
        1
    );
    assert_eq!(
        projected
            .iter()
            .filter(|item| item_id(item) == Some("task-boundary-json"))
            .count(),
        1
    );
    assert!(item_text(&projected[1]).contains("task_context_v1"));
    let emergency = super::native::build_native_replacement(&projection, &carried, vec![]).unwrap();
    assert!(
        emergency
            .iter()
            .any(|item| item_id(item) == Some("task-boundary-json"))
    );
}

#[test]
fn task_boundary_without_prior_semantic_summary_keeps_task_json_and_uses_placeholder() {
    let task_boundary =
        task_boundary_summary(r#"{"format":"task_context_v1","goal":"continue","tasks":[]}"#);
    let history = vec![task_boundary];
    let adopted = super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID)
        .unwrap()
        .unwrap();
    let semantic = super::summary::semantic_context(&history, Some(&adopted));
    assert_eq!(semantic.previous_summary, None);
    assert_eq!(semantic.task_context, Some(0));
    let (summary_text, semantic_summary) =
        super::emergency::emergency_summary_text(&history, semantic.previous_summary).unwrap();
    assert!(!semantic_summary);
    assert!(summary_text.contains(super::emergency::EMERGENCY_PLACEHOLDER));

    let input = candidate_input(&history, &[]);
    let native = super::native::filter_retained_instructions_with_task_context(
        &history,
        &input,
        application(&input),
        semantic.task_context,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    let replacement =
        super::native::build_native_replacement(&native, &summary_text, vec![]).unwrap();
    assert!(
        replacement
            .iter()
            .any(|item| item_id(item) == Some("task-boundary-json"))
    );
}

#[test]
fn task_boundary_rejects_a_prior_semantic_hash_that_does_not_match_its_checkpoint() {
    let mut capacity = checkpoint_summary("accepted semantic state");
    capacity
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap()["semantic_summary_hash"] = json!("c".repeat(64));
    let boundary =
        task_boundary_summary(r#"{"format":"task_context_v1","goal":"continue","tasks":[]}"#);
    assert!(super::summary::find_adopted_v2_checkpoint(&[capacity, boundary], THREAD_ID).is_err());
}

#[test]
fn capacity_checkpoint_with_task_state_but_no_task_envelope_fails_closed() {
    let mut capacity = checkpoint_summary("accepted capacity state");
    let text = item_text(&capacity);
    let v2 = RenCrowCompactionMetadataV2::parse_and_validate(
        capacity
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_compaction
            .clone()
            .unwrap(),
        THREAD_ID,
        &text,
    )
    .unwrap();
    capacity.metadata.get_or_insert_default().rencrow_compaction = Some(
        serde_json::to_value(RenCrowTaskCheckpointMetadataV3::capacity(
            &text,
            Some(stored_task_state_fixture()),
            v2,
        ))
        .unwrap(),
    );
    let error = super::summary::find_adopted_v2_checkpoint(&[capacity], THREAD_ID)
        .expect_err("state-only capacity must not silently lose its task context");
    assert!(error.contains("without its typed task context envelope"));
}

#[test]
fn unknown_checkpoint_version_fails_closed_instead_of_falling_back() {
    let old = checkpoint_summary("older");
    let future = with_compaction_metadata(checkpoint_summary("future"), |metadata| {
        metadata["version"] = json!(77);
    });
    assert!(super::summary::find_adopted_v2_checkpoint(&[old, future], THREAD_ID).is_err());
}

fn function_call(call_id: &str, arguments: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call",
            "id": format!("{call_id}-item"),
            "call_id": call_id,
            "name": "exec_command",
            "arguments": arguments,
        }))
        .unwrap(),
    )
}

fn function_output(call_id: &str, output: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server(format!(
            "{call_id}-output-item"
        ))),
        call_id: Some(call_id.into()),
        name: Some("exec_command".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(output.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn prepared_pair<'a>(
    call_index: usize,
    reference: ObservationReference,
    reference_kind: PreparedCompactionReferenceKind,
    texts: Option<(&'a str, &'a str)>,
    output_total_bytes: usize,
) -> PreparedCompactionSourcePair<'a> {
    PreparedCompactionSourcePair {
        call_index,
        output_index: call_index + 1,
        reference,
        reference_kind,
        output_total_bytes,
        tool_name: "exec_command",
        canonical_call_input: texts.map(|(call, _)| call),
        canonical_output_text: texts.map(|(_, output)| Cow::Borrowed(output)),
        terminal_reference: None,
    }
}

fn output_with_utf8_size(size: usize) -> String {
    let prefix = "Chunk ID: focused-red\nWall time: 0.0000s\nProcess exited with code 0\nOriginal token count: 0\nOutput:\n";
    let mut payload = json!({
        "action": "update",
        "root_task_id": "root",
        "completion_state": {
            "conditions": [{"id": "AC-3", "status": "open"}],
            "units": [{"id": "u-c-browser-save-roundtrip", "status": "planned", "next_action": "test exact save and resume"}],
            "notes": ""
        }
    });
    let base = serde_json::to_string(&payload).unwrap();
    let padding = size - prefix.len() - base.len();
    let notes = format!("{}{}", "値".repeat(padding / 3), "x".repeat(padding % 3));
    payload["completion_state"]["notes"] = json!(notes);
    let output = format!("{prefix}{}", serde_json::to_string(&payload).unwrap());
    assert_eq!(output.len(), size);
    output
}

fn named_function_call(call_id: &str, name: &str, arguments: &str) -> ResponseItemEnvelope {
    let mut call = function_call(call_id, arguments);
    let ResponseItem::FunctionCall { name: tool_name, .. } = &mut call.item else {
        unreachable!();
    };
    *tool_name = name.into();
    call
}

fn named_function_output(call_id: &str, name: &str, output: &str) -> ResponseItemEnvelope {
    let mut item = function_output(call_id, output);
    let ResponseItem::FunctionCallOutput {
        name: tool_name, ..
    } = &mut item.item else {
        unreachable!();
    };
    *tool_name = Some(name.into());
    item
}

fn named_prepared_pair<'a>(
    call_index: usize,
    call_id: &str,
    tool_name: &'a str,
    call_text: &'a str,
    output: &'a str,
) -> PreparedCompactionSourcePair<'a> {
    PreparedCompactionSourcePair {
        call_index,
        output_index: call_index + 1,
        reference: ObservationReference::new(THREAD_ID, call_id, content_sha256(output)),
        reference_kind: PreparedCompactionReferenceKind::Fresh,
        output_total_bytes: output.len(),
        tool_name,
        canonical_call_input: Some(call_text),
        canonical_output_text: Some(Cow::Borrowed(output)),
        terminal_reference: None,
    }
}

#[test]
fn latest_task_state_and_read_working_tail_survive_native_replacement() {
    let old_go_output = "old go test output ".repeat(1_150);
    let status_args = r#"{"action":"status","root_task_id":"root"}"#;
    let status_output = output_with_utf8_size(6_605);
    let read_args = r#"{"cmd":"cat /tmp/prepared-update.json"}"#;
    let exact_update_output = output_with_utf8_size(9_852);
    let mut automation = human("Read the prepared update and then apply it.", "automation-request");
    automation.metadata.as_mut().unwrap().rencrow_input.as_mut().unwrap()["author"] =
        json!("automation");
    let originals = vec![
        function_call("old-go-call", r#"{"cmd":"go test ./..."}"#),
        function_output("old-go-call", &old_go_output),
        automation,
        named_function_call("latest-status", "task_context_boundary", status_args),
        named_function_output("latest-status", "task_context_boundary", &status_output),
        function_call("latest-read", read_args),
        function_output("latest-read", &exact_update_output),
    ];
    let mut sources = PreparedCompactionSources {
        pairs: vec![
            named_prepared_pair(0, "old-go-call", "exec_command", r#"{"cmd":"go test ./..."}"#, &old_go_output),
            named_prepared_pair(3, "latest-status", "task_context_boundary", status_args, &status_output),
            named_prepared_pair(5, "latest-read", "exec_command", read_args, &exact_update_output),
        ],
        protected_indices: vec![],
    };
    let full_input = candidate_input(
        &originals,
        &super::history::verified_pair_projections(&sources),
    );
    let protected = super::stages::protected_working_tail_call_ids(
        &originals,
        &full_input,
        &sources,
        Some("root"),
    );
    super::stages::partition_task_boundary_pairs(&mut sources, &protected);
    let input = candidate_input(
        &originals,
        &super::history::verified_pair_projections(&sources),
    );
    let projections = super::observation::project_unhandled_observations(&originals, &sources, &[]).unwrap();
    let covered = super::observation::summary_covered_after_normal(&[], &projections);
    let native = super::native::filter_retained_instructions_with_task_context(
        &originals,
        &input,
        application(&input),
        None,
        &protected,
    )
    .unwrap();
    let rebuilt = super::native::build_native_replacement(&native, "next action", vec![]).unwrap();
    let output_for = |call_id: &str| {
        rebuilt.iter().find_map(|item| match &item.item {
            ResponseItem::FunctionCallOutput { call_id: Some(id), output, .. } if id == call_id => output.text_content(),
            _ => None,
        })
    };
    assert_eq!(output_for("latest-status"), Some(status_output.as_str()));
    assert_eq!(output_for("latest-read"), Some(exact_update_output.as_str()));
    assert_eq!(protected, ["latest-status", "latest-read"].into_iter().map(str::to_owned).collect());
    assert_eq!(
        sources.pairs.iter().map(|pair| pair.reference.call_id.as_str()).collect::<Vec<_>>(),
        ["old-go-call"]
    );
    assert_eq!(covered.iter().map(|reference| reference.call_id.as_str()).collect::<Vec<_>>(), ["old-go-call"]);
    assert!(!rebuilt.iter().any(|item| matches!(
        &item.item,
        ResponseItem::FunctionCallOutput { call_id: Some(id), .. } if id == "old-go-call"
    )));
}

#[test]
fn working_tail_uses_latest_read_before_size_check_and_requires_active_root() {
    let request = {
        let mut item = human("Use the latest status and read result.", "automation-request");
        item.metadata.as_mut().unwrap().rencrow_input.as_mut().unwrap()["author"] = json!("automation");
        item
    };
    let small = "small previous read";
    let oversized = output_with_utf8_size(12 * 1_024 + 1);
    let wrong_root_args = r#"{"action":"status","root_task_id":"other-root"}"#;
    let status = "wrong root status";
    let read_a = r#"{"cmd":"cat /tmp/older.json"}"#;
    let read_b = r#"{"cmd":"cat /tmp/latest.json"}"#;
    let originals = vec![
        request,
        function_call("older-read", read_a),
        function_output("older-read", small),
        function_call("latest-read", read_b),
        function_output("latest-read", &oversized),
        named_function_call("wrong-status", "task_context_boundary", wrong_root_args),
        named_function_output("wrong-status", "task_context_boundary", status),
    ];
    let sources = PreparedCompactionSources {
        pairs: vec![
            named_prepared_pair(1, "older-read", "exec_command", read_a, small),
            named_prepared_pair(3, "latest-read", "exec_command", read_b, &oversized),
            named_prepared_pair(5, "wrong-status", "task_context_boundary", wrong_root_args, status),
        ],
        protected_indices: vec![],
    };
    let input = candidate_input(&originals, &super::history::verified_pair_projections(&sources));
    assert!(super::stages::protected_working_tail_call_ids(
        &originals,
        &input,
        &sources,
        Some("root"),
    )
    .is_empty());
}

#[test]
fn v2_observation_handling_projects_only_uncovered_identities() {
    let covered_call = r#"{"cmd":"cargo test"}"#;
    let covered_output = "test result: ok";
    let fresh_call = r#"{"cmd":"cat large.txt"}"#;
    let fresh_output = format!("HEAD:{}:TAIL", "x".repeat(5_000));
    let existing_call = r#"{"cmd":"cat archived.txt"}"#;
    let existing_output = "archived output body";
    // Position does not decide handling: the uncovered pair at index 0 was active at the previous
    // checkpoint and completed later, so it is still projected.
    let originals = vec![
        function_call("fresh-call", fresh_call),
        function_output("fresh-call", "[truncated in live history]"),
        function_call("covered-call", covered_call),
        function_output("covered-call", covered_output),
        function_call("existing-call", existing_call),
        function_output("existing-call", "[archived reference marker]"),
    ];
    let reference = |call_id: &str, output: &str| {
        ObservationReference::new(THREAD_ID, call_id, content_sha256(output))
    };
    let prepared = PreparedCompactionSources {
        pairs: vec![
            prepared_pair(
                0,
                reference("fresh-call", &fresh_output),
                PreparedCompactionReferenceKind::Fresh,
                Some((fresh_call, fresh_output.as_str())),
                fresh_output.len(),
            ),
            prepared_pair(
                2,
                reference("covered-call", covered_output),
                PreparedCompactionReferenceKind::Fresh,
                Some((covered_call, covered_output)),
                covered_output.len(),
            ),
            prepared_pair(
                4,
                reference("existing-call", existing_output),
                PreparedCompactionReferenceKind::Existing,
                None,
                existing_output.len(),
            ),
        ],
        protected_indices: vec![],
    };

    let projections = super::observation::project_unhandled_observations(
        &originals,
        &prepared,
        &[reference("covered-call", covered_output)],
    )
    .unwrap();

    assert_eq!(
        projections,
        vec![
            (
                0,
                1,
                codex_history::project_observation(
                    &reference("fresh-call", &fresh_output),
                    "exec_command",
                    fresh_call,
                    &fresh_output,
                )
                .unwrap(),
            ),
            (
                4,
                5,
                codex_history::project_existing_observation(
                    &reference("existing-call", existing_output),
                    "exec_command",
                    existing_call,
                    existing_output.len(),
                )
                .unwrap(),
            ),
        ]
    );
    assert!(projections[0].2.coverage.output.partial);
    assert!(projections[1].2.coverage.output.partial);
}

#[test]
fn v2_handled_set_is_summary_coverage_not_stored_inventory() {
    // An observation stored only in inventory (for example by an emergency marker) was never
    // presented to an accepted summary, so the next Normal presents it and then covers it.
    let call = r#"{"cmd":"cat build.log"}"#;
    let output = "build log body";
    let reference = ObservationReference::new(THREAD_ID, "stored-call", content_sha256(output));
    let inventory_only =
        codex_history::project_observation(&reference, "exec_command", call, output)
            .unwrap()
            .coverage;
    let summary = with_compaction_metadata(checkpoint_summary("state"), |metadata| {
        metadata["observations"] = json!([inventory_only]);
    });
    let originals = vec![
        summary,
        function_call("stored-call", call),
        function_output("stored-call", output),
    ];
    let adopted = super::summary::find_adopted_v2_checkpoint(&originals, THREAD_ID)
        .unwrap()
        .unwrap();
    assert!(
        adopted
            .metadata
            .as_ref()
            .unwrap()
            .summary_covered_observations
            .is_empty()
    );
    let prepared = PreparedCompactionSources {
        pairs: vec![prepared_pair(
            1,
            reference.clone(),
            PreparedCompactionReferenceKind::Fresh,
            Some((call, output)),
            output.len(),
        )],
        protected_indices: vec![],
    };

    let projections = super::observation::project_unhandled_observations(
        &originals,
        &prepared,
        &adopted
            .metadata
            .as_ref()
            .unwrap()
            .summary_covered_observations,
    )
    .unwrap();
    assert_eq!(projections.len(), 1);
    let covered = super::observation::summary_covered_after_normal(
        &adopted
            .metadata
            .as_ref()
            .unwrap()
            .summary_covered_observations,
        &projections,
    );
    assert_eq!(covered, vec![reference]);

    // Once an accepted summary covered it, the same identity is handled and never re-presented.
    assert_eq!(
        super::observation::project_unhandled_observations(&originals, &prepared, &covered),
        Ok(vec![])
    );
    assert_eq!(
        super::observation::summary_covered_after_normal(&covered, &projections),
        covered
    );
}

#[test]
fn v2_observation_handling_rejects_a_recorded_call_id_with_a_different_digest() {
    let call = r#"{"cmd":"cat file"}"#;
    let output = "current output";
    let originals = vec![
        function_call("same-call", call),
        function_output("same-call", output),
    ];
    let prepared = PreparedCompactionSources {
        pairs: vec![prepared_pair(
            0,
            ObservationReference::new(THREAD_ID, "same-call", content_sha256(output)),
            PreparedCompactionReferenceKind::Fresh,
            Some((call, output)),
            output.len(),
        )],
        protected_indices: vec![],
    };

    assert!(
        super::observation::project_unhandled_observations(
            &originals,
            &prepared,
            &[ObservationReference::new(
                THREAD_ID,
                "same-call",
                content_sha256("a different recorded output"),
            )],
        )
        .is_err()
    );
}

#[test]
fn v2_cumulative_observation_coverage_deduplicates_identity_and_rejects_digest_conflicts() {
    let coverage = |call_id: &str, output: &str| {
        codex_history::project_observation(
            &ObservationReference::new(THREAD_ID, call_id, content_sha256(output)),
            "exec_command",
            "{}",
            output,
        )
        .unwrap()
        .coverage
    };
    let first = coverage("first-call", "first output");
    let second = coverage("second-call", "second output");

    assert_eq!(
        super::observation::cumulative_observation_coverage(
            std::slice::from_ref(&first),
            &[first.clone(), second.clone()],
        ),
        Ok(vec![first.clone(), second])
    );
    assert!(
        super::observation::cumulative_observation_coverage(
            std::slice::from_ref(&first),
            &[coverage("first-call", "changed output")],
        )
        .is_err()
    );
}

#[test]
fn v2_known_pruning_applies_adopted_refs_only_while_their_source_text_is_unchanged() {
    let original = human("remove this; keep", "human-request");
    let first_input = candidate_input(std::slice::from_ref(&original), &[]);
    let source = first_input
        .snapshot()
        .unwrap()
        .reference(
            "human-request",
            ByteRange {
                start: 0,
                end: "remove this; ".len(),
            },
        )
        .unwrap();
    let first_application = InstructionSelectionApplication {
        pruning: prune_known_obsolete(&first_input, std::slice::from_ref(&source)).unwrap(),
        plan_hash: Some("host-plan".into()),
        results: vec![],
    };
    let native = super::native::filter_retained_instructions(
        std::slice::from_ref(&original),
        &first_input,
        first_application,
    )
    .unwrap();
    let pruned = native.retained_native_items().next().unwrap().clone();
    let summary = with_compaction_metadata(checkpoint_summary("state"), |metadata| {
        metadata["applied_refs"] = json!([source]);
    });

    // After the committed replacement physically removed the passage, the adopted ref is stale
    // for the current text: it applies nothing and the removed passage is not revived.
    let compacted = vec![pruned, summary.clone()];
    let adopted = super::summary::find_adopted_v2_checkpoint(&compacted, THREAD_ID)
        .unwrap()
        .unwrap();
    let compacted_input = candidate_input(&compacted, &[]);
    assert_eq!(compacted_input.records[0].text, "keep");
    assert_eq!(
        prune_known_obsolete(
            &compacted_input,
            &adopted.metadata.as_ref().unwrap().applied_refs
        ),
        Ok(InstructionPruning::default())
    );

    // While the original source text is still present, the same adopted ref applies exactly.
    let unpruned = vec![original, summary];
    let unpruned_input = candidate_input(&unpruned, &[]);
    let pruning = prune_known_obsolete(
        &unpruned_input,
        &adopted.metadata.as_ref().unwrap().applied_refs,
    )
    .unwrap();
    assert_eq!(pruning.applied, vec![source]);
    assert_eq!(
        pruning
            .retained_human_text
            .get("human-request")
            .map(String::as_str),
        Some("keep")
    );
}

fn in_turn(mut envelope: ResponseItemEnvelope, turn_id: &str) -> ResponseItemEnvelope {
    envelope.item.set_turn_id_if_missing(turn_id);
    envelope
}

fn terminal_exec_pair<'a>(
    call_index: usize,
    call_id: &str,
    call: &'a str,
    output: &'a str,
) -> PreparedCompactionSourcePair<'a> {
    PreparedCompactionSourcePair {
        terminal_reference: Some(ArchiveReference::new(
            THREAD_ID,
            call_id,
            content_sha256(output),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        )),
        ..prepared_pair(
            call_index,
            ObservationReference::new(THREAD_ID, call_id, content_sha256(output)),
            PreparedCompactionReferenceKind::Fresh,
            Some((call, output)),
            output.len(),
        )
    }
}

fn whole_ref(input: &CandidateInput, index: usize) -> codex_history::compaction_plan::SourceRef {
    let record = &input.records[index];
    input
        .snapshot()
        .unwrap()
        .reference(
            &record.id,
            ByteRange {
                start: 0,
                end: record.text.len(),
            },
        )
        .unwrap()
}

#[test]
fn v2_completion_links_link_only_the_unambiguous_single_exec_turn() {
    let call = r#"{"cmd":"cargo test"}"#;
    let output = "test result: ok";
    let large_output = "L".repeat(2_049);
    let originals = vec![
        in_turn(human("Run the tests.", "human-a"), "turn-a"),
        in_turn(function_call("call-a", call), "turn-a"),
        in_turn(function_output("call-a", output), "turn-a"),
        in_turn(human("Run both checks.", "human-b"), "turn-b"),
        in_turn(function_call("call-b1", call), "turn-b"),
        in_turn(function_output("call-b1", output), "turn-b"),
        in_turn(function_call("call-b2", call), "turn-b"),
        in_turn(function_output("call-b2", output), "turn-b"),
        in_turn(human("First request.", "human-c1"), "turn-c"),
        in_turn(human("Second request.", "human-c2"), "turn-c"),
        in_turn(function_call("call-c", call), "turn-c"),
        in_turn(function_output("call-c", output), "turn-c"),
        in_turn(human("Show the large file.", "human-d"), "turn-d"),
        in_turn(function_call("call-d", call), "turn-d"),
        in_turn(function_output("call-d", &large_output), "turn-d"),
    ];
    let prepared = PreparedCompactionSources {
        pairs: vec![
            terminal_exec_pair(1, "call-a", call, output),
            terminal_exec_pair(4, "call-b1", call, output),
            terminal_exec_pair(6, "call-b2", call, output),
            terminal_exec_pair(10, "call-c", call, output),
            terminal_exec_pair(13, "call-d", call, &large_output),
        ],
        protected_indices: vec![],
    };
    let input = candidate_input(
        &originals,
        &super::history::verified_pair_projections(&prepared),
    );
    let pruning = prune_known_obsolete(&input, &[]).unwrap();

    let links = super::observation::build_completion_links(&originals, &input, &pruning, &prepared)
        .unwrap();

    assert_eq!(
        links,
        vec![InstructionObservationLink {
            instruction: whole_ref(&input, 0),
            call: whole_ref(&input, 1),
            output: whole_ref(&input, 2),
            terminal: prepared.pairs[0].terminal_reference.clone().unwrap(),
        }]
    );
    let payload = collect_instruction_candidates(&input, &pruning, &links)
        .unwrap()
        .unwrap();
    assert_eq!(payload["completion_links"].as_array().unwrap().len(), 1);
}

#[test]
fn v2_completion_links_skip_pruned_humans_and_unattributed_tool_activity() {
    let call = r#"{"cmd":"cargo test"}"#;
    let output = "test result: ok";
    let linked = vec![
        in_turn(human("Old detail. Run the tests.", "human-a"), "turn-a"),
        in_turn(function_call("call-a", call), "turn-a"),
        in_turn(function_output("call-a", output), "turn-a"),
    ];
    let prepared = PreparedCompactionSources {
        pairs: vec![terminal_exec_pair(1, "call-a", call, output)],
        protected_indices: vec![],
    };
    let completed = super::history::verified_pair_projections(&prepared);
    let input = candidate_input(&linked, &completed);
    let known = input
        .snapshot()
        .unwrap()
        .reference(
            "human-a",
            ByteRange {
                start: 0,
                end: "Old detail. ".len(),
            },
        )
        .unwrap();
    let pruning = prune_known_obsolete(&input, &[known]).unwrap();
    assert_eq!(
        super::observation::build_completion_links(&linked, &input, &pruning, &prepared),
        Ok(vec![])
    );

    let mut unattributed = linked;
    unattributed.push(function_call("call-without-turn", call));
    let input = candidate_input(&unattributed, &completed);
    assert_eq!(
        super::observation::build_completion_links(
            &unattributed,
            &input,
            &prune_known_obsolete(&input, &[]).unwrap(),
            &prepared,
        ),
        Ok(vec![])
    );
}

#[test]
fn v2_selection_is_required_only_for_new_humans_or_completion_links() {
    let originals = vec![
        human("Earlier request.", "human-old"),
        checkpoint_summary("state"),
        message("assistant", "work after the summary", "work"),
    ];
    let input = candidate_input(&originals, &[]);
    assert!(!super::selection_required(&input, Some(1), &[]));
    assert!(super::selection_required(&input, None, &[]));

    let mut unverified = originals.clone();
    unverified.push(message("user", "Unverified request.", "user-unverified"));
    assert!(!super::selection_required(
        &candidate_input(&unverified, &[]),
        Some(1),
        &[]
    ));

    let mut with_new_human = originals;
    with_new_human.push(human("New request.", "human-new"));
    assert!(super::selection_required(
        &candidate_input(&with_new_human, &[]),
        Some(1),
        &[]
    ));

    let link = InstructionObservationLink {
        instruction: whole_ref(&input, 0),
        call: whole_ref(&input, 2),
        output: whole_ref(&input, 2),
        terminal: ArchiveReference::new(
            THREAD_ID,
            "call",
            content_sha256("output"),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        ),
    };
    assert!(super::selection_required(&input, Some(1), &[link]));
}

fn host_selection(
    payload: &serde_json::Value,
    operations: Vec<ProposedOperation>,
) -> InstructionSelection {
    InstructionSelection::from_host(
        payload["snapshot_hash"].as_str().unwrap().into(),
        payload["presentation_hash"].as_str().unwrap().into(),
        ProposedPlan { operations },
    )
}

fn human_texts(items: &[ResponseItemEnvelope]) -> Vec<(Option<&str>, String)> {
    items
        .iter()
        .filter(|item| {
            item.metadata
                .as_ref()
                .is_some_and(|metadata| metadata.rencrow_input.is_some())
        })
        .map(|item| (item_id(item), item_text(item)))
        .collect()
}

#[test]
fn v2_model_selection_shares_one_application_between_summary_and_replacement() {
    let originals = vec![
        human("Keep Japanese. Use obsolete-label.", "human-a"),
        message("assistant", "Using obsolete-label for now.", "work"),
        human("Use current-label instead.", "human-b"),
    ];
    let input = candidate_input(&originals, &[]);
    let pruning = prune_known_obsolete(&input, &[]).unwrap();
    let payload = collect_instruction_candidates(&input, &pruning, &[])
        .unwrap()
        .unwrap();
    let selection = host_selection(
        &payload,
        vec![ProposedOperation::DropSuperseded {
            source: "human-a".into(),
            source_text: Some("Use obsolete-label.".into()),
            correction: "human-b".into(),
            correction_text: Some("Use current-label instead.".into()),
        }],
    );

    let application = validate_and_apply_selection(&input, &pruning, &[], Some(selection)).unwrap();
    assert_eq!(application.pruning.applied.len(), 1);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application).unwrap();
    let summary_items =
        super::summary::build_summary_history(&originals, &input, &native, &[], None, None)
            .unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();

    let expected = vec![
        (Some("human-a"), "Keep Japanese. ".to_string()),
        (Some("human-b"), "Use current-label instead.".to_string()),
    ];
    assert_eq!(human_texts(&summary_items), expected);
    assert_eq!(human_texts(&candidate), expected);
    for items in [&summary_items, &candidate] {
        let humans = serde_json::to_string(
            &items
                .iter()
                .filter(|item| {
                    item.metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.rencrow_input.is_some())
                })
                .map(|item| &item.item)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!humans.contains("Use obsolete-label."));
    }
}

#[test]
fn v2_completion_selection_removes_the_completed_request_and_keeps_only_its_result() {
    let call = r#"{"cmd":"cargo test"}"#;
    let output = "test result: ok";
    let originals = vec![
        in_turn(human("Run the tests.", "human-a"), "turn-a"),
        in_turn(function_call("call-a", call), "turn-a"),
        in_turn(function_output("call-a", output), "turn-a"),
        in_turn(human("Then update the notes.", "human-b"), "turn-b"),
    ];
    let prepared = PreparedCompactionSources {
        pairs: vec![terminal_exec_pair(1, "call-a", call, output)],
        protected_indices: vec![],
    };
    let input = candidate_input(
        &originals,
        &super::history::verified_pair_projections(&prepared),
    );
    let pruning = prune_known_obsolete(&input, &[]).unwrap();
    let links = super::observation::build_completion_links(&originals, &input, &pruning, &prepared)
        .unwrap();
    assert_eq!(links.len(), 1);
    let payload = collect_instruction_candidates(&input, &pruning, &links)
        .unwrap()
        .unwrap();
    let selection = host_selection(
        &payload,
        vec![ProposedOperation::ReplaceCompleted {
            source: "human-a".into(),
            source_text: None,
            evidence: "call-a-output-item".into(),
            result: "The test suite passed.".into(),
        }],
    );

    let application =
        validate_and_apply_selection(&input, &pruning, &links, Some(selection)).unwrap();
    assert_eq!(
        application
            .results
            .iter()
            .map(|result| result.text.as_str())
            .collect::<Vec<_>>(),
        vec!["The test suite passed."]
    );
    let native =
        super::native::filter_retained_instructions(&originals, &input, application).unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();

    assert_eq!(
        human_texts(&candidate),
        vec![(Some("human-b"), "Then update the notes.".to_string())]
    );
    let ids = candidate.iter().filter_map(item_id).collect::<Vec<_>>();
    assert!(!ids.contains(&"call-a-item"));
    assert!(!ids.contains(&"call-a-output-item"));
}

#[test]
fn conservative_checkpoint_advances_work_but_not_instruction_selection() {
    let mut item = checkpoint_summary("Fresh Work summary, selection unresolved.");
    let raw = item
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap();
    raw["selection_mode"] = json!("conservative_selection");
    raw["presentation_hash"] = json!("2".repeat(64));
    let originals = vec![item];
    let adopted = super::summary::find_adopted_v2_checkpoint(&originals, THREAD_ID)
        .unwrap()
        .unwrap();
    let context = super::summary::semantic_context(&originals, Some(&adopted));
    assert_eq!(context.work_boundary, Some(0));
    assert_eq!(context.previous_summary, Some(0));
    assert_eq!(context.selection_boundary, None);
}

fn committed_task_checkpoint_window() -> Vec<codex_history::RolloutItem> {
    let boundary = task_boundary_summary("accepted task boundary");
    let mut capacity = checkpoint_summary("accepted task capacity");
    let capacity_text = item_text(&capacity);
    let mut v2 = codex_history::RenCrowCompactionMetadataV2::parse_and_validate(
        capacity
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_compaction.clone())
            .expect("fixture V2 capacity metadata"),
        THREAD_ID,
        &capacity_text,
    )
    .expect("fixture V2 capacity validates");
    v2.transaction_following_items = Some(0);
    v2.committed_transaction_hash = None;
    capacity.metadata.get_or_insert_default().rencrow_compaction = Some(
        serde_json::to_value(RenCrowTaskCheckpointMetadataV3::capacity(
            &capacity_text,
            Some(stored_task_state_fixture()),
            v2,
        ))
        .expect("fixture V3 capacity metadata"),
    );
    let compacted = codex_history::CompactedItem {
        message: "accepted task capacity".into(),
        replacement_history: Some(vec![boundary, capacity]),
        retained_context: None,
        guardian_history: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: Some("window-1".into()),
        previous_window_id: None,
        window_id: Some("window-1".into()),
        compaction_response_id: Some("capacity-response".into()),
        latest_token_usage_record: None,
    };
    let mut transaction = vec![codex_history::RolloutItem::Compacted(compacted)];
    let checkpoint_hash = codex_history::compaction_transaction::transaction_hash(&transaction)
        .expect("fixture transaction hash");
    transaction.push(codex_history::RolloutItem::RenCrowCompactionCommit { checkpoint_hash });
    transaction
}

#[test]
fn committed_v3_task_checkpoint_proof_uses_the_owner_committed_target() {
    let transaction = committed_task_checkpoint_window();
    let proof = super::inspect_committed_task_checkpoint_window(&transaction, THREAD_ID, "root")
        .expect("committed V3 task checkpoint proof");
    assert_eq!(proof.root_task_id, "root");
    assert_eq!(proof.closed_task_id, "task-a");
    assert_eq!(proof.task_revision, 5);
    assert_eq!(proof.closed_task_revision, 2);
    assert_eq!(proof.task_boundary_index, 0);
    assert_eq!(
        proof.task_boundary_summary_hash,
        content_sha256("accepted task boundary")
    );
    assert_eq!(proof.committed_transaction_hash.len(), 64);
}

#[test]
fn uncommitted_v3_task_checkpoint_window_is_rejected() {
    let mut transaction = committed_task_checkpoint_window();
    transaction.pop();
    assert!(
        super::inspect_committed_task_checkpoint_window(&transaction, THREAD_ID, "root").is_err()
    );
}

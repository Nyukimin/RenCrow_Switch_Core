use super::*;
use crate::compact::SUMMARY_PREFIX;
use crate::context_manager::ContextManager;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;

const THREAD_ID: &str = "thread-candidate-test";

struct CandidateFixture {
    original: ContextManager,
    base: BaseInstructions,
    projection: NativeProjection,
    initial_context: Vec<ResponseItemEnvelope>,
    candidate: Vec<ResponseItemEnvelope>,
}

fn message(role: &str, text: &str, id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn v2_metadata(summary_text: &str) -> Value {
    json!({
        "version": 2,
        "snapshot_hash": "a".repeat(64),
        "summary_hash": codex_history::archive_reference::content_sha256(summary_text),
        "semantic_summary_hash": codex_history::archive_reference::content_sha256(summary_text),
        "selection_mode": "no_candidates",
        "applied_refs": [],
        "results": [],
        "observations": [],
        "summary_covered_observations": [],
        "important_refs": [],
        "model": "candidate-test-model",
        "responses": [{
            "stage": "summary",
            "response_id": "summary-response",
            "seconds": 0.1,
            "usage": null
        }]
    })
}

fn checkpoint_metadata(summary: &ResponseItemEnvelope) -> &Value {
    summary
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        .expect("candidate fixture has checkpoint metadata")
}

fn summary_text(summary: &ResponseItemEnvelope) -> &str {
    let ResponseItem::Message { content, .. } = &summary.item else {
        panic!("candidate summary is a message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("candidate summary is one InputText item");
    };
    text
}

fn fixture(work_bytes: usize, with_initial_context: bool) -> CandidateFixture {
    let work = message(
        "assistant",
        &"old work ".repeat(work_bytes / 10),
        "old-work",
    );

    let mut human = message("user", "retain this request", "human-request");
    let ResponseItem::Message { content, .. } = &mut human.item else {
        unreachable!();
    };
    content.push(ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,AA==".into(),
        },
        detail: None,
    });
    human.metadata.get_or_insert_default().rencrow_input = Some(json!({
        "version": 1,
        "author": "human",
        "thread_id": THREAD_ID,
        "selected_text": "retain this request",
        "receipt_hash": "human-receipt"
    }));

    let pending = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: Some(ResponseItemId::from_server("active-call-item".into())),
        name: "exec_command".into(),
        namespace: None,
        arguments: "{\"cmd\":\"still-running\"}".into(),
        encrypted_function_args: None,
        call_id: "active-call".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let media_output = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("media-output-item".into())),
        call_id: "media-call".into(),
        name: Some("image_tool".into()),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "opaque media evidence".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AQ==".into(),
                },
                detail: None,
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    });

    let mut originals = Vec::new();
    if work_bytes > 0 {
        originals.push(work);
    }
    originals.extend([human, pending, media_output]);

    let input = super::super::history::capture(
        &originals,
        "candidate-binding".into(),
        THREAD_ID,
        vec![],
        &[],
    )
    .unwrap();
    let application = codex_history::compaction_selection::InstructionSelectionApplication {
        pruning: codex_history::compaction_preprocess::prune_known_obsolete(&input, &[]).unwrap(),
        plan_hash: None,
        results: vec![],
    };
    let projection =
        super::super::native::filter_retained_instructions(&originals, &input, application)
            .unwrap();

    let summary_text = format!("{SUMMARY_PREFIX}\nCurrent verified facts.");
    let initial_context = with_initial_context
        .then(|| message("user", "canonical current context", "initial-context"))
        .into_iter()
        .collect::<Vec<_>>();
    let mut candidate = super::super::native::build_native_replacement(
        &projection,
        &summary_text,
        initial_context.clone(),
    )
    .unwrap();
    candidate
        .last_mut()
        .unwrap()
        .metadata
        .get_or_insert_default()
        .rencrow_compaction = Some(v2_metadata(&summary_text));

    let mut original = ContextManager::new();
    original.replace_annotated(originals);

    CandidateFixture {
        original,
        base: BaseInstructions {
            text: "system instructions".into(),
            provenance: None,
        },
        projection,
        initial_context,
        candidate,
    }
}

fn limits(auto_scope: Option<i64>, full: Option<i64>) -> ContextWindowTokenStatus {
    ContextWindowTokenStatus {
        active_context_tokens: 0,
        auto_compact_scope_tokens: 0,
        auto_compact_scope_limit: auto_scope,
        full_context_window_limit: full,
        base_window_tokens_remaining: None,
        auto_compact_window_prefill_tokens: None,
        full_context_window_limit_reached: false,
        token_limit_reached: false,
        turn_end_compaction_threshold_reached: false,
    }
}

fn validate(
    fixture: &CandidateFixture,
    candidate: &[ResponseItemEnvelope],
    scope: AutoCompactTokenLimitScope,
    limits: &ContextWindowTokenStatus,
) -> Result<(), String> {
    validate_compaction_candidate(
        &fixture.original,
        &fixture.base,
        &fixture.projection,
        &fixture.initial_context,
        candidate,
        THREAD_ID,
        scope,
        limits,
    )
}

#[test]
fn v2_candidate_accepts_shrunk_exact_native_projection_initial_context_and_checkpoint() {
    let fixture = fixture(100_000, true);
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_ok()
    );
}

#[test]
fn v2_candidate_rejects_missing_reordered_duplicated_or_changed_retained_native_items() {
    let fixture = fixture(100_000, true);
    let mut missing = fixture.candidate.clone();
    let active_index = missing
        .iter()
        .position(|item| {
            item.item
                .id()
                .is_some_and(|id| id.as_str() == "active-call-item")
        })
        .unwrap();
    missing.remove(active_index);
    assert!(
        validate(
            &fixture,
            &missing,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut reordered = fixture.candidate.clone();
    let human_index = reordered
        .iter()
        .position(|item| {
            item.item
                .id()
                .is_some_and(|id| id.as_str() == "human-request")
        })
        .unwrap();
    reordered.swap(human_index, active_index);
    assert!(
        validate(
            &fixture,
            &reordered,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut duplicated = fixture.candidate.clone();
    duplicated.insert(active_index, duplicated[active_index].clone());
    assert!(
        validate(
            &fixture,
            &duplicated,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut changed_media = fixture.candidate.clone();
    let human = changed_media
        .iter_mut()
        .find(|item| {
            item.item
                .id()
                .is_some_and(|id| id.as_str() == "human-request")
        })
        .unwrap();
    let ResponseItem::Message { content, .. } = &mut human.item else {
        unreachable!();
    };
    let ContentItem::InputImage { image, .. } = &mut content[1] else {
        unreachable!();
    };
    *image = ImageReference::Inline {
        image_url: "data:image/png;base64,changed".into(),
    };
    assert!(
        validate(
            &fixture,
            &changed_media,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_candidate_rejects_initial_context_outside_the_original_helpers_canonical_slot() {
    let fixture = fixture(100_000, true);
    let mut misplaced = fixture.candidate.clone();
    let context_index = misplaced
        .iter()
        .position(|item| {
            item.item
                .id()
                .is_some_and(|id| id.as_str() == "initial-context")
        })
        .unwrap();
    let human_index = misplaced
        .iter()
        .position(|item| {
            item.item
                .id()
                .is_some_and(|id| id.as_str() == "human-request")
        })
        .unwrap();
    misplaced.swap(context_index, human_index);
    assert!(
        validate(
            &fixture,
            &misplaced,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_candidate_rejects_missing_or_mismatched_typed_summary_metadata_and_hash() {
    let fixture = fixture(100_000, false);
    let mut missing_metadata = fixture.candidate.clone();
    missing_metadata.last_mut().unwrap().metadata = None;
    assert!(
        validate(
            &fixture,
            &missing_metadata,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut wrong_summary = fixture.candidate.clone();
    let ResponseItem::Message { content, .. } = &mut wrong_summary.last_mut().unwrap().item else {
        unreachable!();
    };
    let ContentItem::InputText { text } = &mut content[0] else {
        unreachable!();
    };
    *text = format!("{SUMMARY_PREFIX}\nchanged after metadata binding");
    assert!(
        validate(
            &fixture,
            &wrong_summary,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut empty_summary = fixture.candidate.clone();
    let ResponseItem::Message { content, .. } = &mut empty_summary.last_mut().unwrap().item else {
        unreachable!();
    };
    let ContentItem::InputText { text } = &mut content[0] else {
        unreachable!();
    };
    *text = format!("{SUMMARY_PREFIX}\n");
    assert!(
        validate(
            &fixture,
            &empty_summary,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut untyped_summary = fixture.candidate.clone();
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut untyped_summary.last_mut().unwrap().item
    else {
        unreachable!();
    };
    internal_chat_message_metadata_passthrough
        .as_mut()
        .unwrap()
        .content_item_kinds = Some(vec![ContentItemKind("user.text".into())]);
    assert!(
        validate(
            &fixture,
            &untyped_summary,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut wrong_role = fixture.candidate.clone();
    let ResponseItem::Message { role, .. } = &mut wrong_role.last_mut().unwrap().item else {
        unreachable!();
    };
    *role = "assistant".into();
    assert!(
        validate(
            &fixture,
            &wrong_role,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut nonterminal_summary = fixture.candidate.clone();
    let summary = nonterminal_summary.pop().unwrap();
    nonterminal_summary.insert(0, summary);
    assert!(
        validate(
            &fixture,
            &nonterminal_summary,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_candidate_rejects_metadata_already_prepared_or_committed_for_a_transaction() {
    let fixture = fixture(100_000, false);

    let mut prepared = fixture.candidate.clone();
    prepared
        .last_mut()
        .unwrap()
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap()["transaction_following_items"] = json!(0);
    assert!(
        codex_history::RenCrowCompactionMetadataV2::parse_and_validate(
            checkpoint_metadata(prepared.last().unwrap()).clone(),
            THREAD_ID,
            summary_text(prepared.last().unwrap()),
        )
        .is_ok()
    );
    assert!(
        validate(
            &fixture,
            &prepared,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let mut committed = fixture.candidate.clone();
    committed
        .last_mut()
        .unwrap()
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap()["committed_transaction_hash"] = json!("b".repeat(64));
    assert!(
        codex_history::RenCrowCompactionMetadataV2::parse_and_validate(
            checkpoint_metadata(committed.last().unwrap()).clone(),
            THREAD_ID,
            summary_text(committed.last().unwrap()),
        )
        .is_ok()
    );
    assert!(
        validate(
            &fixture,
            &committed,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_candidate_rejects_no_shrink() {
    let fixture = fixture(0, false);
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_candidate_enforces_total_and_fresh_body_after_prefix_limits() {
    let fixture = fixture(100_000, false);
    let over_total_scope = limits(Some(0), Some(i64::MAX));
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::Total,
            &over_total_scope,
        )
        .is_err()
    );

    let fresh_body_window = limits(Some(1), Some(i64::MAX));
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::BodyAfterPrefix,
            &fresh_body_window,
        )
        .is_ok()
    );

    let immediately_retriggering_body_window = limits(Some(0), Some(i64::MAX));
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::BodyAfterPrefix,
            &immediately_retriggering_body_window,
        )
        .is_err()
    );

    let over_full_cap = limits(Some(i64::MAX), Some(0));
    assert!(
        validate(
            &fixture,
            &fixture.candidate,
            AutoCompactTokenLimitScope::BodyAfterPrefix,
            &over_full_cap,
        )
        .is_err()
    );
}

fn preflight(
    fixture: &CandidateFixture,
    scope: AutoCompactTokenLimitScope,
    limits: &ContextWindowTokenStatus,
) -> Result<(), String> {
    preflight_compaction_floor(
        &fixture.original,
        &fixture.base,
        &fixture.projection,
        &fixture.initial_context,
        scope,
        limits,
    )
}

#[test]
fn v2_preflight_rejects_a_floor_that_cannot_shrink_or_fit_before_any_summary() {
    let unshrinkable = fixture(0, false);
    assert!(
        preflight(
            &unshrinkable,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(i64::MAX)),
        )
        .is_err()
    );

    let large = fixture(100_000, false);
    assert!(
        preflight(
            &large,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(i64::MAX), Some(0)),
        )
        .is_err()
    );
    assert!(
        preflight(
            &large,
            AutoCompactTokenLimitScope::Total,
            &limits(Some(0), Some(i64::MAX)),
        )
        .is_err()
    );
}

#[test]
fn v2_preflight_allows_a_viable_floor_while_final_validation_stays_independent() {
    let fixture = fixture(100_000, true);
    let open_limits = limits(Some(i64::MAX), Some(i64::MAX));
    assert_eq!(
        preflight(&fixture, AutoCompactTokenLimitScope::Total, &open_limits),
        Ok(())
    );

    let mut tampered = fixture.candidate.clone();
    checkpoint_metadata_mut(&mut tampered)["summary_hash"] = json!("c".repeat(64));
    assert!(
        validate(
            &fixture,
            &tampered,
            AutoCompactTokenLimitScope::Total,
            &open_limits,
        )
        .is_err()
    );
}

fn checkpoint_metadata_mut(candidate: &mut [ResponseItemEnvelope]) -> &mut Value {
    candidate
        .last_mut()
        .unwrap()
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap()
}

fn human_message(text: &str, id: &str) -> ResponseItemEnvelope {
    let mut human = message("user", text, id);
    human.metadata.get_or_insert_default().rencrow_input = Some(json!({
        "version": 1,
        "author": "human",
        "thread_id": THREAD_ID,
        "selected_text": text,
        "receipt_hash": format!("receipt-{id}"),
    }));
    human
}

fn receipt(stage: CheckpointResponseStage, response_id: &str) -> CompactionModelResponseReceipt {
    CompactionModelResponseReceipt {
        stage,
        response_id: response_id.into(),
        seconds: 0.1,
        usage: None,
    }
}

fn capture_input(
    originals: &[ResponseItemEnvelope],
) -> codex_history::compaction_candidate::CandidateInput {
    super::super::history::capture(
        originals,
        "candidate-binding".into(),
        THREAD_ID,
        vec![],
        &[],
    )
    .unwrap()
}

fn validate_fresh_candidate(
    originals: &[ResponseItemEnvelope],
    application: InstructionSelectionApplication,
    presentation_hash: Option<String>,
    responses: Vec<CompactionModelResponseReceipt>,
    important_refs: Vec<ObservationReference>,
) -> Result<(), String> {
    let input = capture_input(originals);
    let projection =
        super::super::native::filter_retained_instructions(originals, &input, application.clone())?;
    let summary_text = format!("{SUMMARY_PREFIX}\nCurrent verified facts.");
    let metadata = fresh_checkpoint_metadata(
        &summary_text,
        input.snapshot()?.hash().to_owned(),
        presentation_hash,
        &application,
        vec![],
        vec![],
        important_refs,
        responses,
        "candidate-model".into(),
        None,
    );
    let mut candidate =
        super::super::native::build_native_replacement(&projection, &summary_text, vec![])?;
    candidate
        .last_mut()
        .unwrap()
        .metadata
        .get_or_insert_default()
        .rencrow_compaction = Some(serde_json::to_value(&metadata).unwrap());
    let mut original = ContextManager::new();
    original.replace_annotated(originals.to_vec());
    validate_compaction_candidate(
        &original,
        &BaseInstructions {
            text: "system instructions".into(),
            provenance: None,
        },
        &projection,
        &[],
        &candidate,
        THREAD_ID,
        AutoCompactTokenLimitScope::Total,
        &limits(Some(i64::MAX), Some(i64::MAX)),
    )
}

#[test]
fn v2_fresh_metadata_without_selection_passes_final_validation() {
    let originals = vec![
        message("assistant", &"old work ".repeat(2_000), "old-work"),
        human_message("Keep Japanese.", "human-a"),
    ];
    let input = capture_input(&originals);
    let pruning = codex_history::compaction_preprocess::prune_known_obsolete(&input, &[]).unwrap();
    let application = codex_history::compaction_selection::validate_and_apply_selection(
        &input,
        &pruning,
        &[],
        None,
    )
    .unwrap();

    assert_eq!(
        validate_fresh_candidate(
            &originals,
            application.clone(),
            None,
            vec![receipt(
                CheckpointResponseStage::Summary,
                "summary-response"
            )],
            vec![],
        ),
        Ok(())
    );
    // A presentation hash without an instruction-selection request is not recorded.
    assert_eq!(
        validate_fresh_candidate(
            &originals,
            application,
            Some("d".repeat(64)),
            vec![receipt(
                CheckpointResponseStage::Summary,
                "summary-response"
            )],
            vec![],
        ),
        Ok(())
    );
}

#[test]
fn v2_fresh_metadata_with_selection_passes_and_inconsistent_parts_are_rejected() {
    use codex_history::compaction_pipeline::InstructionSelection;
    use codex_history::compaction_pipeline::ProposedOperation;
    use codex_history::compaction_pipeline::ProposedPlan;

    let originals = vec![
        message("assistant", &"old work ".repeat(2_000), "old-work"),
        human_message("Keep Japanese. Use obsolete-label.", "human-a"),
        human_message("Use current-label instead.", "human-b"),
    ];
    let input = capture_input(&originals);
    let pruning = codex_history::compaction_preprocess::prune_known_obsolete(&input, &[]).unwrap();
    let payload =
        codex_history::compaction_preprocess::collect_instruction_candidates(&input, &pruning, &[])
            .unwrap()
            .unwrap();
    let presentation_hash = payload["presentation_hash"].as_str().unwrap().to_owned();
    let selection = InstructionSelection::from_host(
        payload["snapshot_hash"].as_str().unwrap().into(),
        presentation_hash.clone(),
        ProposedPlan {
            operations: vec![ProposedOperation::DropSuperseded {
                source: "human-a".into(),
                source_text: Some("Use obsolete-label.".into()),
                correction: "human-b".into(),
                correction_text: Some("Use current-label instead.".into()),
            }],
        },
    );
    let application = codex_history::compaction_selection::validate_and_apply_selection(
        &input,
        &pruning,
        &[],
        Some(selection),
    )
    .unwrap();
    let both_receipts = vec![
        receipt(
            CheckpointResponseStage::InstructionSelection,
            "selection-response",
        ),
        receipt(CheckpointResponseStage::Summary, "summary-response"),
    ];

    assert_eq!(
        validate_fresh_candidate(
            &originals,
            application.clone(),
            Some(presentation_hash.clone()),
            both_receipts.clone(),
            vec![],
        ),
        Ok(())
    );
    // Missing the selection receipt makes the mode disagree with the applied plan.
    assert!(
        validate_fresh_candidate(
            &originals,
            application.clone(),
            Some(presentation_hash.clone()),
            vec![receipt(
                CheckpointResponseStage::Summary,
                "summary-response"
            )],
            vec![],
        )
        .is_err()
    );
    // An important reference must belong to the checkpoint observation inventory.
    assert!(
        validate_fresh_candidate(
            &originals,
            application,
            Some(presentation_hash),
            both_receipts,
            vec![ObservationReference::new(
                THREAD_ID,
                "unknown-call",
                "e".repeat(64),
            )],
        )
        .is_err()
    );
}

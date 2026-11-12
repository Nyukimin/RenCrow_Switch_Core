// Modified by RenCrow: failures must preserve original instructions and valid witnesses.
use super::*;
use crate::compaction_candidate::CandidateRecord;
use crate::compaction_candidate::Origin;
use crate::compaction_plan::ByteRange;
use crate::compaction_preprocess::collect_instruction_candidates;
use pretty_assertions::assert_eq;

fn fixture() -> (CandidateInput, InstructionPruning, SelectionRequest) {
    let records = [
        ("old", "Keep Japanese. Use old-port."),
        ("new", "Use new-port instead."),
        ("third", "Use final-port instead."),
    ]
    .into_iter()
    .map(|(id, text)| CandidateRecord {
        id: id.into(),
        origin: Origin::Human,
        intake_ref: Some(format!("intake/{id}")),
        scope: "thread".into(),
        role: "user".into(),
        text: text.into(),
        protected: vec![],
        execution_evidence: false,
        opaque: None,
    })
    .collect();
    let input = CandidateInput {
        version: 1,
        binding: "fixture".into(),
        records,
        current_context: vec![],
        prior_invalidations: vec![],
    };
    let pruning = InstructionPruning::default();
    let payload = collect_instruction_candidates(&input, &pruning, &[])
        .unwrap()
        .unwrap();
    let request = SelectionRequest::new(&input, &payload).unwrap();
    (input, pruning, request)
}

fn passage(request: &SelectionRequest, text: &str) -> String {
    request
        .passages
        .iter()
        .find(|(_, p)| p.text == text)
        .unwrap()
        .0
        .clone()
}

fn supersede(request: &SelectionRequest, old: &str, new: &str) -> Value {
    json!({"action":"drop_superseded","source":passage(request,old),"correction":passage(request,new)})
}

#[test]
fn id_decisions_resolve_original_unicode_and_preserve_the_active_part() {
    let (input, pruning, request) = fixture();
    let operation = supersede(&request, "Use old-port.", "Use new-port instead.");
    let report = request
        .apply(
            &input,
            &pruning,
            &[],
            &json!({"operations":[operation]}).to_string(),
        )
        .unwrap();
    assert_eq!(report.rejected, 0);
    assert_eq!(
        report.application.pruning.retained_human_text["old"],
        "Keep Japanese. "
    );
    assert_eq!(report.application.pruning.applied.len(), 1);
    assert_eq!(
        report.application.pruning.applied[0].range,
        ByteRange { start: 15, end: 28 }
    );
}

#[test]
fn invalid_entry_does_not_discard_independent_valid_decisions() {
    let (input, pruning, request) = fixture();
    let valid = supersede(&request, "Use old-port.", "Use new-port instead.");
    let invalid = json!({"action":"drop_superseded","source":"invented","correction":"invented"});
    let report = request
        .apply(
            &input,
            &pruning,
            &[],
            &json!({"operations":[invalid,valid,{"action":"drop_everything"}]}).to_string(),
        )
        .unwrap();
    assert_eq!(report.rejected, 2);
    assert_eq!(report.application.pruning.applied.len(), 1);
    assert_eq!(
        report.application.pruning.retained_human_text["old"],
        "Keep Japanese. "
    );
}

#[test]
fn later_operation_cannot_remove_an_already_accepted_correction_witness() {
    let (input, pruning, request) = fixture();
    let operations = [
        supersede(&request, "Use old-port.", "Use new-port instead."),
        supersede(&request, "Use new-port instead.", "Use final-port instead."),
    ];
    let report = request
        .apply(
            &input,
            &pruning,
            &[],
            &json!({"operations":operations}).to_string(),
        )
        .unwrap();
    assert_eq!(report.rejected, 1);
    assert_eq!(report.application.pruning.applied.len(), 1);
    assert!(
        !report
            .application
            .pruning
            .retained_human_text
            .contains_key("new")
    );
}

#[test]
fn truncated_json_is_not_completed_and_fences_do_not_create_meaning() {
    let (input, pruning, request) = fixture();
    for text in [
        "{\"operations\":[",
        "{\"operations\":[{\"action\":\"drop_superseded",
        "{\"operations\":[]} trailing",
        "{\"operations\":[],\"operations\":[]}",
    ] {
        assert!(
            request.apply(&input, &pruning, &[], text).is_err(),
            "{text}"
        );
    }
    let report = request
        .apply(&input, &pruning, &[], "```json\n{\"operations\":[]}\n```")
        .unwrap();
    assert!(report.normalized_fence);
    assert_eq!(report.application.pruning, pruning);
}

#[test]
fn model_cannot_reintroduce_copied_text_or_duplicate_a_decision_field() {
    let (input, pruning, request) = fixture();
    let source = passage(&request, "Use old-port.");
    let correction = passage(&request, "Use new-port instead.");
    let duplicate = format!(
        "{{\"operations\":[{{\"action\":\"drop_superseded\",\"source\":\"{source}\",\"source\":\"{source}\",\"correction\":\"{correction}\"}}]}}"
    );
    let mut copied = supersede(&request, "Use old-port.", "Use new-port instead.");
    copied["source_text"] = json!("model's paraphrase");
    for text in [duplicate, json!({"operations":[copied]}).to_string()] {
        let report = request.apply(&input, &pruning, &[], &text).unwrap();
        assert_eq!(report.rejected, 1);
        assert_eq!(report.application.pruning, pruning);
    }
}

#[test]
fn stale_passage_ids_cannot_be_applied_to_a_new_snapshot() {
    let (mut input, pruning, request) = fixture();
    let operation = supersede(&request, "Use old-port.", "Use new-port instead.");
    input.binding = "different-checkpoint".into();
    let payload = collect_instruction_candidates(&input, &pruning, &[])
        .unwrap()
        .unwrap();
    let next = SelectionRequest::new(&input, &payload).unwrap();
    let report = next
        .apply(
            &input,
            &pruning,
            &[],
            &json!({"operations":[operation]}).to_string(),
        )
        .unwrap();
    assert_eq!(report.rejected, 1);
    assert_eq!(report.application.pruning, pruning);
}

#[test]
fn protected_and_repeated_passages_are_not_removal_choices() {
    let (mut input, pruning, _) = fixture();
    input.records[0].protected = vec![ByteRange { start: 0, end: 14 }];
    input.records[1].text = "同じ条件。同じ条件。".into();
    let payload = collect_instruction_candidates(&input, &pruning, &[])
        .unwrap()
        .unwrap();
    let request = SelectionRequest::new(&input, &payload).unwrap();
    assert!(
        !request
            .passages
            .values()
            .any(|p| p.text == "Keep Japanese." && p.removable)
    );
    assert!(!request.passages.values().any(|p| p.text == "同じ条件。"));
    assert!(
        request
            .passages
            .values()
            .any(|p| p.text == "Use old-port." && p.removable)
    );
}

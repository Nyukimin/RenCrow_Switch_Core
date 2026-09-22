use super::*;

fn input() -> CandidateInput {
    CandidateInput {
        version: 1,
        binding: "test/session/turn".into(),
        current_context: vec![serde_json::json!({"role":"developer","content":"Current rules"})],
        records: vec![
            CandidateRecord {
                id: "u1".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/1".into()),
                scope: "test".into(),
                role: "user".into(),
                text: "Use old plan".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "u2".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/2".into()),
                scope: "test".into(),
                role: "user".into(),
                text: "Cancel old plan; use new plan".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "work".into(),
                origin: Origin::Work,
                intake_ref: None,
                scope: "test".into(),
                role: "assistant".into(),
                text: "Investigation result".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "unknown".into(),
                origin: Origin::Unknown,
                intake_ref: None,
                scope: "test".into(),
                role: "user".into(),
                text: "Unattributed data".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: Some(serde_json::json!({"media":"reference"})),
            },
        ],
    }
}

fn bundle(input: &CandidateInput) -> CandidateBundle {
    let snapshot = input.snapshot().unwrap();
    let source = |i: usize| {
        snapshot
            .reference(
                &input.records[i].id,
                ByteRange {
                    start: 0,
                    end: input.records[i].text.len(),
                },
            )
            .unwrap()
    };
    let plan = CompactionPlan {
        schema_version: 1,
        snapshot_hash: snapshot.hash().into(),
        operations: vec![Operation::DropSuperseded {
            source: source(0),
            correction: source(1),
        }],
    };
    let review = SemanticReview {
        plan_hash: plan.hash().unwrap(),
        accepted_operations: vec![0],
    };
    let view = input.view(&plan, &review).unwrap();
    let summary = WorkSummary {
        view_hash: digest(&view).unwrap(),
        text: "Investigation result; continue new plan".into(),
        source_ids: vec!["work".into()],
    };
    let summary_review = SummaryReview {
        summary_hash: digest(&summary).unwrap(),
        accepted: true,
    };
    CandidateBundle {
        version: 1,
        input_hash: digest(input).unwrap(),
        plan,
        plan_review: review,
        summary,
        summary_review,
        model: "worker".into(),
        effort: "high".into(),
        responses: vec![],
    }
}

#[test]
fn builds_separate_inputs_without_reviving_removed_human_text() {
    let input = input();
    let before = serde_json::to_value(&input).unwrap();
    let selected = input.assemble(&bundle(&input)).unwrap();
    assert_eq!(selected["human_input"].as_array().unwrap().len(), 1);
    assert_eq!(selected["human_input"][0]["id"], "u2");
    assert_eq!(selected["protected"][0]["id"], "unknown");
    assert_eq!(
        selected["protected"][0]["opaque"],
        serde_json::json!({"media":"reference"})
    );
    assert_eq!(selected["current_context"], before["current_context"]);
    assert_eq!(serde_json::to_value(&input).unwrap(), before);
}

#[test]
fn user_role_alone_does_not_become_human() {
    let mut input = input();
    input.records[0].origin = Origin::Unknown;
    let data = input.proposal_input().unwrap();
    assert_eq!(data["sources"][0]["origin"], "unknown");
    input.records[0].origin = Origin::Human;
    input.records[0].intake_ref = None;
    assert!(input.snapshot().is_err());
    input.records[0].intake_ref = Some("receipt".into());
    input.records[0].role = "assistant".into();
    assert!(input.snapshot().is_err());
}

#[test]
fn stale_input_and_summary_are_rejected() {
    let mut input = input();
    let mut bundle = bundle(&input);
    input.records[1].text.push_str(" another constraint");
    assert!(input.assemble(&bundle).is_err());
    let input = super::tests::input();
    bundle.input_hash = digest(&input).unwrap();
    bundle.summary.text = "tampered".into();
    assert!(input.assemble(&bundle).is_err());
}

#[test]
fn lost_work_coverage_and_rejected_summary_fail() {
    let input = input();
    let mut bundle = bundle(&input);
    bundle.summary.source_ids.clear();
    bundle.summary_review.summary_hash = digest(&bundle.summary).unwrap();
    assert!(input.assemble(&bundle).is_err());
    let mut bundle = super::tests::bundle(&input);
    bundle.summary_review.accepted = false;
    assert!(input.assemble(&bundle).is_err());
}

#[test]
fn attachments_and_protected_instructions_cannot_be_dropped() {
    let mut input = input();
    input.records[0].opaque = Some(serde_json::json!({"image":"original"}));
    let snapshot = input.snapshot().unwrap();
    let plan = CompactionPlan {
        schema_version: 1,
        snapshot_hash: snapshot.hash().into(),
        operations: vec![Operation::DropSuperseded {
            source: snapshot
                .reference(
                    "u1",
                    ByteRange {
                        start: 0,
                        end: input.records[0].text.len(),
                    },
                )
                .unwrap(),
            correction: snapshot
                .reference(
                    "u2",
                    ByteRange {
                        start: 0,
                        end: input.records[1].text.len(),
                    },
                )
                .unwrap(),
        }],
    };
    let review = SemanticReview {
        plan_hash: plan.hash().unwrap(),
        accepted_operations: vec![0],
    };
    assert!(input.view(&plan, &review).is_err());
}

#[test]
fn old_host_context_requires_current_owner_context() {
    let mut data = input();
    data.records[0].origin = Origin::Host;
    data.current_context.clear();
    assert!(data.snapshot().is_err());
}

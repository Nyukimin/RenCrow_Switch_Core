use super::*;
use crate::compaction_candidate::CandidateRecord;
use crate::compaction_candidate::Origin;

fn input() -> CandidateInput {
    CandidateInput {
        version: 1,
        binding: "fixture".into(),
        current_context: vec![],
        prior_invalidations: vec![],
        records: vec![
            CandidateRecord {
                id: "old".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/old".into()),
                scope: "test".into(),
                role: "user".into(),
                text: "instruction old".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "current".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/current".into()),
                scope: "test".into(),
                role: "user".into(),
                text: "instruction current".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
        ],
    }
}

#[test]
fn id_only_proposal_binds_to_snapshot_and_rejects_unknown_ids() {
    let input = input();
    let proposed = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "old",
            "correction": "current"
        }]
    }))
    .unwrap();
    let plan = bind_plan(&input, proposed).unwrap();
    let view = input
        .view(
            &plan,
            &crate::compaction_plan::SemanticReview {
                plan_hash: plan.hash().unwrap(),
                accepted_operations: vec![0],
            },
        )
        .unwrap();
    assert!(view.retained[0].text.is_empty());
    assert_eq!(view.retained[1].text, "instruction current");

    let invalid = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "fabricated",
            "correction": "current"
        }]
    }))
    .unwrap();
    assert!(bind_plan(&input, invalid).is_err());
}

#[test]
fn drop_superseded_binds_exact_unique_correction_passage() {
    let input = input();
    let proposed: ProposedPlan = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "old",
            "correction": "current",
            "correction_text": "current"
        }]
    }))
    .expect("V2 correction passage is part of the proposal contract");

    let plan = bind_plan(&input, proposed).unwrap();
    let Operation::DropSuperseded { correction, .. } = &plan.operations[0] else {
        panic!("expected drop_superseded");
    };
    assert_eq!(correction.id, "current");
    assert_eq!(
        correction.range,
        ByteRange {
            start: "instruction ".len(),
            end: "instruction current".len(),
        }
    );
}

#[test]
fn drop_superseded_legacy_missing_correction_text_keeps_whole_record_binding() {
    let input = input();
    let proposed: ProposedPlan = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "old",
            "correction": "current"
        }]
    }))
    .expect("legacy proposals omit correction_text");

    let plan = bind_plan(&input, proposed).unwrap();
    let Operation::DropSuperseded { correction, .. } = &plan.operations[0] else {
        panic!("expected drop_superseded");
    };
    assert_eq!(
        correction.range,
        ByteRange {
            start: 0,
            end: "instruction current".len(),
        }
    );
}

#[test]
fn drop_superseded_rejects_ambiguous_correction_text() {
    let mut input = input();
    input.records[1].text = "current; current".into();
    let proposed: ProposedPlan = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "old",
            "correction": "current",
            "correction_text": "current"
        }]
    }))
    .unwrap();

    assert!(bind_plan(&input, proposed).is_err());
}

#[test]
fn passage_resolution_handles_unicode_and_rejects_ambiguity() {
    assert_eq!(
        selected_range("前文。旧指示。続行。", Some("旧指示。")).unwrap(),
        ByteRange {
            start: "前文。".len(),
            end: "前文。旧指示。".len()
        }
    );
    for (text, passage) in [
        ("aaaa", "aaa"),
        ("同じ。同じ。", "同じ。"),
        ("原文", "変更文"),
        ("原文", ""),
    ] {
        assert!(selected_range(text, Some(passage)).is_err());
    }
}

#[test]
fn mixed_record_pruning_keeps_active_text_and_protected_spans() {
    let active = "認証を維持する。";
    let old = "毎回全体テストする。";
    let done = "試験用directoryを確認する。";
    let input = CandidateInput {
        version: 1,
        binding: "mixed".into(),
        current_context: vec![],
        prior_invalidations: vec![],
        records: vec![
            CandidateRecord {
                id: "mixed".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/mixed".into()),
                scope: "fixture".into(),
                role: "user".into(),
                text: format!("{active}\n{old}\n{done}"),
                protected: vec![ByteRange {
                    start: 0,
                    end: active.len(),
                }],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "correction".into(),
                origin: Origin::Human,
                intake_ref: Some("intake/correction".into()),
                scope: "fixture".into(),
                role: "user".into(),
                text: "毎回全体テストの指示は撤回。必要な試験だけ実行。".into(),
                protected: vec![],
                execution_evidence: false,
                opaque: None,
            },
            CandidateRecord {
                id: "receipt".into(),
                origin: Origin::Work,
                intake_ref: None,
                scope: "fixture".into(),
                role: "tool".into(),
                text: "directory exists; exit 0".into(),
                protected: vec![],
                execution_evidence: true,
                opaque: None,
            },
        ],
    };
    let proposed = serde_json::from_value(serde_json::json!({
        "operations": [
            {
                "action": "drop_superseded",
                "source": "mixed",
                "source_text": old,
                "correction": "correction"
            },
            {
                "action": "replace_completed",
                "source": "mixed",
                "source_text": done,
                "evidence": "receipt",
                "result": "directory exists"
            }
        ]
    }))
    .unwrap();
    let plan = bind_plan(&input, proposed).unwrap();
    let view = input
        .view(
            &plan,
            &crate::compaction_plan::SemanticReview {
                plan_hash: plan.hash().unwrap(),
                accepted_operations: vec![0, 1],
            },
        )
        .unwrap();
    assert_eq!(view.retained[0].text, format!("{active}\n\n"));
    assert_eq!(view.results.len(), 1);
    assert_eq!(view.results[0].text, "directory exists");

    let invalid = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "mixed",
            "source_text": active,
            "correction": "correction"
        }]
    }))
    .unwrap();
    let invalid = bind_plan(&input, invalid).unwrap();
    assert!(
        input
            .view(
                &invalid,
                &crate::compaction_plan::SemanticReview {
                    plan_hash: invalid.hash().unwrap(),
                    accepted_operations: vec![],
                },
            )
            .is_err()
    );
}

#[test]
fn stage_payloads_keep_source_text_and_operation_order() {
    let input = input();
    let proposed = serde_json::from_value(serde_json::json!({
        "operations": [{
            "action": "drop_superseded",
            "source": "old",
            "correction": "current"
        }]
    }))
    .unwrap();
    let plan = bind_plan(&input, proposed).unwrap();
    assert_eq!(
        sources(&input),
        serde_json::json!({
            "sources": [
                {
                    "id": "old",
                    "origin": "human",
                    "scope": "test",
                    "text": "instruction old",
                    "protected": [],
                    "has_opaque": false,
                    "execution_evidence": false
                },
                {
                    "id": "current",
                    "origin": "human",
                    "scope": "test",
                    "text": "instruction current",
                    "protected": [],
                    "has_opaque": false,
                    "execution_evidence": false
                }
            ]
        })
    );
    assert_eq!(
        review_input(&input, &plan).unwrap(),
        serde_json::json!({
            "sources": sources(&input),
            "plan": plan,
            "selected_text_by_operation": [{"source": "old", "text": "instruction old"}]
        })
    );
}

#[test]
fn non_human_history_has_a_deterministic_empty_plan_without_deleting_records() {
    for origin in [Origin::Unknown, Origin::Work, Origin::Host] {
        let mut input = input();
        input.current_context = vec![serde_json::json!({"current":"owner rules"})];
        for record in &mut input.records {
            record.origin = origin.clone();
            record.intake_ref = None;
        }
        assert!(!requires_plan_inference(&input));
        let plan = bind_plan(&input, ProposedPlan { operations: vec![] }).unwrap();
        let review = crate::compaction_plan::SemanticReview {
            plan_hash: plan.hash().unwrap(),
            accepted_operations: vec![],
        };
        let view = input.view(&plan, &review).unwrap();
        assert!(view.applied_operations.is_empty());
        assert_eq!(view.retained.len(), input.records.len());
        for (retained, original) in view.retained.iter().zip(&input.records) {
            assert_eq!(retained.text, original.text);
        }
    }
    assert!(requires_plan_inference(&input()));
    let mut mixed = input();
    mixed.records[0].origin = Origin::Unknown;
    mixed.records[0].intake_ref = None;
    assert!(requires_plan_inference(&mixed));
}

#[test]
fn summary_generation_uses_host_bound_work_inventory_without_hidden_payload() {
    let mut input = input();
    let hidden_text = "protected result payload".repeat(80);
    input.records.push(CandidateRecord {
        id: "protected-work".into(),
        origin: Origin::Work,
        intake_ref: None,
        scope: "test".into(),
        role: "tool".into(),
        text: hidden_text.clone(),
        protected: vec![],
        execution_evidence: false,
        opaque: Some(serde_json::json!({"reference":"owner-held"})),
    });
    let plan = bind_plan(&input, ProposedPlan { operations: vec![] }).unwrap();
    let view = input
        .view(
            &plan,
            &crate::compaction_plan::SemanticReview {
                plan_hash: plan.hash().unwrap(),
                accepted_operations: vec![],
            },
        )
        .unwrap();
    let generation = input.summary_input(&view, &plan).unwrap();
    let generation_text = generation.to_string();
    assert!(generation.get("work_source_ids").is_none());
    assert_eq!(
        generation["separately_retained_work"],
        serde_json::json!(["protected-work"])
    );
    assert!(!generation_text.contains(&hidden_text));

    let proposed: ProposedSummary = serde_json::from_value(serde_json::json!({
        "text": "Keep the current result and remaining work."
    }))
    .unwrap();
    let bound = input.bind_summary(&view, proposed.text).unwrap();
    assert_eq!(bound.source_ids, vec!["protected-work"]);
    assert!(
        serde_json::from_value::<ProposedSummary>(serde_json::json!({
            "text": "summary",
            "source_ids": ["protected-work"]
        }))
        .is_err()
    );
}

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
        prior_invalidations: Vec::new(),
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
    let summary = input
        .bind_summary(&view, "Investigation result; continue new plan".into())
        .unwrap();
    let summary_hash = digest(&summary).unwrap();
    CandidateBundle {
        version: 2,
        input_hash: digest(input).unwrap(),
        plan,
        plan_review: review,
        summary,
        summary_hash: Some(summary_hash),
        summary_review: None,
        model: "worker".into(),
        effort: "high".into(),
        responses: vec![],
    }
}

fn v1_bundle(input: &CandidateInput) -> CandidateBundle {
    let mut bundle = bundle(input);
    bundle.version = 1;
    bundle.summary_hash = None;
    bundle.summary_review = Some(SummaryReview {
        summary_hash: digest(&bundle.summary).unwrap(),
        accepted: true,
    });
    bundle
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
    assert_eq!(
        input.assemble(&bundle).unwrap_err(),
        "summary validation failed"
    );
}

#[test]
fn lost_work_coverage_and_rejected_summary_fail() {
    let input = input();
    let mut bundle = bundle(&input);
    bundle.summary.source_ids.clear();
    bundle.summary_hash = Some(digest(&bundle.summary).unwrap());
    assert!(
        input
            .assemble(&bundle)
            .unwrap_err()
            .contains("coverage mismatch")
    );
    let mut bundle = v1_bundle(&input);
    bundle.summary_review.as_mut().unwrap().accepted = false;
    assert!(input.assemble(&bundle).is_err());
}

#[test]
fn versioned_review_receipts_preserve_v1_and_make_v2_absence_explicit() {
    let input = input();
    let current = bundle(&input);
    let current_json = serde_json::to_value(&current).unwrap();
    assert_eq!(current_json["version"], 2);
    assert!(current_json["summary_hash"].is_string());
    assert!(current_json["summary_review"].is_null());
    let decoded: CandidateBundle = serde_json::from_value(current_json).unwrap();
    assert!(decoded.summary_review.is_none());
    assert!(input.assemble(&decoded).is_ok());

    let legacy = v1_bundle(&input);
    let legacy_json = serde_json::to_value(&legacy).unwrap();
    assert!(legacy_json.get("summary_hash").is_none());
    let decoded: CandidateBundle = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(decoded.version, 1);
    assert!(decoded.summary_review.as_ref().unwrap().accepted);
    assert!(input.assemble(&decoded).is_ok());

    let mut reviewed_v2 = current.clone();
    reviewed_v2.summary_review = Some(SummaryReview {
        summary_hash: digest(&reviewed_v2.summary).unwrap(),
        accepted: true,
    });
    assert!(input.assemble(&reviewed_v2).is_ok());
    reviewed_v2.summary_review.as_mut().unwrap().summary_hash = "stale".into();
    assert!(input.assemble(&reviewed_v2).is_err());
    reviewed_v2.summary_review.as_mut().unwrap().summary_hash =
        digest(&reviewed_v2.summary).unwrap();
    reviewed_v2.summary_review.as_mut().unwrap().accepted = false;
    assert!(input.assemble(&reviewed_v2).is_err());

    let mut missing_summary_hash = current.clone();
    missing_summary_hash.summary_hash = None;
    assert_eq!(
        input.assemble(&missing_summary_hash).unwrap_err(),
        "summary validation failed"
    );
    let mut missing_summary_hash_json = serde_json::to_value(current.clone()).unwrap();
    missing_summary_hash_json
        .as_object_mut()
        .unwrap()
        .remove("summary_hash");
    let missing_summary_hash: CandidateBundle =
        serde_json::from_value(missing_summary_hash_json).unwrap();
    assert_eq!(
        input.assemble(&missing_summary_hash).unwrap_err(),
        "summary validation failed"
    );
    let mut stale_summary_hash = current.clone();
    stale_summary_hash.summary_hash = Some("stale".into());
    assert_eq!(
        input.assemble(&stale_summary_hash).unwrap_err(),
        "summary validation failed"
    );

    let mut missing = legacy.clone();
    missing.summary_review = None;
    assert!(input.assemble(&missing).is_err());

    let mut stale_review = legacy;
    stale_review.summary_review.as_mut().unwrap().summary_hash = "stale".into();
    assert!(input.assemble(&stale_review).is_err());

    let mut optional_v1_hash = v1_bundle(&input);
    optional_v1_hash.summary_hash = Some(digest(&optional_v1_hash.summary).unwrap());
    assert!(input.assemble(&optional_v1_hash).is_ok());
    optional_v1_hash.summary_hash = Some("stale".into());
    assert_eq!(
        input.assemble(&optional_v1_hash).unwrap_err(),
        "summary validation failed"
    );
}

#[test]
fn summary_view_hash_is_checked_before_assembly() {
    let input = input();
    let mut bundle = bundle(&input);
    bundle.summary.view_hash = "stale".into();
    bundle.summary_hash = Some(digest(&bundle.summary).unwrap());
    assert_eq!(
        input.assemble(&bundle).unwrap_err(),
        "summary validation failed"
    );
}

#[test]
fn summary_binding_rejects_stale_views_and_unbounded_text() {
    let input = input();
    let bundle = bundle(&input);
    let view = input.view(&bundle.plan, &bundle.plan_review).unwrap();
    let mut stale = view.clone();
    stale.snapshot_hash = "stale".into();
    assert!(input.bind_summary(&stale, "summary".into()).is_err());
    assert!(input.bind_summary(&view, "  \n".into()).is_err());
    assert!(
        input
            .bind_summary(&view, "x".repeat(SUMMARY_TEXT_MAX_BYTES + 1))
            .is_err()
    );
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

#[test]
fn summary_input_removes_prior_invalidations_from_work_but_keeps_other_facts() {
    let mut input = input();
    input.records[0].text = "Keep current plan".into();
    input.prior_invalidations = vec!["Use old plan".into()];
    input.records[2].text = "Prior summary: Use old plan. Keep evidence artifact-42.".into();
    let snapshot = input.snapshot().unwrap();
    let plan = CompactionPlan {
        schema_version: 1,
        snapshot_hash: snapshot.hash().into(),
        operations: vec![],
    };
    let review = SemanticReview {
        plan_hash: plan.hash().unwrap(),
        accepted_operations: vec![],
    };
    let view = input.view(&plan, &review).unwrap();
    let summary = input.summary_input(&view, &plan).unwrap();
    let work = summary["view"]["retained"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fragment| fragment["id"] == "work")
        .unwrap();
    let text = work["text"].as_str().unwrap();
    assert!(!text.contains("Use old plan"));
    assert!(text.contains("Keep evidence artifact-42."));
    assert!(summary.get("negative_validation").is_none());
    assert!(!summary.to_string().contains("Use old plan"));
    let proposed = WorkSummary {
        view_hash: digest(&view).unwrap(),
        text: "Keep evidence artifact-42.".into(),
        source_ids: vec!["work".into()],
    };
    let review_input = input.summary_review_input(&view, &plan, &proposed).unwrap();
    assert_eq!(review_input["input"], summary);
    assert_eq!(
        review_input["negative_validation"]["passages"],
        serde_json::json!(["Use old plan"])
    );
}

#[test]
fn assemble_rejects_summary_reintroducing_current_or_prior_invalidations() {
    let input = input();
    let mut bundle = bundle(&input);
    bundle.summary.text = "Investigation result; Use old plan".into();
    bundle.summary_hash = Some(digest(&bundle.summary).unwrap());
    assert!(input.assemble(&bundle).is_err());

    let mut carried = input;
    carried.prior_invalidations = vec!["Prior withdrawn command".into()];
    carried.records[2].text = "Prior summary: Prior withdrawn command; useful fact.".into();
    let snapshot = carried.snapshot().unwrap();
    let plan = CompactionPlan {
        schema_version: 1,
        snapshot_hash: snapshot.hash().into(),
        operations: vec![],
    };
    let review = SemanticReview {
        plan_hash: plan.hash().unwrap(),
        accepted_operations: vec![],
    };
    let view = carried.view(&plan, &review).unwrap();
    let mut next = CandidateBundle {
        version: 2,
        input_hash: digest(&carried).unwrap(),
        plan,
        plan_review: review,
        summary: WorkSummary {
            view_hash: digest(&view).unwrap(),
            text: "Prior withdrawn command; useful fact.".into(),
            source_ids: vec!["work".into()],
        },
        summary_hash: None,
        summary_review: None,
        model: "worker".into(),
        effort: "high".into(),
        responses: vec![],
    };
    next.summary_hash = Some(digest(&next.summary).unwrap());
    assert!(
        carried
            .assemble(&next)
            .unwrap_err()
            .contains("reintroduces an invalidated source passage")
    );
}

#[test]
fn invalidations_deduplicate_and_reject_empty_entries() {
    let mut input = input();
    input.prior_invalidations = vec!["Use old plan".into(), "Use old plan".into()];
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
    let view = input.view(&plan, &review).unwrap();
    assert_eq!(
        input.invalidations(&plan, &view).unwrap(),
        vec!["Use old plan"]
    );
    input.prior_invalidations = vec![String::new()];
    assert!(input.invalidations(&plan, &view).is_err());
}

#[test]
fn verbatim_work_is_not_duplicated_into_summary_or_review() {
    let mut input = input();
    let mut tool = input.records[2].clone();
    tool.id = "protected-tool".into();
    tool.text = "UNIQUE_TOOL_PAYLOAD_8732".repeat(1000);
    tool.opaque = Some(serde_json::json!({"tool_result":"preserve"}));
    tool.execution_evidence = true;
    input.records.push(tool.clone());
    let mut bundle = bundle(&input);
    assert!(bundle.summary.source_ids.contains(&tool.id));
    let view = input.view(&bundle.plan, &bundle.plan_review).unwrap();
    let original_view = digest(&view).unwrap();
    let generation = input.summary_input(&view, &bundle.plan).unwrap();
    let review = input
        .summary_review_input(&view, &bundle.plan, &bundle.summary)
        .unwrap();
    assert!(!generation.to_string().contains("UNIQUE_TOOL_PAYLOAD_8732"));
    assert!(!review.to_string().contains("UNIQUE_TOOL_PAYLOAD_8732"));
    assert_eq!(
        generation["separately_retained_work"],
        serde_json::json!([tool.id])
    );
    assert!(generation.get("work_source_ids").is_none());
    assert!(generation.to_string().contains("separately_retained_work"));
    assert!(!generation.to_string().contains("text_hash"));
    assert!(!generation.to_string().contains("text_bytes"));
    assert_eq!(digest(&view).unwrap(), original_view);
    assert_eq!(view.retained.last().unwrap().text, tool.text);
    let assembled = input.assemble(&bundle).unwrap();
    assert_eq!(assembled["protected"][1]["text"], tool.text);
    assert_eq!(assembled["protected"][1]["opaque"], tool.opaque.unwrap());
    bundle.summary.source_ids.pop();
    bundle.summary_hash = Some(digest(&bundle.summary).unwrap());
    assert!(input.assemble(&bundle).unwrap_err().contains("coverage"));
}

#[test]
fn completion_evidence_is_still_visible_when_its_original_is_retained() {
    let mut input = input();
    let mut tool = input.records[2].clone();
    tool.id = "completion-tool".into();
    tool.text = "COMPLETION_EVIDENCE_9284 WITHDRAWN_9284".into();
    input.prior_invalidations.push("WITHDRAWN_9284".into());
    tool.opaque = Some(serde_json::json!({"result":"original"}));
    tool.execution_evidence = true;
    input.records.push(tool.clone());
    let snapshot = input.snapshot().unwrap();
    let reference = |record: &CandidateRecord| {
        snapshot
            .reference(
                &record.id,
                ByteRange {
                    start: 0,
                    end: record.text.len(),
                },
            )
            .unwrap()
    };
    let plan = CompactionPlan {
        schema_version: 1,
        snapshot_hash: snapshot.hash().into(),
        operations: vec![Operation::ReplaceCompleted {
            source: reference(&input.records[0]),
            evidence: reference(&tool),
            result: "Recorded completion".into(),
        }],
    };
    let review = SemanticReview {
        plan_hash: plan.hash().unwrap(),
        accepted_operations: vec![0],
    };
    let view = input.view(&plan, &review).unwrap();
    let data = input.summary_input(&view, &plan).unwrap();
    assert!(data.to_string().contains("COMPLETION_EVIDENCE_9284"));
    assert_eq!(data["separately_retained_work"], serde_json::json!([]));
    assert!(!data.to_string().contains("WITHDRAWN_9284"));
    let summary = WorkSummary {
        view_hash: digest(&view).unwrap(),
        text: "Recorded completion".into(),
        source_ids: vec!["work".into(), tool.id.clone()],
    };
    let review_data = input.summary_review_input(&view, &plan, &summary).unwrap();
    assert_eq!(review_data["completion_evidence"][0]["text"], tool.text);
}

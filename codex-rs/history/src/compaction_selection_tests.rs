use super::*;
use crate::archive_reference::ArchiveReference;
use crate::archive_reference::ArchiveTerminalStatus;
use crate::archive_reference::content_sha256;
use crate::compaction_candidate::CandidateInput;
use crate::compaction_candidate::CandidateRecord;
use crate::compaction_candidate::Origin;
use crate::compaction_pipeline::InstructionSelection;
use crate::compaction_pipeline::ProposedOperation;
use crate::compaction_pipeline::ProposedPlan;
use crate::compaction_plan::ByteRange;
use crate::compaction_plan::SourceRef;
use crate::compaction_preprocess::InstructionObservationLink;
use crate::compaction_preprocess::InstructionPruning;
use crate::compaction_preprocess::collect_instruction_candidates;
use crate::compaction_preprocess::prune_known_obsolete;

fn record(id: &str, origin: Origin, role: &str, text: &str) -> CandidateRecord {
    let is_human = matches!(&origin, Origin::Human);
    CandidateRecord {
        id: id.into(),
        origin,
        intake_ref: is_human.then(|| format!("intake/{id}")),
        scope: "thread".into(),
        role: role.into(),
        text: text.into(),
        protected: vec![],
        execution_evidence: false,
        opaque: None,
    }
}

fn input(records: Vec<CandidateRecord>) -> CandidateInput {
    CandidateInput {
        version: 1,
        binding: "thread-1/turn-1/checkpoint-1".into(),
        records,
        current_context: vec![],
        prior_invalidations: vec!["legacy bare text is not authoritative".into()],
    }
}

fn source_ref(input: &CandidateInput, id: &str, passage: &str) -> SourceRef {
    let record = input.records.iter().find(|r| r.id == id).unwrap();
    let start = record.text.find(passage).unwrap();
    input
        .snapshot()
        .unwrap()
        .reference(
            id,
            ByteRange {
                start,
                end: start + passage.len(),
            },
        )
        .unwrap()
}

fn full_ref(input: &CandidateInput, id: &str) -> SourceRef {
    let record = input.records.iter().find(|r| r.id == id).unwrap();
    input
        .snapshot()
        .unwrap()
        .reference(
            id,
            ByteRange {
                start: 0,
                end: record.text.len(),
            },
        )
        .unwrap()
}

fn link(
    input: &CandidateInput,
    human: &str,
    call: &str,
    output: &str,
    call_id: &str,
) -> InstructionObservationLink {
    let output_record = input.records.iter().find(|r| r.id == output).unwrap();
    InstructionObservationLink {
        instruction: full_ref(input, human),
        call: full_ref(input, call),
        output: full_ref(input, output),
        terminal: ArchiveReference::new(
            "thread-1",
            call_id,
            content_sha256(&output_record.text),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        ),
    }
}

fn bound_selection(
    input: &CandidateInput,
    pruning: &InstructionPruning,
    links: &[InstructionObservationLink],
    proposed: ProposedPlan,
) -> InstructionSelection {
    let payload = collect_instruction_candidates(input, pruning, links)
        .unwrap()
        .expect("fixture has an eligible Human candidate");
    InstructionSelection::from_host(
        payload["snapshot_hash"].as_str().unwrap().to_owned(),
        payload["presentation_hash"].as_str().unwrap().to_owned(),
        proposed,
    )
}

fn drop(
    source: &str,
    source_text: Option<&str>,
    correction: &str,
    correction_text: Option<&str>,
) -> ProposedOperation {
    ProposedOperation::DropSuperseded {
        source: source.into(),
        source_text: source_text.map(str::to_owned),
        correction: correction.into(),
        correction_text: correction_text.map(str::to_owned),
    }
}

fn completion(source: &str, evidence: &str, source_text: &str, result: &str) -> ProposedOperation {
    ProposedOperation::ReplaceCompleted {
        source: source.into(),
        source_text: Some(source_text.into()),
        evidence: evidence.into(),
        result: result.into(),
    }
}

#[test]
fn v2_selection_apply_none_preserves_verified_prior_map_without_copying_work() {
    let mut huge = record(
        "large-work",
        Origin::Work,
        "tool",
        &"opaque-work ".repeat(16 * 1024),
    );
    huge.execution_evidence = true;
    let source = input(vec![
        record("human", Origin::Human, "user", "Retire this. Keep current."),
        huge,
    ]);
    let prior =
        prune_known_obsolete(&source, &[source_ref(&source, "human", "Retire this.")]).unwrap();
    let before = serde_json::to_vec(&source).unwrap();

    let applied = validate_and_apply_selection(&source, &prior, &[], None).unwrap();

    assert_eq!(applied.pruning, prior);
    assert!(applied.results.is_empty());
    assert_eq!(applied.plan_hash, None);
    assert_eq!(
        applied
            .pruning
            .retained_human_text
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["human"]
    );
    assert_eq!(serde_json::to_vec(&source).unwrap(), before);
}

#[test]
fn v2_selection_apply_binds_exact_correction_and_preserves_unrelated_work() {
    let mut unrelated = record(
        "large-work",
        Origin::Work,
        "assistant",
        &"unrelated ".repeat(8192),
    );
    unrelated.execution_evidence = true;
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this command."),
        record("correction", Origin::Human, "user", "Use the new command."),
        unrelated,
    ]);
    let pruning = InstructionPruning::default();
    let before = serde_json::to_vec(&source).unwrap();
    let selection = bound_selection(
        &source,
        &pruning,
        &[],
        ProposedPlan {
            operations: vec![drop(
                "old",
                None,
                "correction",
                Some("Use the new command."),
            )],
        },
    );

    let applied = validate_and_apply_selection(&source, &pruning, &[], Some(selection)).unwrap();

    assert_eq!(applied.pruning.retained_human_text["old"], "");
    assert_eq!(
        applied
            .pruning
            .retained_human_text
            .get("correction")
            .map(String::as_str)
            .unwrap_or(&source.records[1].text),
        "Use the new command."
    );
    assert_eq!(applied.pruning.applied.len(), 1);
    assert!(applied.plan_hash.is_some());
    assert!(applied.results.is_empty());
    assert_eq!(serde_json::to_vec(&source).unwrap(), before);
    assert!(
        !applied
            .pruning
            .retained_human_text
            .contains_key("large-work")
    );
}

#[test]
fn v2_selection_apply_rejects_forged_maps_and_stale_snapshot_bindings() {
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this."),
        record("correction", Origin::Human, "user", "Use this."),
    ]);
    let pruning = InstructionPruning::default();
    let selection = bound_selection(&source, &pruning, &[], ProposedPlan { operations: vec![] });
    let mut forged = pruning.clone();
    forged
        .retained_human_text
        .insert("old".into(), "forged".into());
    assert!(validate_and_apply_selection(&source, &forged, &[], None).is_err());

    let mut changed = source.clone();
    changed.records[0].text.push_str(" changed");
    assert!(validate_and_apply_selection(&changed, &pruning, &[], Some(selection)).is_err());
}

#[test]
fn v2_selection_apply_requires_nonblank_exact_correction_text() {
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this."),
        record("correction", Origin::Human, "user", "Use this."),
    ]);
    let pruning = InstructionPruning::default();
    for correction_text in [None, Some(""), Some(" \t\n ")] {
        let selection = bound_selection(
            &source,
            &pruning,
            &[],
            ProposedPlan {
                operations: vec![drop(
                    "old",
                    Some("Retire this."),
                    "correction",
                    correction_text,
                )],
            },
        );
        assert!(validate_and_apply_selection(&source, &pruning, &[], Some(selection)).is_err());
    }
}

#[test]
fn v2_selection_apply_rejects_hidden_ranges_and_whole_source_after_partial_pruning() {
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this now."),
        record("correction", Origin::Human, "user", "Use this instead."),
    ]);
    let prior = prune_known_obsolete(&source, &[source_ref(&source, "old", "this")]).unwrap();
    let hidden_passage = bound_selection(
        &source,
        &prior,
        &[],
        ProposedPlan {
            operations: vec![drop(
                "old",
                Some("Retire this now."),
                "correction",
                Some("Use this instead."),
            )],
        },
    );
    assert!(validate_and_apply_selection(&source, &prior, &[], Some(hidden_passage)).is_err());

    let whole_source = bound_selection(
        &source,
        &prior,
        &[],
        ProposedPlan {
            operations: vec![drop("old", None, "correction", Some("Use this instead."))],
        },
    );
    assert!(validate_and_apply_selection(&source, &prior, &[], Some(whole_source)).is_err());
}

#[test]
fn v2_selection_apply_rejects_prior_removal_from_a_correction_witness() {
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this."),
        record("correction", Origin::Human, "user", "Use this instead."),
    ]);
    let prior = prune_known_obsolete(&source, &[source_ref(&source, "correction", "Use")]).unwrap();
    let selection = bound_selection(
        &source,
        &prior,
        &[],
        ProposedPlan {
            operations: vec![drop(
                "old",
                Some("Retire this."),
                "correction",
                Some("Use this instead."),
            )],
        },
    );
    assert!(validate_and_apply_selection(&source, &prior, &[], Some(selection)).is_err());
}

#[test]
fn v2_selection_apply_rejects_removing_another_operation_correction() {
    let source = input(vec![
        record("old", Origin::Human, "user", "Retire this."),
        record("current", Origin::Human, "user", "Use this instead."),
        record("later", Origin::Human, "user", "Use the final rule."),
    ]);
    let pruning = InstructionPruning::default();
    let selection = bound_selection(
        &source,
        &pruning,
        &[],
        ProposedPlan {
            operations: vec![
                drop(
                    "old",
                    Some("Retire this."),
                    "current",
                    Some("Use this instead."),
                ),
                drop(
                    "current",
                    Some("Use this instead."),
                    "later",
                    Some("Use the final rule."),
                ),
            ],
        },
    );

    assert!(validate_and_apply_selection(&source, &pruning, &[], Some(selection)).is_err());
}

fn completion_input(two: bool) -> CandidateInput {
    let mut records = vec![record("human-1", Origin::Human, "user", "Run check one.")];
    records.push(record(
        "call-1",
        Origin::Work,
        "assistant",
        "{\"command\":\"check one\"}",
    ));
    let mut output1 = record("output-1", Origin::Work, "tool", "check one exited 0");
    output1.execution_evidence = true;
    records.push(output1);
    if two {
        records.push(record("human-2", Origin::Human, "user", "Run check two."));
        records.push(record(
            "call-2",
            Origin::Work,
            "assistant",
            "{\"command\":\"check two\"}",
        ));
        let mut output2 = record("output-2", Origin::Work, "tool", "check two exited 0");
        output2.execution_evidence = true;
        records.push(output2);
    }
    input(records)
}

#[test]
fn v2_selection_apply_accepts_two_independently_linked_completions() {
    let source = completion_input(true);
    let links = vec![
        link(&source, "human-1", "call-1", "output-1", "exec-1"),
        link(&source, "human-2", "call-2", "output-2", "exec-2"),
    ];
    let pruning = InstructionPruning::default();
    let selection = bound_selection(
        &source,
        &pruning,
        &links,
        ProposedPlan {
            operations: vec![
                completion(
                    "human-1",
                    "output-1",
                    "Run check one.",
                    "Check one returned exit code 0.",
                ),
                completion(
                    "human-2",
                    "output-2",
                    "Run check two.",
                    "Check two returned exit code 0.",
                ),
            ],
        },
    );

    let applied = validate_and_apply_selection(&source, &pruning, &links, Some(selection)).unwrap();

    assert_eq!(applied.pruning.applied.len(), 2);
    assert_eq!(applied.results.len(), 2);
    assert_eq!(applied.results[0].evidence, links[0].output);
    assert_eq!(applied.results[1].evidence, links[1].output);
}

#[test]
fn v2_selection_apply_requires_the_same_explicit_link_binding() {
    let source = completion_input(true);
    let first = link(&source, "human-1", "call-1", "output-1", "exec-1");
    let second = link(&source, "human-2", "call-2", "output-2", "exec-2");
    let pruning = InstructionPruning::default();
    let selection = bound_selection(
        &source,
        &pruning,
        std::slice::from_ref(&first),
        ProposedPlan { operations: vec![] },
    );
    let before = serde_json::to_vec(&source).unwrap();

    assert!(validate_and_apply_selection(&source, &pruning, &[second], Some(selection)).is_err());
    assert_eq!(serde_json::to_vec(&source).unwrap(), before);
}

#[test]
fn v2_selection_apply_requires_link_containment_and_exact_output_reference() {
    let mut source = completion_input(false);
    let mut unrelated_output = record(
        "output-extra",
        Origin::Work,
        "tool",
        "Another completed command result.",
    );
    unrelated_output.execution_evidence = true;
    source.records.push(unrelated_output);
    let link = link(&source, "human-1", "call-1", "output-1", "exec-1");
    let mut narrow_link = link.clone();
    narrow_link.instruction.range.end = "Run".len();
    let pruning = InstructionPruning::default();
    let outside = bound_selection(
        &source,
        &pruning,
        std::slice::from_ref(&narrow_link),
        ProposedPlan {
            operations: vec![completion(
                "human-1",
                "output-1",
                "Run check one",
                "completed",
            )],
        },
    );
    assert!(
        validate_and_apply_selection(
            &source,
            &pruning,
            std::slice::from_ref(&narrow_link),
            Some(outside)
        )
        .is_err()
    );

    let unrelated_evidence = bound_selection(
        &source,
        &pruning,
        std::slice::from_ref(&link),
        ProposedPlan {
            operations: vec![completion(
                "human-1",
                "output-extra",
                "Run check one.",
                "completed",
            )],
        },
    );
    assert!(
        validate_and_apply_selection(&source, &pruning, &[link], Some(unrelated_evidence)).is_err()
    );
}

#[test]
fn v2_selection_apply_binds_prior_map_ranges_even_when_visible_text_matches() {
    let source = input(vec![record("human", Origin::Human, "user", "aa current")]);
    let first = prune_known_obsolete(&source, &[source_ref(&source, "human", "a")]).unwrap();
    let second_ref = source_ref(&source, "human", "a");
    let mut second_ref = second_ref;
    second_ref.range.start = 1;
    second_ref.range.end = 2;
    let second = prune_known_obsolete(&source, &[second_ref]).unwrap();
    assert_eq!(first.retained_human_text, second.retained_human_text);
    let selection = bound_selection(&source, &first, &[], ProposedPlan { operations: vec![] });

    assert!(validate_and_apply_selection(&source, &second, &[], Some(selection)).is_err());
}

#[test]
fn v2_selection_apply_rejects_protected_and_opaque_human_sources() {
    let mut protected = record("old", Origin::Human, "user", "Retire this.");
    protected.protected.push(ByteRange { start: 0, end: 6 });
    let mut opaque = record("correction", Origin::Human, "user", "Use this.");
    opaque.opaque = Some(serde_json::json!({"attachment": true}));
    let source = input(vec![protected, opaque]);
    let pruning = InstructionPruning::default();
    let selection = bound_selection(
        &source,
        &pruning,
        &[],
        ProposedPlan {
            operations: vec![drop("old", Some("Retire"), "correction", Some("Use this."))],
        },
    );
    assert!(validate_and_apply_selection(&source, &pruning, &[], Some(selection)).is_err());
}

#[test]
fn v2_selection_apply_rejects_non_human_and_unknown_sources() {
    let source = input(vec![
        record("human", Origin::Human, "user", "A current instruction."),
        record("work", Origin::Work, "tool", "A tool result."),
        record("correction", Origin::Human, "user", "Use this instead."),
    ]);
    let pruning = InstructionPruning::default();
    let work_source = bound_selection(
        &source,
        &pruning,
        &[],
        ProposedPlan {
            operations: vec![drop(
                "work",
                Some("A tool result."),
                "correction",
                Some("Use this instead."),
            )],
        },
    );
    assert!(validate_and_apply_selection(&source, &pruning, &[], Some(work_source)).is_err());

    let unknown_source = bound_selection(
        &source,
        &pruning,
        &[],
        ProposedPlan {
            operations: vec![drop(
                "missing",
                Some("not present"),
                "correction",
                Some("Use this instead."),
            )],
        },
    );
    assert!(validate_and_apply_selection(&source, &pruning, &[], Some(unknown_source)).is_err());
}

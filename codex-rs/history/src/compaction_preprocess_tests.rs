use super::*;
use crate::archive_reference::ArchiveReference;
use crate::archive_reference::ArchiveTerminalStatus;
use crate::archive_reference::content_sha256;
use crate::compaction_candidate::CandidateInput;
use crate::compaction_candidate::CandidateRecord;
use crate::compaction_candidate::Origin;
use crate::compaction_plan::ByteRange;
use crate::compaction_plan::SourceRef;
use pretty_assertions::assert_eq;
use serde_json::json;

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

fn input(records: Vec<CandidateRecord>, prior_invalidations: Vec<String>) -> CandidateInput {
    CandidateInput {
        version: 1,
        binding: "thread-1/turn-1".into(),
        records,
        current_context: vec![json!({"base":"current context"})],
        prior_invalidations,
    }
}

fn source_ref(input: &CandidateInput, record_id: &str, passage: &str) -> SourceRef {
    let record = input
        .records
        .iter()
        .find(|record| record.id == record_id)
        .unwrap();
    let start = record.text.find(passage).unwrap();
    input
        .snapshot()
        .unwrap()
        .reference(
            record_id,
            ByteRange {
                start,
                end: start + passage.len(),
            },
        )
        .unwrap()
}

fn full_source_ref(input: &CandidateInput, record_id: &str) -> SourceRef {
    let record = input
        .records
        .iter()
        .find(|record| record.id == record_id)
        .unwrap();
    input
        .snapshot()
        .unwrap()
        .reference(
            record_id,
            ByteRange {
                start: 0,
                end: record.text.len(),
            },
        )
        .unwrap()
}

fn completion_source() -> CandidateInput {
    input(
        vec![
            record(
                "human-request",
                Origin::Human,
                "user",
                "Run the test suite and report its result.",
            ),
            record(
                "call-item",
                Origin::Work,
                "assistant",
                "{\"command\":\"just test\"}",
            ),
            {
                let mut output = record(
                    "output-item",
                    Origin::Work,
                    "tool",
                    "Finished: 12 tests passed.",
                );
                output.execution_evidence = true;
                output
            },
        ],
        vec![],
    )
}

fn completion_link(input: &CandidateInput) -> InstructionObservationLink {
    let output = input
        .records
        .iter()
        .find(|record| record.id == "output-item")
        .unwrap();
    InstructionObservationLink {
        instruction: full_source_ref(input, "human-request"),
        call: full_source_ref(input, "call-item"),
        output: full_source_ref(input, "output-item"),
        terminal: ArchiveReference::new(
            "thread-1",
            "exec-call-1",
            content_sha256(&output.text),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        ),
    }
}

fn completion_link_for(
    input: &CandidateInput,
    instruction_id: &str,
    call_source_id: &str,
    output_source_id: &str,
    tool_call_id: &str,
) -> InstructionObservationLink {
    let output = input
        .records
        .iter()
        .find(|record| record.id == output_source_id)
        .unwrap();
    InstructionObservationLink {
        instruction: full_source_ref(input, instruction_id),
        call: full_source_ref(input, call_source_id),
        output: full_source_ref(input, output_source_id),
        terminal: ArchiveReference::new(
            "thread-1",
            tool_call_id,
            content_sha256(&output.text),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        ),
    }
}

#[test]
fn prune_known_obsolete_ignores_legacy_literals_and_removes_only_exact_source_refs() {
    let source = input(
        vec![record(
            "human-1",
            Origin::Human,
            "user",
            "Use the old route; keep checking the new route.",
        )],
        vec![
            "keep checking".into(),
            "a phrase that occurs in no record".into(),
        ],
    );
    let obsolete = source_ref(&source, "human-1", "Use the old route");
    let before = serde_json::to_value(&source).unwrap();

    let result = prune_known_obsolete(&source, &[obsolete.clone()]).unwrap();

    assert_eq!(result.applied, vec![obsolete]);
    assert_eq!(
        result.retained_human_text,
        [(
            "human-1".to_owned(),
            "; keep checking the new route.".to_owned()
        )]
        .into_iter()
        .collect()
    );
    assert_eq!(serde_json::to_value(&source).unwrap(), before);
}

#[test]
fn prune_known_obsolete_skips_missing_changed_and_reissued_sources() {
    let prior = input(
        vec![record(
            "instruction-old",
            Origin::Human,
            "user",
            "Use the old route.",
        )],
        vec![],
    );
    let old_reference = source_ref(&prior, "instruction-old", "Use the old route");
    let missing = SourceRef {
        id: "missing-id".into(),
        ..old_reference.clone()
    };
    let refs = vec![old_reference.clone(), missing];

    let current = input(
        vec![
            record(
                "instruction-old",
                Origin::Human,
                "user",
                "Please use the old route again.",
            ),
            record(
                "instruction-new",
                Origin::Human,
                "user",
                "Use the old route.",
            ),
        ],
        vec!["Please use the old route again".into()],
    );

    let result = prune_known_obsolete(&current, &refs).unwrap();

    assert!(result.applied.is_empty());
    assert!(result.retained_human_text.is_empty());
}

#[test]
fn prune_known_obsolete_ignores_overlaps_from_stale_hashes_in_either_order() {
    let prior = input(
        vec![record(
            "instruction",
            Origin::Human,
            "user",
            "Retire the old route now.",
        )],
        vec![],
    );
    let old_reference = source_ref(&prior, "instruction", "Retire the old route");
    let current = input(
        vec![record(
            "instruction",
            Origin::Human,
            "user",
            "Use the old route today.",
        )],
        vec![],
    );
    let current_reference = source_ref(&current, "instruction", "Use the old route");
    assert!(old_reference.range.start < current_reference.range.end);
    assert!(current_reference.range.start < old_reference.range.end);

    for refs in [
        vec![old_reference.clone(), current_reference.clone()],
        vec![current_reference.clone(), old_reference.clone()],
    ] {
        let result = prune_known_obsolete(&current, &refs).unwrap();
        assert_eq!(result.applied, vec![current_reference.clone()]);
        assert_eq!(
            result
                .retained_human_text
                .get("instruction")
                .map(String::as_str),
            Some(" today.")
        );
    }
}

#[test]
fn prune_known_obsolete_rejects_exact_nonhuman_sources() {
    let source = input(
        vec![
            record("work", Origin::Work, "assistant", "Tool result body."),
            record("host", Origin::Host, "developer", "Host context body."),
        ],
        vec![],
    );
    for reference in [
        source_ref(&source, "work", "Tool result"),
        source_ref(&source, "host", "Host context"),
    ] {
        assert!(prune_known_obsolete(&source, &[reference]).is_err());
    }
}

#[test]
fn prune_known_obsolete_does_not_use_legacy_invalidations_as_deletion_proof() {
    let source = input(
        vec![
            record(
                "human",
                Origin::Human,
                "user",
                "Please keep the phrase that used to be invalidated.",
            ),
            record(
                "summary",
                Origin::Work,
                "user",
                "Old summary repeated retired material.",
            ),
        ],
        vec!["retired material".into(), "".into()],
    );

    let result = prune_known_obsolete(&source, &[]).unwrap();

    assert_eq!(result, InstructionPruning::default());
}

#[test]
fn prune_known_obsolete_removes_multiple_unicode_ranges_from_original_offsets() {
    let source = input(
        vec![record(
            "human-1",
            Origin::Human,
            "user",
            "旧規則。保持。再確認。",
        )],
        vec![],
    );
    let refs = vec![
        source_ref(&source, "human-1", "旧規則"),
        source_ref(&source, "human-1", "再確認"),
    ];

    let result = prune_known_obsolete(&source, &refs).unwrap();

    assert_eq!(result.applied, refs);
    assert_eq!(
        result
            .retained_human_text
            .get("human-1")
            .map(String::as_str),
        Some("。保持。。")
    );
}

#[test]
fn prune_known_obsolete_rejects_matching_protected_opaque_and_unknown_sources() {
    let valid = record("valid", Origin::Human, "user", "Remove this phrase too.");
    let mut protected = record(
        "protected",
        Origin::Human,
        "user",
        "Remove this exact phrase.",
    );
    let start = protected.text.find("exact phrase").unwrap();
    protected.protected.push(ByteRange {
        start,
        end: start + "exact phrase".len(),
    });
    let source = input(vec![valid, protected], vec![]);
    assert!(
        prune_known_obsolete(
            &source,
            &[
                source_ref(&source, "valid", "Remove this phrase"),
                source_ref(&source, "protected", "exact phrase"),
            ],
        )
        .is_err()
    );

    let mut opaque = record("opaque", Origin::Human, "user", "Remove this exact phrase.");
    opaque.opaque = Some(json!({"attachment":"ref"}));
    let source = input(vec![opaque], vec![]);
    assert!(
        prune_known_obsolete(&source, &[source_ref(&source, "opaque", "exact phrase")]).is_err()
    );

    let source = input(
        vec![record(
            "unknown",
            Origin::Unknown,
            "user",
            "Remove this exact phrase.",
        )],
        vec![],
    );
    assert!(
        prune_known_obsolete(&source, &[source_ref(&source, "unknown", "exact phrase")]).is_err()
    );
}

#[test]
fn prune_known_obsolete_rejects_invalid_duplicate_and_overlapping_ranges() {
    let source = input(
        vec![record("human", Origin::Human, "user", "猫犬鳥")],
        vec![],
    );
    let first = source_ref(&source, "human", "猫犬");
    let second = source_ref(&source, "human", "犬鳥");
    assert!(prune_known_obsolete(&source, &[first.clone(), first]).is_err());
    assert!(
        prune_known_obsolete(&source, &[second, source_ref(&source, "human", "猫犬")]).is_err()
    );

    let mut invalid_utf8_boundary = source_ref(&source, "human", "猫");
    invalid_utf8_boundary.range = ByteRange { start: 1, end: 2 };
    assert!(prune_known_obsolete(&source, &[invalid_utf8_boundary]).is_err());

    let mut empty_range = source_ref(&source, "human", "猫");
    empty_range.range = ByteRange { start: 2, end: 2 };
    assert!(prune_known_obsolete(&source, &[empty_range]).is_err());
}

#[test]
fn prune_known_obsolete_is_immutable_and_returns_only_modified_human_text() {
    let large_text = "x".repeat(256 * 1024);
    let source = input(
        vec![
            record(
                "human",
                Origin::Human,
                "user",
                "Drop the old rule; keep the current request.",
            ),
            record("large-work", Origin::Work, "assistant", &large_text),
        ],
        vec!["drop the old rule".into()],
    );
    let human_reference = source_ref(&source, "human", "Drop the old rule");
    let before = serde_json::to_value(&source).unwrap();

    let result = prune_known_obsolete(&source, &[human_reference.clone()]).unwrap();
    let repeated = prune_known_obsolete(&source, &[human_reference.clone()]).unwrap();

    assert_eq!(result, repeated);
    assert_eq!(result.applied, vec![human_reference]);
    assert_eq!(
        result.retained_human_text,
        [("human".into(), "; keep the current request.".into())]
            .into_iter()
            .collect()
    );
    assert!(!result.retained_human_text.contains_key("large-work"));
    assert_eq!(serde_json::to_value(&source).unwrap(), before);
}

#[path = "compaction_preprocess_candidate_tests.rs"]
mod candidate_tests;

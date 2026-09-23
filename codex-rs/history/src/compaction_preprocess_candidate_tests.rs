use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn collect_instruction_candidates_ignores_unrelated_work_and_preserves_human_order() {
    let lean = input(
        vec![
            record("human-1", Origin::Human, "user", "First request."),
            record("human-2", Origin::Human, "user", "Second request."),
        ],
        vec![],
    );
    let mut large_evidence = record(
        "unrelated-large-work",
        Origin::Work,
        "tool",
        &"UNRELATED_RESULT ".repeat(32 * 1024),
    );
    large_evidence.execution_evidence = true;
    let expanded = input(
        vec![
            record("human-1", Origin::Human, "user", "First request."),
            record(
                "normal-work",
                Origin::Work,
                "assistant",
                "An unrelated assistant note.",
            ),
            large_evidence,
            record("unknown", Origin::Unknown, "user", "Do not elevate this."),
            record("host", Origin::Host, "developer", "Host context."),
            record("human-2", Origin::Human, "user", "Second request."),
        ],
        vec![],
    );

    let lean_payload = collect_instruction_candidates(&lean, &InstructionPruning::default(), &[])
        .unwrap()
        .unwrap();
    let expanded_payload =
        collect_instruction_candidates(&expanded, &InstructionPruning::default(), &[])
            .unwrap()
            .unwrap();

    assert_eq!(lean_payload["sources"], expanded_payload["sources"]);
    assert_eq!(
        lean_payload.to_string().len(),
        expanded_payload.to_string().len()
    );
    assert_eq!(
        expanded_payload["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["human-1", "human-2"]
    );
    assert!(
        expanded_payload["completion_links"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!expanded_payload.to_string().contains("UNRELATED_RESULT"));
    assert!(!expanded_payload.to_string().contains("assistant note"));
    assert!(!expanded_payload.to_string().contains("Do not elevate this"));
    assert!(!expanded_payload.to_string().contains("Host context"));
}

#[test]
fn collect_instruction_candidates_applies_pruning_and_rebases_protected_ranges() {
    let mut human = record(
        "human",
        Origin::Human,
        "user",
        "Retire old; keep current request.",
    );
    let protected_start = human.text.find("current").unwrap();
    let protected_end = protected_start + "current".len();
    human.protected.push(ByteRange {
        start: protected_start,
        end: protected_end,
    });
    let source = input(vec![human], vec!["ignored legacy phrase".into()]);
    let pruning =
        prune_known_obsolete(&source, &[source_ref(&source, "human", "Retire old")]).unwrap();

    let payload = collect_instruction_candidates(&source, &pruning, &[])
        .unwrap()
        .unwrap();
    let human_payload = &payload["sources"][0];

    assert_eq!(human_payload["text"], "; keep current request.");
    assert_eq!(
        human_payload["protected"],
        json!([{
            "start": protected_start - "Retire old".len(),
            "end": protected_end - "Retire old".len(),
        }])
    );
    assert_eq!(human_payload["candidate"], true);

    let mut tampered_map = pruning.clone();
    tampered_map
        .retained_human_text
        .insert("human".into(), "forged projection".into());
    assert!(collect_instruction_candidates(&source, &tampered_map, &[]).is_err());

    let mut tampered_ref = pruning;
    tampered_ref.applied[0].hash = "0".repeat(64);
    assert!(collect_instruction_candidates(&source, &tampered_ref, &[]).is_err());
}

#[test]
fn collect_instruction_candidates_keeps_protected_humans_as_context_and_skips_empty_candidates() {
    let mut fully_protected = record("protected", Origin::Human, "user", "Do not remove this.");
    fully_protected.protected.push(ByteRange {
        start: 0,
        end: fully_protected.text.len(),
    });
    let mut opaque = record("opaque", Origin::Human, "user", "Attachment context.");
    opaque.opaque = Some(json!({"secret":"opaque-payload-must-not-be-sent"}));
    let editable = record(
        "editable",
        Origin::Human,
        "user",
        "Please update this instruction.",
    );
    let source = input(
        vec![fully_protected.clone(), opaque.clone(), editable],
        vec![],
    );

    let payload = collect_instruction_candidates(&source, &InstructionPruning::default(), &[])
        .unwrap()
        .unwrap();
    let sources = payload["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[0]["candidate"], false);
    assert_eq!(sources[1]["candidate"], false);
    assert_eq!(sources[1]["has_opaque"], true);
    assert_eq!(sources[2]["candidate"], true);
    assert!(
        !payload
            .to_string()
            .contains("opaque-payload-must-not-be-sent")
    );

    let emptied = input(
        vec![
            fully_protected,
            opaque,
            record("retired", Origin::Human, "user", "Already retired."),
        ],
        vec![],
    );
    let pruning = prune_known_obsolete(
        &emptied,
        &[source_ref(&emptied, "retired", "Already retired.")],
    )
    .unwrap();
    assert!(
        collect_instruction_candidates(&emptied, &pruning, &[])
            .unwrap()
            .is_none()
    );
}

#[test]
fn collect_instruction_candidates_includes_only_one_explicit_terminal_pair_link() {
    let mut source = completion_source();
    let mut unrelated = record(
        "unlinked-work",
        Origin::Work,
        "tool",
        "Same-thread output without a host link.",
    );
    unrelated.execution_evidence = true;
    source.records.push(unrelated);
    let link = completion_link(&source);

    let payload = collect_instruction_candidates(&source, &InstructionPruning::default(), &[link])
        .unwrap()
        .unwrap();

    assert_eq!(payload["sources"].as_array().unwrap().len(), 1);
    assert_eq!(payload["sources"][0]["id"], "human-request");
    let links = payload["completion_links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["instruction_id"], "human-request");
    assert_eq!(links[0]["call_source_id"], "call-item");
    assert_eq!(links[0]["output_source_id"], "output-item");
    assert_eq!(links[0]["tool_call_id"], "exec-call-1");
    assert_eq!(links[0]["call_text"], "{\"command\":\"just test\"}");
    assert_eq!(links[0]["output_text"], "Finished: 12 tests passed.");
    assert_eq!(links[0]["terminal_status"], "completed");
    assert_eq!(links[0]["terminal_exit_code"], 0);
    assert_eq!(links[0]["candidate_only"], true);
    assert!(
        !payload
            .to_string()
            .contains("Same-thread output without a host link")
    );
    assert!(links[0].get("original_content_sha256").is_none());
    assert!(links[0].get("process_id").is_none());
}

#[test]
fn collect_instruction_candidates_allows_independent_instruction_links() {
    let source = input(
        vec![
            record("human-a", Origin::Human, "user", "Run check A."),
            record("call-a", Origin::Work, "assistant", "{\"command\":\"a\"}"),
            {
                let mut output = record("output-a", Origin::Work, "tool", "A finished.");
                output.execution_evidence = true;
                output
            },
            record("human-b", Origin::Human, "user", "Run check B."),
            record("call-b", Origin::Work, "assistant", "{\"command\":\"b\"}"),
            {
                let mut output = record("output-b", Origin::Work, "tool", "B finished.");
                output.execution_evidence = true;
                output
            },
        ],
        vec![],
    );
    let links = [
        completion_link_for(&source, "human-a", "call-a", "output-a", "tool-call-a"),
        completion_link_for(&source, "human-b", "call-b", "output-b", "tool-call-b"),
    ];

    let payload = collect_instruction_candidates(&source, &InstructionPruning::default(), &links)
        .unwrap()
        .unwrap();

    assert_eq!(
        payload["completion_links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["instruction_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["human-a", "human-b"]
    );
    assert_eq!(
        payload["completion_links"][0]["call_text"],
        "{\"command\":\"a\"}"
    );
    assert_eq!(payload["completion_links"][1]["output_text"], "B finished.");

    let mut duplicate_instruction = links[1].clone();
    duplicate_instruction.instruction = links[0].instruction.clone();
    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[links[0].clone(), duplicate_instruction],
        )
        .unwrap_err()
        .contains("reused")
    );

    let mut reused_call = links[1].clone();
    reused_call.call = links[0].call.clone();
    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[links[0].clone(), reused_call],
        )
        .unwrap_err()
        .contains("reused")
    );

    let mut reused_output = links[1].clone();
    reused_output.output = links[0].output.clone();
    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[links[0].clone(), reused_output],
        )
        .unwrap_err()
        .contains("reused")
    );

    let mut reused_terminal_pair = links[1].clone();
    reused_terminal_pair.terminal.thread_id = links[0].terminal.thread_id.clone();
    reused_terminal_pair.terminal.call_id = links[0].terminal.call_id.clone();
    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[links[0].clone(), reused_terminal_pair],
        )
        .unwrap_err()
        .contains("reused")
    );
}

#[test]
fn collect_instruction_candidates_rejects_stale_or_tampered_links_and_pruning_maps() {
    let source = completion_source();
    let link = completion_link(&source);

    let mut stale = link.clone();
    stale.call.hash = "0".repeat(64);
    assert!(
        collect_instruction_candidates(&source, &InstructionPruning::default(), &[stale]).is_err()
    );

    let mut wrong_terminal_hash = link.clone();
    wrong_terminal_hash.terminal.original_content_sha256 = content_sha256("tampered output");
    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[wrong_terminal_hash],
        )
        .is_err()
    );

    let mut failed_terminal = link.clone();
    failed_terminal.terminal.status = ArchiveTerminalStatus::Completed;
    failed_terminal.terminal.exit_code = 1;
    assert!(collect_instruction_candidates(
        &source,
        &InstructionPruning::default(),
        &[failed_terminal],
    )
    .is_err());

    let mut wrong_version = link.clone();
    wrong_version.terminal.version += 1;
    assert!(
        collect_instruction_candidates(&source, &InstructionPruning::default(), &[wrong_version],)
            .is_err()
    );

    let mut partial_call = link.clone();
    partial_call.call.range.end -= 1;
    assert!(
        collect_instruction_candidates(&source, &InstructionPruning::default(), &[partial_call],)
            .is_err()
    );

    assert!(
        collect_instruction_candidates(
            &source,
            &InstructionPruning::default(),
            &[link.clone(), link],
        )
        .is_err()
    );
}

#[test]
fn collect_instruction_candidates_rejects_oversized_or_protected_link_parts() {
    let mut oversized = completion_source();
    oversized.records[1].text = "x".repeat(2_049);
    let oversized_link = completion_link(&oversized);
    assert!(
        collect_instruction_candidates(
            &oversized,
            &InstructionPruning::default(),
            &[oversized_link],
        )
        .is_err()
    );

    let mut protected = completion_source();
    let protected_len = protected.records[0].text.len();
    protected.records[0].protected.push(ByteRange {
        start: 0,
        end: protected_len,
    });
    let protected_link = completion_link(&protected);
    assert!(
        collect_instruction_candidates(
            &protected,
            &InstructionPruning::default(),
            &[protected_link],
        )
        .is_err()
    );

    let mut non_human_instruction = completion_source();
    non_human_instruction.records[0].origin = Origin::Unknown;
    non_human_instruction.records[0].intake_ref = None;
    let non_human_link = completion_link(&non_human_instruction);
    assert!(
        collect_instruction_candidates(
            &non_human_instruction,
            &InstructionPruning::default(),
            &[non_human_link],
        )
        .is_err()
    );

    let mut wrong_execution_role = completion_source();
    wrong_execution_role.records[1].execution_evidence = true;
    let invalid_link = completion_link(&wrong_execution_role);
    assert!(
        collect_instruction_candidates(
            &wrong_execution_role,
            &InstructionPruning::default(),
            &[invalid_link],
        )
        .is_err()
    );
}

#[test]
fn instruction_presentation_hash_binds_different_pruning_ranges_with_same_retained_text() {
    let source = input(
        vec![record("human", Origin::Human, "user", "same same")],
        vec![],
    );
    let snapshot = source.snapshot().unwrap();
    let first_removed = snapshot
        .reference("human", ByteRange { start: 0, end: 5 })
        .unwrap();
    let second_removed = snapshot
        .reference("human", ByteRange { start: 4, end: 9 })
        .unwrap();
    let first_pruning = prune_known_obsolete(&source, &[first_removed]).unwrap();
    let second_pruning = prune_known_obsolete(&source, &[second_removed]).unwrap();
    assert_eq!(
        first_pruning.retained_human_text,
        second_pruning.retained_human_text
    );
    assert_eq!(
        first_pruning.retained_human_text["human"],
        second_pruning.retained_human_text["human"]
    );

    let first_hash =
        instruction_presentation_hash(&snapshot.hash(), &first_pruning.applied, &[]).unwrap();
    let same_hash =
        instruction_presentation_hash(&snapshot.hash(), &first_pruning.applied, &[]).unwrap();
    let second_hash =
        instruction_presentation_hash(&snapshot.hash(), &second_pruning.applied, &[]).unwrap();

    assert_eq!(first_hash, same_hash);
    assert_ne!(first_hash, second_hash);
    let first_payload = collect_instruction_candidates(&source, &first_pruning, &[])
        .unwrap()
        .unwrap();
    let second_payload = collect_instruction_candidates(&source, &second_pruning, &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        first_payload["sources"][0]["text"],
        second_payload["sources"][0]["text"]
    );
    assert_eq!(first_payload["presentation_hash"], first_hash);
    assert_eq!(second_payload["presentation_hash"], second_hash);
}

#[test]
fn instruction_presentation_hash_binds_host_links_and_link_ranges() {
    let source = completion_source();
    let snapshot = source.snapshot().unwrap();
    let snapshot_hash = snapshot.hash();
    let pruning = InstructionPruning::default();
    let full = completion_link(&source);
    let mut narrower = full.clone();
    narrower.instruction = source_ref(&source, "human-request", "Run the test suite");

    let full_hash =
        instruction_presentation_hash(&snapshot_hash, &pruning.applied, &[full.clone()]).unwrap();
    let repeated_hash =
        instruction_presentation_hash(&snapshot_hash, &pruning.applied, &[full.clone()]).unwrap();
    let narrow_hash =
        instruction_presentation_hash(&snapshot_hash, &pruning.applied, &[narrower.clone()])
            .unwrap();

    assert_eq!(full_hash, repeated_hash);
    assert_ne!(full_hash, narrow_hash);
    let full_payload = collect_instruction_candidates(&source, &pruning, &[full])
        .unwrap()
        .unwrap();
    let narrow_payload = collect_instruction_candidates(&source, &pruning, &[narrower])
        .unwrap()
        .unwrap();
    assert_eq!(
        full_payload["completion_links"][0]["output_text"],
        narrow_payload["completion_links"][0]["output_text"]
    );
    assert_ne!(
        full_payload["presentation_hash"],
        narrow_payload["presentation_hash"]
    );
}

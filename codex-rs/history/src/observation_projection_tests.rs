use super::*;
use crate::archive_reference::ObservationReference;
use crate::archive_reference::content_sha256;
use pretty_assertions::assert_eq;

fn reference(call_id: &str, output: &str) -> ObservationReference {
    ObservationReference::new("thread-1", call_id, content_sha256(output))
}

fn assert_part_matches(raw: &str, part: &ObservationPartExcerpt) {
    assert!(part.coverage.validate().is_ok());
    assert_eq!(part.coverage.total_bytes, raw.len());
    assert_eq!(part.coverage.presented_ranges.len(), part.excerpts.len());
    for (range, excerpt) in part.coverage.presented_ranges.iter().zip(&part.excerpts) {
        assert_eq!(raw.get(range.start..range.end), Some(excerpt.as_str()));
    }

    let mut ranges = part
        .coverage
        .presented_ranges
        .iter()
        .chain(&part.coverage.unpresented_ranges)
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut cursor = 0;
    for range in ranges {
        assert_eq!(range.start, cursor);
        assert!(range.start < range.end);
        cursor = range.end;
    }
    assert_eq!(cursor, raw.len());
}

#[test]
fn small_parts_are_fully_presented_and_call_digest_is_independent() {
    let call_text = "lookup({\"path\":\"doc.md\"})";
    let output_text = "found two relevant sections";
    let reference = reference("call-1", output_text);

    let projection =
        project_observation(&reference, "lookup_document", call_text, output_text).unwrap();

    assert_eq!(projection.summary.reference, reference);
    assert_eq!(projection.summary.tool_name, "lookup_document");
    assert_eq!(projection.summary.call.excerpts, [call_text]);
    assert_eq!(projection.summary.output.excerpts, [output_text]);
    assert_eq!(
        projection.summary.call.coverage.sha256,
        content_sha256(call_text)
    );
    assert_eq!(
        projection.summary.output.coverage.sha256,
        content_sha256(output_text)
    );
    assert_ne!(projection.summary.call.coverage.sha256, reference.sha256);
    assert_eq!(
        projection.summary.call.coverage.total_bytes,
        call_text.len()
    );
    assert_eq!(
        projection.summary.output.coverage.total_bytes,
        output_text.len()
    );
    assert_part_matches(call_text, &projection.summary.call);
    assert_part_matches(output_text, &projection.summary.output);
    assert_eq!(projection.coverage.call, projection.summary.call.coverage);
    assert_eq!(
        projection.coverage.output,
        projection.summary.output.coverage
    );
    assert!(projection.validate().is_ok());
}

#[test]
fn empty_parts_have_empty_ranges_and_are_not_partial() {
    let empty_reference = reference("empty-call", "");

    let projection = project_observation(&empty_reference, "empty_result", "", "").unwrap();

    for part in [&projection.coverage.call, &projection.coverage.output] {
        assert_eq!(part.total_bytes, 0);
        assert!(part.presented_ranges.is_empty());
        assert!(part.unpresented_ranges.is_empty());
        assert!(!part.partial);
        assert!(part.validate().is_ok());
    }
    assert!(projection.summary.call.excerpts.is_empty());
    assert!(projection.summary.output.excerpts.is_empty());
    assert!(projection.validate().is_ok());
}

#[test]
fn large_ascii_parts_use_bounded_nonoverlapping_head_and_tail() {
    let call_text = format!("{}{}", "c".repeat(4_096), "d".repeat(4_096));
    let output_text = format!("{}{}", "x".repeat(3_000), "y".repeat(3_000));
    let reference = reference("large-call", &output_text);

    let projection =
        project_observation(&reference, "read_large_file", &call_text, &output_text).unwrap();

    for (raw, part) in [
        (&call_text, &projection.summary.call),
        (&output_text, &projection.summary.output),
    ] {
        assert_eq!(part.excerpts.len(), 2);
        assert!(
            part.excerpts
                .iter()
                .all(|text| text.len() <= OBSERVATION_PART_EDGE_BYTES)
        );
        assert_eq!(
            part.excerpts[0],
            raw[..part.coverage.presented_ranges[0].end]
        );
        assert_eq!(
            part.excerpts[1],
            raw[part.coverage.presented_ranges[1].start..]
        );
        assert!(part.coverage.partial);
        assert_eq!(part.coverage.unpresented_ranges.len(), 1);
        assert_part_matches(raw, part);
    }
}

#[test]
fn unicode_and_emoji_edges_move_inward_to_valid_utf8_boundaries() {
    let call_text = "猫🙂abc🐦".repeat(600);
    let output_text = "🙂猫犬🦉".repeat(700);
    let reference = reference("unicode-call", &output_text);

    let projection =
        project_observation(&reference, "search_unicode", &call_text, &output_text).unwrap();

    for (raw, part) in [
        (&call_text, &projection.summary.call),
        (&output_text, &projection.summary.output),
    ] {
        assert_eq!(part.excerpts.len(), 2);
        assert!(
            part.excerpts
                .iter()
                .all(|text| text.len() <= OBSERVATION_PART_EDGE_BYTES)
        );
        assert!(raw.is_char_boundary(part.coverage.presented_ranges[0].end));
        assert!(raw.is_char_boundary(part.coverage.presented_ranges[1].start));
        assert_part_matches(raw, part);
    }
}

#[test]
fn digest_mismatch_and_empty_identity_fields_are_rejected() {
    let good_reference = reference("call-1", "expected output");
    assert!(project_observation(&good_reference, "read_file", "call", "different output").is_err());

    let invalid_digest = ObservationReference::new("thread-1", "call-1", "not-a-sha256".into());
    assert!(project_observation(&invalid_digest, "read_file", "call", "expected output").is_err());

    for reference in [
        ObservationReference::new("", "call-1", content_sha256("output")),
        ObservationReference::new("thread-1", "", content_sha256("output")),
    ] {
        assert!(project_observation(&reference, "read_file", "call", "output").is_err());
    }
    assert!(project_observation(&good_reference, " \t", "call", "expected output").is_err());
}

#[test]
fn checkpoint_coverage_serializes_metadata_without_raw_or_excerpt_text() {
    let call_text = format!("CALL_SENTINEL:{}", "c".repeat(5_000));
    let output_text = format!("OUTPUT_SENTINEL:{}", "o".repeat(5_000));
    let reference = reference("call-coverage", &output_text);
    let projection =
        project_observation(&reference, "custom_read", &call_text, &output_text).unwrap();

    let coverage_json = serde_json::to_value(&projection.coverage).unwrap();
    let encoded_coverage = coverage_json.to_string();
    assert!(coverage_json.get("reference").is_some());
    assert!(coverage_json.get("tool_name").is_some());
    assert!(coverage_json.get("call").is_some());
    assert!(coverage_json.get("output").is_some());
    assert!(!coverage_json.get("excerpts").is_some());
    assert!(!encoded_coverage.contains("CALL_SENTINEL"));
    assert!(!encoded_coverage.contains("OUTPUT_SENTINEL"));
    assert!(!encoded_coverage.contains(&call_text));
    assert!(!encoded_coverage.contains(&output_text));

    let summary_json = serde_json::to_value(&projection.summary).unwrap();
    assert_eq!(
        summary_json["call"]["coverage"]["sha256"],
        content_sha256(&call_text)
    );
    assert_eq!(
        summary_json["output"]["coverage"]["sha256"],
        reference.sha256
    );
    assert_eq!(summary_json["call"]["coverage"]["partial"], true);
    assert_eq!(summary_json["output"]["coverage"]["partial"], true);
    assert!(summary_json["call"]["coverage"]["unpresented_ranges"].is_array());
}

#[test]
fn projection_is_deterministic_and_does_not_modify_input_strings() {
    let call_text = "q".repeat(3_000);
    let output_text = "r".repeat(3_500);
    let originals = (call_text.clone(), output_text.clone());
    let reference = reference("stable-call", &output_text);

    let first = project_observation(&reference, "stable_tool", &call_text, &output_text).unwrap();
    let second = project_observation(&reference, "stable_tool", &call_text, &output_text).unwrap();

    assert_eq!(first, second);
    assert_eq!((call_text, output_text), originals);
}

#[test]
fn coverage_validation_rejects_partial_marked_as_full_or_inconsistent_projection() {
    let call_text = "call".repeat(700);
    let output_text = "output".repeat(800);
    let reference = reference("partial-call", &output_text);
    let mut projection =
        project_observation(&reference, "partial_tool", &call_text, &output_text).unwrap();
    assert!(projection.coverage.validate().is_ok());

    projection.coverage.call.partial = false;
    assert!(projection.coverage.validate().is_err());

    let mut projection =
        project_observation(&reference, "partial_tool", &call_text, &output_text).unwrap();
    projection.summary.output.coverage.partial = false;
    assert!(projection.validate().is_err());
}

#[test]
fn threshold_sized_part_is_full_and_one_byte_over_is_partial() {
    let full_text = "f".repeat(OBSERVATION_PART_FULL_LIMIT_BYTES);
    let partial_text = "p".repeat(OBSERVATION_PART_FULL_LIMIT_BYTES + 1);

    let full = project_observation(
        &reference("full", "small output"),
        "threshold_tool",
        &full_text,
        "small output",
    )
    .unwrap();
    let partial = project_observation(
        &reference("partial", &partial_text),
        "threshold_tool",
        "small call",
        &partial_text,
    )
    .unwrap();

    assert_eq!(full.summary.call.excerpts, [full_text.as_str()]);
    assert!(!full.coverage.call.partial);
    assert_eq!(partial.summary.output.excerpts.len(), 2);
    assert!(partial.coverage.output.partial);
}

#[test]
fn coverage_validation_rejects_incorrect_output_reference_binding() {
    let output_text = "output";
    let reference = reference("binding-call", output_text);
    let mut projection =
        project_observation(&reference, "binding_tool", "call", output_text).unwrap();
    projection.coverage.output.sha256 = content_sha256("different");
    assert!(projection.coverage.validate().is_err());
}

#[test]
fn existing_reference_projects_only_current_call_and_marks_original_output_unpresented() {
    let original_output = format!("ORIGINAL_OUTPUT_SENTINEL:{}", "r".repeat(6_000));
    let reference = reference("existing-call", &original_output);
    let call_text = format!("CURRENT_CALL_SENTINEL:{}", "c".repeat(5_000));
    let output_total_bytes = original_output.len();

    let projection =
        project_existing_observation(&reference, "exec_command", &call_text, output_total_bytes)
            .unwrap();

    assert_eq!(projection.summary.reference, reference);
    assert_eq!(projection.summary.tool_name, "exec_command");
    assert_eq!(
        projection.summary.call.coverage.sha256,
        content_sha256(&call_text)
    );
    assert_eq!(
        projection.summary.call.coverage.total_bytes,
        call_text.len()
    );
    assert_eq!(projection.summary.call.excerpts.len(), 2);
    assert!(
        projection
            .summary
            .call
            .excerpts
            .iter()
            .all(|excerpt| excerpt.len() <= OBSERVATION_PART_EDGE_BYTES)
    );
    assert_eq!(
        projection.coverage.output.sha256,
        projection.summary.reference.sha256
    );
    assert_eq!(
        projection.coverage.output.total_bytes,
        original_output.len()
    );
    assert!(projection.coverage.output.presented_ranges.is_empty());
    assert_eq!(
        projection.coverage.output.unpresented_ranges,
        [ByteRange {
            start: 0,
            end: original_output.len(),
        }]
    );
    assert!(projection.coverage.output.partial);
    assert!(projection.summary.output.excerpts.is_empty());
    assert!(projection.validate().is_ok());

    let serialized = serde_json::to_string(&projection.coverage).unwrap();
    assert!(!serialized.contains("CURRENT_CALL_SENTINEL"));
    assert!(!serialized.contains("ORIGINAL_OUTPUT_SENTINEL"));
    let carried: ObservationCoverage = serde_json::from_str(&serialized).unwrap();
    assert_eq!(carried, projection.coverage);
    assert!(carried.validate().is_ok());
}

#[test]
fn existing_reference_empty_output_has_empty_ranges_and_rejects_invalid_metadata() {
    let empty_reference = reference("empty-existing", "");
    let empty = project_existing_observation(&empty_reference, "read_file", "", 0).unwrap();
    assert!(empty.coverage.output.presented_ranges.is_empty());
    assert!(empty.coverage.output.unpresented_ranges.is_empty());
    assert!(!empty.coverage.output.partial);
    assert!(empty.summary.output.excerpts.is_empty());
    assert!(empty.validate().is_ok());

    let short_output = "abc";
    let short_reference = reference("short-existing", short_output);
    let short = project_existing_observation(
        &short_reference,
        "read_file",
        "current call",
        short_output.len(),
    )
    .unwrap();
    assert!(short.coverage.output.presented_ranges.is_empty());
    assert_eq!(
        short.coverage.output.unpresented_ranges,
        [ByteRange { start: 0, end: 3 }]
    );
    assert!(short.coverage.output.partial);
    assert!(short.validate().is_ok());

    let bad_empty_reference =
        ObservationReference::new("thread-1", "empty-existing", content_sha256("not empty"));
    assert!(project_existing_observation(&bad_empty_reference, "read_file", "", 0).is_err());

    let invalid_digest = ObservationReference::new("thread-1", "existing", "bad-digest".to_owned());
    assert!(project_existing_observation(&invalid_digest, "read_file", "call", 12).is_err());

    let nonempty_reference = reference("existing", "original");
    let mut invalid =
        project_existing_observation(&nonempty_reference, "read_file", "call", "original".len())
            .unwrap();
    invalid.coverage.output.unpresented_ranges[0].start = 1;
    assert!(invalid.coverage.validate().is_err());
}

#[test]
fn existing_v2_coverage_roundtrips_without_reprojection_or_raw_text() {
    let call_text = "c".repeat(3_000);
    let output_text = "o".repeat(4_000);
    let projection = project_observation(
        &reference("v2-existing", &output_text),
        "custom_read",
        &call_text,
        &output_text,
    )
    .unwrap();
    let encoded = serde_json::to_vec(&projection.coverage).unwrap();
    let carried: ObservationCoverage = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(carried, projection.coverage);
    assert!(carried.validate().is_ok());
    assert!(!String::from_utf8_lossy(&encoded).contains(&call_text));
    assert!(!String::from_utf8_lossy(&encoded).contains(&output_text));
}

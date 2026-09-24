//! V2 range retrieval and inventory CLI argument regressions (Annex A F29, Part 2 §59–61).

use super::*;
use clap::Parser;

const THREAD: &str = "00000000-0000-4000-8000-000000000008";
const CALL_ID: &str = "call-f08";
const OUTPUT_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PART_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn evidence_args(range_fields: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec![
        "rencrow-compaction".to_owned(),
        "evidence".to_owned(),
        "--thread".to_owned(),
        THREAD.to_owned(),
        "--call-id".to_owned(),
        CALL_ID.to_owned(),
        "--sha256".to_owned(),
        OUTPUT_SHA.to_owned(),
    ];
    for (flag, value) in range_fields {
        args.push((*flag).to_owned());
        args.push((*value).to_owned());
    }
    args
}

#[test]
fn legacy_full_evidence_argument_shape_stays_parseable_without_range_flags() {
    let parsed = Args::try_parse_from(evidence_args(&[]))
        .expect("legacy full evidence command remains valid");
    let Command::Evidence {
        thread,
        call_id,
        sha256,
        part,
        start,
        end,
        part_sha256,
        ..
    } = parsed.command
    else {
        panic!("expected evidence command");
    };
    std::assert_eq!(thread, THREAD);
    std::assert_eq!(call_id, CALL_ID);
    std::assert_eq!(sha256, OUTPUT_SHA);
    std::assert!(part.is_none());
    std::assert!(start.is_none());
    std::assert!(end.is_none());
    std::assert!(part_sha256.is_none());
}

#[test]
fn evidence_range_arguments_are_all_or_none_and_accept_the_complete_tuple() {
    let fields = [
        ("--part", "output"),
        ("--start", "4"),
        ("--end", "20"),
        ("--part-sha256", PART_SHA),
    ];
    for mask in 1_u8..((1_u8 << fields.len()) - 1) {
        let partial = fields
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, field)| *field)
            .collect::<Vec<_>>();
        std::assert!(
            Args::try_parse_from(evidence_args(&partial)).is_err(),
            "partial range tuple mask {mask:04b} must be rejected"
        );
    }

    let complete = Args::try_parse_from(evidence_args(&fields))
        .expect("all four range flags should be accepted together");
    std::assert!(matches!(complete.command, Command::Evidence { .. }));
}

#[test]
fn invalid_part_name_is_rejected_by_the_shared_part_parser() {
    let args = evidence_args(&[
        ("--part", "arguments"),
        ("--start", "0"),
        ("--end", "1"),
        ("--part-sha256", PART_SHA),
    ]);
    std::assert!(Args::try_parse_from(args).is_err());
}

fn inventory_args(options: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec![
        "rencrow-compaction".to_owned(),
        "inventory".to_owned(),
        "--thread".to_owned(),
        THREAD.to_owned(),
    ];
    for (flag, value) in options {
        args.push((*flag).to_owned());
        args.push((*value).to_owned());
    }
    args
}

#[test]
fn inventory_supports_exact_call_id_or_first_page_or_explicit_zero_continuation() {
    for options in [
        vec![],
        vec![("--call-id", CALL_ID)],
        vec![("--checkpoint-hash", OUTPUT_SHA), ("--offset", "0")],
    ] {
        let parsed = Args::try_parse_from(inventory_args(&options))
            .expect("each supported inventory query shape should parse");
        std::assert!(matches!(parsed.command, Command::Inventory { .. }));
    }
}

#[test]
fn inventory_rejects_partial_pages_and_call_id_pagination_conflicts() {
    for options in [
        vec![("--checkpoint-hash", OUTPUT_SHA)],
        vec![("--offset", "0")],
        vec![
            ("--call-id", CALL_ID),
            ("--checkpoint-hash", OUTPUT_SHA),
            ("--offset", "0"),
        ],
        vec![("--call-id", CALL_ID), ("--offset", "0")],
        vec![("--call-id", CALL_ID), ("--checkpoint-hash", OUTPUT_SHA)],
    ] {
        std::assert!(
            Args::try_parse_from(inventory_args(&options)).is_err(),
            "invalid inventory option combination must be rejected"
        );
    }
}

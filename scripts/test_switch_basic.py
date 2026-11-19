#!/usr/bin/env python3
"""Run the Switch basic regression set after an exact Nextest inventory check."""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST = Path(__file__).with_name("switch-basic-tests.json")
METADATA_OPTIONS = (
    "--archive-file",
    "--cargo-metadata",
    "--workspace-remap",
    "--binaries-metadata",
    "--target-dir-remap",
    "--build-dir-remap",
    "--config-file",
    "--user-config-file",
)
METADATA_ARTIFACT_OPTIONS = (
    "archive_file",
    "cargo_metadata",
    "binaries_metadata",
)
REMAP_OPTIONS = ("workspace_remap", "target_dir_remap", "build_dir_remap")


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--list-only",
        action="store_true",
        help="validate and print the selected test count",
    )
    parser.add_argument("--profile", default="local")
    parser.add_argument("--test-threads")
    parser.add_argument("--retries")
    for option in METADATA_OPTIONS:
        parser.add_argument(option)
    return parser.parse_args()


def load_selection() -> tuple[list[dict[str, str]], str, list[str]]:
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    if data.get("schema_version") != 1:
        raise ValueError("unsupported Switch basic test manifest schema")
    selected: list[dict[str, str]] = []
    for group in data.get("groups", []):
        for case in group.get("tests", []):
            binary_id = case.get("binary_id")
            test_name = case.get("test_name")
            package_name = case.get("package_name")
            if not isinstance(binary_id, str) or not binary_id:
                raise ValueError("manifest case is missing binary_id")
            if not isinstance(test_name, str) or not test_name:
                raise ValueError("manifest case is missing test_name")
            if not isinstance(package_name, str) or not package_name:
                raise ValueError("manifest case is missing package_name")
            selected.append(
                {
                    "binary_id": binary_id,
                    "test_name": test_name,
                    "package_name": package_name,
                }
            )
    expected_count = data.get("test_count")
    if not selected or expected_count != len(selected):
        raise ValueError(
            "manifest has no tests or its test_count does not match its entries"
        )
    pairs = [(case["binary_id"], case["test_name"]) for case in selected]
    if len(set(pairs)) != len(pairs):
        raise ValueError("manifest contains duplicate binary_id/test_name pairs")
    expression = " | ".join(
        f"(binary_id(={case['binary_id']}) & test(={case['test_name']}))"
        for case in selected
    )
    packages = sorted({case["package_name"] for case in selected})
    return selected, expression, packages


def shared_options(args: argparse.Namespace) -> list[str]:
    result = ["--profile", args.profile]
    for option in METADATA_OPTIONS:
        value = getattr(args, option[2:].replace("-", "_"))
        if value is not None:
            result.extend((option, value))
    return result


def uses_metadata_reuse(args: argparse.Namespace) -> bool:
    if (args.binaries_metadata is None) != (args.cargo_metadata is None):
        raise ValueError(
            "--binaries-metadata and --cargo-metadata must be provided together"
        )
    reuse = any(getattr(args, name) is not None for name in METADATA_ARTIFACT_OPTIONS)
    remap = any(getattr(args, name) is not None for name in REMAP_OPTIONS)
    if remap and not reuse:
        raise ValueError(
            "target/workspace remapping requires an archive or metadata reuse input"
        )
    return reuse


def validate_list(
    selected: list[dict[str, str]],
    expression: str,
    packages: list[str],
    args: argparse.Namespace,
) -> None:
    command = [
        "cargo",
        "nextest",
        "list",
        "--no-pager",
        "--message-format",
        "json",
        *shared_options(args),
        "-E",
        expression,
    ]
    metadata_reuse = uses_metadata_reuse(args)
    if not metadata_reuse:
        for package in packages:
            command.extend(("--package", package))
        command.extend(("--build-jobs", "1"))
    env = os.environ.copy()
    env["NEXTEST_PROFILE"] = args.profile
    listed = subprocess.run(
        command,
        cwd=REPO_ROOT / "codex-rs",
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if listed.returncode != 0:
        sys.stderr.write(listed.stdout)
        sys.stderr.write(listed.stderr)
        raise RuntimeError(
            f"cargo nextest list failed with exit code {listed.returncode}"
        )
    try:
        payload: dict[str, Any] = json.loads(listed.stdout)
        suites = payload["rust-suites"]
        if not isinstance(suites, dict):
            raise TypeError("rust-suites is not an object")
        actual: list[dict[str, str]] = []
        for suite in suites.values():
            binary_id = suite["binary-id"]
            package_name = suite["package-name"]
            for test_name, testcase in suite["testcases"].items():
                if testcase.get("filter-match", {}).get("status") == "matches":
                    if testcase.get("kind") != "test":
                        raise ValueError(
                            f"selected item is not a test: {binary_id} {test_name}"
                        )
                    if testcase.get("ignored") is True:
                        raise ValueError(
                            f"selected test is ignored: {binary_id} {test_name}"
                        )
                    actual.append(
                        {
                            "binary_id": binary_id,
                            "test_name": test_name,
                            "package_name": package_name,
                        }
                    )
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise RuntimeError(
            f"could not validate Nextest list output: {error}"
        ) from error
    expected_set = {
        (case["binary_id"], case["test_name"], case["package_name"])
        for case in selected
    }
    actual_set = {
        (case["binary_id"], case["test_name"], case["package_name"]) for case in actual
    }
    if len(actual) != len(actual_set):
        raise RuntimeError("Nextest list returned duplicate selected tests")
    missing = sorted(expected_set - actual_set)
    unexpected = sorted(actual_set - expected_set)
    if missing or unexpected or len(actual) != len(selected):
        raise RuntimeError(
            f"Switch basic selection mismatch (expected {len(selected)}, found {len(actual)}; "
            f"missing={missing}, unexpected={unexpected})"
        )
    print(
        f"Validated {len(actual)} Switch basic tests; no tests will be skipped as missing."
    )


def main() -> int:
    args = arguments()
    try:
        selected, expression, packages = load_selection()
        metadata_reuse = uses_metadata_reuse(args)
        validate_list(selected, expression, packages, args)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"Switch basic test preflight failed: {error}", file=sys.stderr)
        return 2
    if args.list_only:
        return 0

    command = ["just", "test", *shared_options(args), "-E", expression]
    if not metadata_reuse:
        for package in packages:
            command.extend(("--package", package))
        command.extend(("--build-jobs", "1"))
    if args.test_threads is not None:
        command.extend(("--test-threads", args.test_threads))
    if args.retries is not None:
        command.extend(("--retries", args.retries))
    return subprocess.run(command, cwd=REPO_ROOT, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())

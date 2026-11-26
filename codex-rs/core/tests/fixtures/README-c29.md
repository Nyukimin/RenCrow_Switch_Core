# Isolated C29 Supervisor and CLI bridge

The regular capacity-failure test checks native handoff without a Supervisor.
The opt-in bridge runs the pinned Python `run_parent_handoff` against that same
sealed record, then starts the test-owned paired Codex CLI in a PTY. The source
`Config` and CLI both load the test-owned `CODEX_HOME/config.toml`; the launcher
arguments are translated to direct CLI flags while preserving model, reasoning
effort, guard, handoff directory, exact handoff record path, and automation
input author. The CLI's imported successor rollout is checked by the real
Supervisor lineage observer. A local Wiremock Responses endpoint supplies the
one actual continuation turn, and the rollout must contain exactly one
new `task_started` event. The PTY adapter delivers the full message with
bracketed paste, performs the cold-owner and same-process `continuing`
observations while Enter is pending, then submits Enter exactly once. The
input order is saved in the owner result so a multiline recovery memo cannot
split into multiple turns.

For the CLI bridge, the fixture stores only a dummy API key through the
existing File auth store in its temporary `CODEX_HOME`. It trusts only the
temporary workspace using the canonical `project_trust_key`; it does not read
or write the user's HOME or keychain. The CLI child receives no API-key
environment variable.

Set these variables before running the acceptance test:

- `RENCROW_C29_HARNESS`: absolute path to `c29_parent_handoff_harness.py`.
- `RENCROW_TOOLS_ROOT`: a test-owned extracted `git archive` tree or clean
  checkout of RenCrow_Tools.
- `RENCROW_TOOLS_COMMIT`: the exact Tools commit used by that archive.
- `RENCROW_PARENT_HANDOFF_SHA256`: SHA-256 of that archive's
  `tools/observability/ren2_supervisor/parent_handoff.py`.
- `RENCROW_C29_CLI_BIN`: paired test-owned Codex binary path.
- `RENCROW_C29_CLI_COMMIT`: paired binary source commit.
- `RENCROW_C29_CLI_SHA256`: paired binary SHA-256.
- `RENCROW_C29_CLI_MANIFEST`: paired build manifest binding the binary path,
  source commit, and hash.
- `RENCROW_C29_EVIDENCE_DIR` (optional): new test-owned directory to preserve
  the sealed record, config, process log, duplicate-start proof, rollout, and
  Supervisor state with hashes. On Python bridge failure, it preserves the
  exact record, capsule, harness input, Python diagnostics, and CLI log (or an
  explicit absent-log marker) in a new non-overwriting failure directory; no
  auth material is copied.

The fixture checks that changes between the paired CLI source commit and the
test HEAD are limited to Core tests and fixtures. The paired binary path and
hash are verified against its manifest; uncommitted CLI/TUI source changes are
not part of that binary. Nextest's Unix per-test process group is the ownership
boundary. Before starting any child, the Rust test verifies that its PID is
the group leader; the Python bridge verifies that it is a direct child in that
same group. The dummy source, CLI PTY, duplicate-start test, and cold-owner
helper inherit the group, and the result records each observed PGID. Normal
cleanup signals and waits only the exact PIDs returned by this fixture's
`Popen` calls; it never signals the whole group. On a nextest timeout or
interruption, nextest's TERM/KILL applies to the isolated test group and its
inherited children ([nextest timeout contract](https://www.nexte.st/docs/features/slow-tests/)).
The isolated acceptance runner must allow enough time for the fixture's bounded
startup, helper waits, and normal cleanup; extending that runner limit does
not replace per-child cleanup. The test never invokes the installed
`codex-switch` launcher or production Supervisor binding.

From `codex-rs`, run:

```sh
just test --cargo-profile dev-small -p codex-core --features serde_json/preserve_order --lib --retries 0 --test-threads 1 --no-tests fail -E 'test(actual_capacity_compaction_failure_seals_and_cold_restores_parent_handoff)'
```

The opt-in bridge uses `serde_json/preserve_order` so the test producer and
paired CLI consumer use the same JSON object-order feature when checking the
sealed capsule digest. This is a fixture alignment requirement; it does not
establish that persisted handoff digests are compatible across different
serde_json feature sets. The regular non-bridge native test does not require
this opt-in feature flag.

While the ledger is `continuing`, a separate Python process restores the same
ledger/settings/record/rollout and calls the real `run_parent_handoff` in a
read-only adapter. It must return `continuing`, preserve all file hashes, and
make zero sends or commands while the CLI remains alive. The original Python
owner is not stopped, so this proves durable-state reread across processes;
it does not prove an actual Supervisor restart. The test exercises the paired
CLI/TUI with a local mock model response, not a real model service or the full
production launcher/resource-check path. Product progress after the actual
turn remains outside this acceptance boundary.

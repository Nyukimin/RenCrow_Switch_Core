# Isolated C29 Supervisor bridge

The regular capacity-failure test checks the native handoff without starting a
Supervisor. To also exercise the Python Supervisor owner against the same
sealed record and an OS child running the native `ThreadManager`, set these
environment variables before running the test:

- `RENCROW_C29_HARNESS`: absolute path to `c29_parent_handoff_harness.py`.
- `RENCROW_TOOLS_ROOT`: an isolated, clean RenCrow_Tools checkout or archive.
- `RENCROW_TOOLS_COMMIT`: the corresponding Tools commit ID.
- `RENCROW_PARENT_HANDOFF_SHA256`: SHA-256 of that checkout's
  `tools/observability/ren2_supervisor/parent_handoff.py`.
- `RENCROW_C29_EVIDENCE_DIR` (optional): a test-owned directory in which to
  preserve copies of the generated record, logs, and owner state.

From `codex-rs`, run:

```sh
just test --cargo-profile dev-small -p codex-core --lib --retries 0 --test-threads 1 --no-tests fail -E 'test(actual_capacity_compaction_failure_seals_and_cold_restores_parent_handoff)'
```

The fixture owns its dummy source process and native child. The Python host
adapter stubs the TUI and continuation delivery; it does not verify a live
launcher, an actual model turn, or a Supervisor cold restart. Without
`RENCROW_C29_HARNESS`, the regular native test still runs and does not claim
the cross-process bridge was exercised.

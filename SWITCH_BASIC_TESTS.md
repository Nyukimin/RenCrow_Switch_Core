# Switch basic regression set

Run `just test-switch-basic` from the repository root. The command first uses
`cargo nextest list --message-format json` to verify the committed exact
`binary_id`/test-name allowlist, then delegates execution to the existing
`just test` recipe. Missing, renamed, ignored, duplicated, or extra selected
tests stop the command before execution; an empty list cannot pass. Use
`just test-switch-basic --list-only` to perform the same inventory check without
running test cases. Nextest may still compile the selected test binaries when
no archive or metadata reuse option is supplied.

The ordinary path limits Cargo selection to the six packages represented in
the allowlist and uses one build job. Existing Nextest archive and metadata
reuse options are forwarded to both list and run. When a reuse/remap option is
provided, Cargo package/build selectors are omitted because they conflict with
reused metadata; the exact binary/test filter remains in force.

The 45 cases in `scripts/switch-basic-tests.json` cover these Switch contracts:

- Compaction transaction commit, interrupted or uncertain persistence, replay,
  rollback, emergency recovery, and completion receipts.
- Raw rollout reconstruction and checkpoint/cold-resume restoration, including
  same-thread source identity, cutoffs, and exclusion of foreign segments.
- Host-owned task completion, reviewer evidence, and failed-review recovery.
- Role tool allowlists, read-only policy, managed startup policy, and Switch
  thread-title settings.
- The Switch checkpoint CLI's valid selection and malformed-target rejection.

This is a small basic set, not complete Switch or platform coverage. Expand it
when a Switch-owned contract gains a new state transition, persistence format,
recovery path, or external entry point. Add representative success and failure
or recovery cases for that contract, identify them by exact binary and test
name, and preserve the fail-closed count check. Do not add unrelated upstream
tests just to increase the count. This task's user instruction to test only
functionality affected by Switch takes precedence over the upstream default of
running the full suite after broad core changes; the ordinary `just test`
recipe remains unchanged.

For selecting and constructing change-specific tests in addition to this basic
set, follow the change-specific testing procedure in [FORK_RULES.md](FORK_RULES.md).
That procedure owns the impact analysis, reuse/addition decision, exact
selection check, static gates, scoped execution, and acceptance record.

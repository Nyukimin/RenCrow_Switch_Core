<!-- Modified by RenCrow, 2026-10-27. Non-official Codex fork. -->
# Native execution contracts

Base: `e8417aaea4352e3e77f529adaa13a41c7a653afb`. This opt-in Fork feature
observes tool execution in the host. It does not depend on prompt compliance or
external hooks. Unconfigured clients retain upstream behavior. It does not change
sandbox, approvals, authentication, GPU scheduling, or the thread storage format.

The product policy is owned by RenCrow_LLM `docs/07`; persistent defaults live in
`examples/codex/rencrow.config.toml.example` and `examples/codex/roles/` there.
The launcher regenerates the profile and copies roles on each start.

```toml
rencrow_execution_guard = { max_read_only_calls = 24, max_repeated_reads = 3, require_child_results = true }
```

`max_read_only_calls` remains accepted for configuration compatibility. It no
longer stops ordinary distinct reads by total count; it bounds diagnostics tied
to a failed current diff. `max_repeated_reads` still limits the same command in
the same working directory until accepted progress.

Role configuration can reduce the diagnostic and repeated-read limits and add requirements;
it cannot remove an inherited requirement or increase a limit. Worker/default
cap failed-check diagnostics at 12 reads, require an accepted patch and a passing
check, and reject direct agent CLI launches. Verifier requires a passing check;
explorer/analyst retain read-only investigation. Existing role tool restrictions
still apply.

Typed task contracts use `task_kind = "implement" | "verify" | "read_only" | "orchestrate"`.
When present, the role task kind takes precedence over the inherited task kind.
`implement` requires an edit and a passing check; `verify` requires a passing
check without an edit; `read_only` requires neither. `orchestrate` coordinates
work: a turn with no current-turn diff and no delegated implementation child
does not require a patch or check. A current-turn diff or implementation-child
delegation creates the existing completion requirement: current-turn diff or a
host-confirmed successful implementation result, followed by a fresh passing
parent check. The implementation-child obligation persists if that worker
fails or is closed, and read-only/verify children do not create it. Explicit
`require_edit` and `require_check` settings still apply. Every task kind still
collects explicitly required child results. Failed, pending, or started checks
block Orchestrate turns and tasks whose contract requires a check; ReadOnly may
report a failed investigation check without being forced to rerun it. A role that
sets `read_only` together with `require_edit = true` is rejected as contradictory.
Without `task_kind`, legacy `require_edit` and `require_check` behavior remains.
Limits and nested CLI restrictions continue to combine by narrowing (minimum/OR).

A passing check resets repeated-read counts only once for each previously
unverified current-turn diff stamp; a new host-verified child result also resets
them. A small patch alone, repeating a passing check, assistant claims, and
reading a test log do not reset them. Identical commands include the working
directory in their identity. Unclassified shell commands also count as reads;
known checks and polling an existing process remain exempt. Failed checks permit
bounded diagnostic reads for the current diff at
most once per stamp and for at most two stamps per turn. Refused commands return
`RENCROW_READ_BUDGET_EXCEEDED` before execution. The third refusal ends the turn
through its ordinary terminal error path, without another model request or a
panic. This progress guard does not change permissions or sandbox policy and is
not a security boundary.

`require_edit` means a nonempty current-turn `TurnDiffTracker` diff from the
built-in patch tool. Shell-written files and changes from a different child or
previous turn are not evidence. Use worker for implementation, explorer/analyst
for investigation; a legitimate no-change worker task must be returned as an
error or reassigned. It must not invent a patch just to pass.

With `require_edit`, the host rejects clear `cp`/`mv`, file output redirects,
and limited Python `-c` or quoted-heredoc writes before execution, then directs
the model to `apply_patch`. This bounded preflight recognizes `Path.write_text`
and `write_bytes`, plus `open` calls with literal `w`, `a`, `x`, or `+` modes;
it does not infer variable modes or promise detection of arbitrary scripts.
These refusals count against the existing repeated-read budget.

`require_check` requires a real structured exec result with exit code zero after
the latest tracked edit. A later edit invalidates the check. A failed check must
be rerun successfully with the same command; a different passing command cannot
hide it. Async checks remain pending until `write_stdin` returns their exit code;
a process started before an edit cannot verify that edit. Supported conservative
forms: `go test/build/vet`, `cargo test/check/build/clippy`, `cargo nextest run`,
`node --check`, `pytest`, `npm test`, `python[3] [-B] -m pytest/unittest/compileall`,
`python[3] [-B] -m py_compile`, `just test/fmt-check`, `git diff --check`. Environment assignments and `cd DIR &&`
prefixes are accepted. A bare trailing `2>&1` is also accepted because it
preserves the check's exit status. Pipelines, other redirection, `|| true`,
extra trailing commands, and arbitrary test wrappers do not count. Run a
supported check directly.
This proves that a check ran, not that it tests the requested feature adequately;
review and acceptance testing remain necessary.
If an otherwise unrecognized simple-command sequence contains a supported check,
the host refuses the sequence and asks to run that check directly in the same
working directory and environment. A recognized `cd DIR && CHECK` invocation
remains valid.

With `require_child_results`, V1 spawn/send_input/resume receipts add children to
the current-turn collection set. `wait_agent` with Completed/Errored/Shutdown or a
successful explicit `close_agent` removes them. Timeout, Running, Interrupted,
and NotFound do not count as collected. Cancellation is not implementation
success. The set is per turn and not persisted across turns/process restarts;
children inherited from an earlier turn are included when contacted again.
V2 collection is unsupported and explicitly rejects completion with this flag.
Other conditions work without V1. There is no forced spawning: dependency/path
assignment and semantic review remain the orchestrator's responsibility.

`deny_nested_agent_cli` rejects direct `codex`, `codex-switch`,
`rencrow-switch-core`, and `claude` commands (including absolute Unix paths),
except a standalone help/version query. It is a workflow guard, not a replacement
for the sandbox: arbitrary script/wrapper detection is deliberately not claimed.

At ordinary turn stop, missing receipts produce
`RENCROW_EXECUTION_CONTRACT_INCOMPLETE`, an error event and Errored agent status,
instead of Completed. The rejection does not loop by injecting another prompt.
Internal summarization turns are exempt. A new user turn begins with fresh
receipts; the model must perform the required work in that turn.

Cold V1 resume now reapplies the recorded role under current parent restrictions,
restores stored model/provider/effort, and preserves the current parent sandbox,
approval reviewer and working directory. A missing provider fails instead of
falling back to the parent route. An unspecified default role is also recorded
for V1 when its role file is configured. Legacy metadata without a role is not
reinterpreted as a guessed role. V2's existing resume path is retained.

Validation uses real mocked Responses/tool-loop integration (repeated read
rejection, prose-only error, actual patch plus passing check), state-machine unit
tests (failed/async check invalidation, read budgets, child collection), and
resume configuration tests. See the work report for exact executed results;
Linux/Windows runtime behavior is not claimed by macOS testing.

To disable, remove the guard field from both the canonical main and applicable
role templates, regenerate/restart, and ensure no writer still uses the old
configuration. It adds no persisted receipt store. Restore a backed-up binary
only after stopping the current writer; binary replacement alone does not
resolve in-flight child work. Upstream can replace this module when it offers
equivalent host-enforced role progress and V1 resume invariants.

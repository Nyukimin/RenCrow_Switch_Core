// Modified by RenCrow: host-observed progress, bounded reads and completion contracts.
//! No prompt inference, external hooks, permissions changes, or new persistent store.
use crate::function_tool::FunctionCallError;
use crate::memory_usage::shell_script_for_invocation;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use codex_config::RencrowExecutionGuard;
use codex_config::RencrowTaskKind;
use codex_protocol::error::CodexErr;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_shell_command::parse_command::parse_shell_script;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::hash::Hasher;

pub(crate) const PATCH_RETRYABLE_ERROR_MARKER: &str = "RENCROW_PATCH_RETRYABLE:";
pub(crate) const PATCH_SYNTAX_FAILURE_KIND: &str = "syntax:";
pub(crate) const PATCH_CONTEXT_FAILURE_KIND: &str = "context:";
const MAX_FAILED_CHECKS_IN_ERROR: usize = 3;
const MAX_FAILED_CHECK_COMMAND_CHARS: usize = 160;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuardExecutionEvidence {
    pub(crate) thread_id: codex_protocol::ThreadId,
    pub(crate) turn_id: String,
    pub(crate) parent_thread_id: codex_protocol::ThreadId,
    pub(crate) diff_stamp: u64,
    pub(crate) has_diff: bool,
    pub(crate) checked: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GuardState {
    reads: u16,
    repeated: HashMap<String, u16>,
    diff_stamp: u64,
    diff_initialized: bool,
    diff_nonempty: bool,
    verified_diff_stamps: HashSet<u64>,
    diagnostic_diff_stamps: HashSet<u64>,
    diagnostic_reads_stamp: Option<u64>,
    baseline_failure_reads_stamp: Option<u64>,
    child_revision: u64,
    budget_rejections: u8,
    patch_verification_failures: u8,
    checked: bool,
    failed_checks: HashSet<String>,
    uncredited_python_test: Option<(String, u64)>,
    pending_checks: HashMap<i64, (String, u64)>,
    started_checks: HashMap<String, (String, u64)>,
    children: HashSet<String>,
    child_results: HashSet<(codex_protocol::ThreadId, String)>,
    implementation_child_delegated: bool,
}

impl GuardState {
    fn charge_read(&mut self, policy: &RencrowExecutionGuard, key: &str) -> Result<(), String> {
        let repeated =
            self.repeated.get(key).copied().unwrap_or(0) >= policy.max_repeated_reads.get();
        let current_diff_diagnostics = self.diagnostic_reads_stamp == Some(self.diff_stamp);
        let baseline_failure_budget = self.baseline_failure_reads_stamp == Some(self.diff_stamp);
        let read_budget_exhausted = (current_diff_diagnostics || baseline_failure_budget)
            && self.reads >= policy.max_read_only_calls.get();
        if repeated || read_budget_exhausted {
            let requires_edit = policy.requires_edit();
            let requires_check = self.requires_check_for_turn(policy, self.diff_nonempty);
            let recovery = match (requires_edit, requires_check) {
                (false, false) => {
                    "Do not make further reads. If this contract permits completion without a patch or check, report evidence-backed findings or a plan now; if more investigation is essential, use allowed delegation and collect its result. State any unknowns. Do not invent a patch or run an unrelated check."
                }
                (true, true) => {
                    "Do not make further reads. Use the available evidence to make the required edit, then run and pass the required check directly. If investigation is still essential, use allowed delegation and collect its result. Do not invent a patch."
                }
                (true, false) => {
                    "Do not make further reads. Use the available evidence to make the required edit. If investigation is still essential, use allowed delegation and collect its result. Do not invent a patch."
                }
                (false, true) => {
                    "Do not make further reads. Run and pass the required check directly, then complete with evidence-backed results. If investigation is still essential, use allowed delegation and collect its result."
                }
            };
            let reason = if repeated {
                "same read command repeated without accepted progress"
            } else if current_diff_diagnostics {
                "bounded diagnostics for the failed current diff are exhausted"
            } else {
                "bounded read budget after failed checks of the unmodified baseline is exhausted"
            };
            return Err(format!(
                "RENCROW_READ_BUDGET_EXCEEDED: {reason}; command not executed. {recovery}"
            ));
        }
        if current_diff_diagnostics || baseline_failure_budget {
            self.reads = self.reads.saturating_add(1);
        }
        *self.repeated.entry(key.to_owned()).or_default() += 1;
        Ok(())
    }

    fn observe_diff(&mut self, diff: Option<&str>) {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        diff.hash(&mut hash);
        let stamp = hash.finish();
        if self.diff_stamp != stamp {
            if self.diff_initialized {
                self.patch_verification_failures = 0;
            }
            self.diff_stamp = stamp;
            self.checked = false;
            self.diagnostic_reads_stamp = None;
            self.baseline_failure_reads_stamp = None;
            self.uncredited_python_test = None;
            self.reads = 0;
        }
        self.diff_initialized = true;
        self.diff_nonempty = diff.is_some_and(|diff| !diff.is_empty());
    }

    fn reset_reads(&mut self) {
        self.reads = 0;
        self.repeated.clear();
    }

    fn check_stamp(&self) -> u64 {
        if self.child_revision == 0 {
            return self.diff_stamp;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.diff_stamp.hash(&mut hasher);
        self.child_revision.hash(&mut hasher);
        hasher.finish()
    }

    fn record_budget_rejection(&mut self) -> bool {
        self.budget_rejections = self.budget_rejections.saturating_add(1);
        self.budget_rejections >= 3
    }

    fn record_patch_verification_failure(&mut self, diff: Option<&str>) -> bool {
        self.observe_diff(diff);
        self.patch_verification_failures = self.patch_verification_failures.saturating_add(1);
        self.patch_verification_failures >= 3
    }

    fn finish_check(&mut self, key: &str, stamp: u64, exit_code: i64) {
        if stamp != self.check_stamp() {
            return;
        }
        if exit_code != 0 {
            self.failed_checks.insert(key.to_owned());
            if self.diff_nonempty && !self.recover_diagnostic_reads(self.diff_stamp) {
                self.diagnostic_reads_stamp = Some(self.diff_stamp);
                self.reads = u16::MAX;
            } else if !self.diff_nonempty
                && self.baseline_failure_reads_stamp != Some(self.diff_stamp)
            {
                // A failed check against an unmodified baseline is not evidence that the
                // current-turn patch is broken. Keep the failed check outstanding, but allow
                // bounded diagnosis under the ordinary read limit.
                self.baseline_failure_reads_stamp = Some(self.diff_stamp);
                self.reads = 0;
            }
        }
        if exit_code == 0 {
            self.failed_checks.remove(key);
            self.checked = true;
            self.uncredited_python_test = None;
            self.diagnostic_reads_stamp = None;
            self.baseline_failure_reads_stamp = None;
            self.reads = 0;
            if self.verified_diff_stamps.insert(self.diff_stamp) {
                self.budget_rejections = 0;
                self.reset_reads();
                self.diagnostic_diff_stamps.clear();
            }
        }
    }

    fn record_uncredited_python_test(&mut self, command: &str) {
        self.uncredited_python_test =
            Some((command.chars().take(200).collect(), self.check_stamp()));
    }

    fn uncredited_python_test(&self) -> Option<&str> {
        self.uncredited_python_test
            .as_ref()
            .filter(|(_, stamp)| *stamp == self.check_stamp())
            .map(|(command, _)| command.as_str())
    }

    fn cancel_started_check(&mut self, call_id: &str) {
        self.started_checks.remove(call_id);
    }

    fn recover_diagnostic_reads(&mut self, stamp: u64) -> bool {
        if self.diff_stamp != stamp || !self.diff_initialized || !self.diff_nonempty {
            return false;
        }
        if self.diagnostic_diff_stamps.len() >= 2 || !self.diagnostic_diff_stamps.insert(stamp) {
            return false;
        }
        self.reset_reads();
        self.diagnostic_reads_stamp = Some(stamp);
        true
    }

    fn record_child_progress(&mut self) {
        self.checked = false;
        self.child_revision = self.child_revision.saturating_add(1);
        self.budget_rejections = 0;
        self.repeated.clear();
    }

    fn observe_child_result(&mut self, tool: &str, args: &Value, value: &Value) {
        match tool {
            "spawn_agent" => {
                if let Some(id) = value["agent_id"].as_str() {
                    self.children.insert(id.to_owned());
                }
            }
            "resume_agent" => {
                if let Some(id) = args["id"].as_str() {
                    if matches!(
                        serde_json::from_value::<AgentStatus>(value["status"].clone()),
                        Ok(AgentStatus::Completed(_)
                            | AgentStatus::Errored(_)
                            | AgentStatus::Shutdown)
                    ) {
                        self.children.remove(id);
                    } else {
                        self.children.insert(id.to_owned());
                    }
                }
            }
            "send_input" => {
                if let Some(id) = args["target"].as_str() {
                    self.children.insert(id.to_owned());
                }
            }
            "close_agent" => {
                if let Some(id) = args["target"].as_str() {
                    self.children.remove(id);
                }
            }
            "wait_agent" => {
                if let Some(statuses) = value["status"].as_object() {
                    for (id, status) in statuses {
                        if serde_json::from_value::<AgentStatus>(status.clone()).is_ok_and(
                            |status| {
                                matches!(
                                    status,
                                    AgentStatus::Completed(_)
                                        | AgentStatus::Errored(_)
                                        | AgentStatus::Shutdown
                                )
                            },
                        ) {
                            self.children.remove(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn child_recovery_prompt(&self, attempt: &mut u8) -> Option<String> {
        if self.children.is_empty() || *attempt >= 2 {
            return None;
        }
        *attempt += 1;
        let mut ids = self.children.iter().cloned().collect::<Vec<_>>();
        ids.sort();
        Some(format!(
            "RENCROW_CHILD_RECOVERY {}/2: delegated results remain uncollected: {}. Do not finish yet. Use wait_agent to collect terminal results or explicitly close unwanted children. A wait timeout is not failure; inspect status before interrupting. Do not spawn replacements or repeat unknown-outcome tool operations. Preserve existing edits, role/model/effort and permissions, then continue the original task. This host reminder does not authorize cancellation of useful work or bypass any guard.",
            attempt,
            ids.join(", ")
        ))
    }

    fn completion_recovery_prompt(
        &self,
        policy: &RencrowExecutionGuard,
        has_diff: bool,
        attempt: &mut u8,
    ) -> Option<String> {
        if *attempt >= 1 {
            return None;
        }
        let missing = self.missing(policy, has_diff);
        if missing.is_empty() {
            return None;
        }
        let check_guidance = self
            .uncredited_python_test()
            .filter(|_| {
                missing.iter().any(|item| {
                    *item == "successful check after the last edit (failed checks must be rerun)"
                })
            })
            .map(|command| {
                format!(
                    " A successful exec of `{command}` was observed but was not classified as a check; use a recognized direct test runner such as `python3 -m unittest -v tests` or `pytest -q <test_path>`."
                )
            })
            .unwrap_or_default();
        *attempt += 1;
        let requires_check = self.requires_check_for_turn(policy, has_diff);
        let action = if policy.task_kind == Some(RencrowTaskKind::Orchestrate)
            && !requires_check
            && (!self.failed_checks.is_empty()
                || !self.pending_checks.is_empty()
                || !self.started_checks.is_empty())
        {
            "resolve pending or started checks and rerun failed checks successfully"
        } else if policy.task_kind == Some(RencrowTaskKind::Orchestrate)
            && self.implementation_child_delegated
            && !has_diff
        {
            "collect a successful implementation result from a child and run a passing check"
        } else {
            match (policy.requires_edit(), requires_check) {
                (true, true) => {
                    "make the required edit, then run and pass a check after the last edit"
                }
                (true, false) => {
                    "make the required edit and do not claim completion until its diff is present"
                }
                (false, true) => "run and pass the required check after the last edit",
                (false, false) => "continue until the missing delegated results are collected",
            }
        };
        Some(format!(
            "RENCROW_EXECUTION_RECOVERY {}/1: completion is missing {}. Continue this task now: {}. Do not claim completion until the host-observed evidence is present. Preserve the current model, effort, tools, sandbox, and permissions.{}",
            attempt,
            missing.join("; "),
            action,
            check_guidance
        ))
    }

    fn missing(&self, policy: &RencrowExecutionGuard, has_diff: bool) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if policy.requires_edit() && !has_diff {
            missing.push("accepted current-turn patch diff");
        }
        let orchestrate = policy.task_kind == Some(RencrowTaskKind::Orchestrate);
        let requires_check = self.requires_check_for_turn(policy, has_diff);
        let outstanding_checks = !self.failed_checks.is_empty()
            || !self.pending_checks.is_empty()
            || !self.started_checks.is_empty();
        if (requires_check && (!self.checked || outstanding_checks))
            || (orchestrate && outstanding_checks)
        {
            missing.push("successful check after the last edit (failed checks must be rerun)");
        }
        if policy.requires_child_results() {
            if !self.children.is_empty() {
                missing.push("collected or explicitly closed delegated children");
            }
            if orchestrate
                && self.implementation_child_delegated
                && !has_diff
                && self.child_results.is_empty()
            {
                missing.push("current-turn patch diff or host-confirmed successful implementation result from a child");
            }
        }
        missing
    }

    fn failed_check_diagnostic(&self) -> String {
        if self.failed_checks.is_empty() {
            return String::new();
        }
        let mut checks = self.failed_checks.iter().collect::<Vec<_>>();
        checks.sort();
        let details = checks
            .into_iter()
            .take(MAX_FAILED_CHECKS_IN_ERROR)
            .map(|command| safe_check_command_display(command))
            .collect::<Vec<_>>();
        let omitted = self.failed_checks.len().saturating_sub(details.len());
        let suffix = if omitted > 0 {
            format!("; {omitted} more omitted")
        } else {
            String::new()
        };
        format!("; unresolved failed checks: {}{suffix}", details.join("; "))
    }

    fn requires_check_for_turn(&self, policy: &RencrowExecutionGuard, has_diff: bool) -> bool {
        policy.requires_check()
            || (policy.task_kind == Some(RencrowTaskKind::Orchestrate)
                && (has_diff
                    || self.implementation_child_delegated
                    || !self.child_results.is_empty()))
    }
}

fn safe_check_command_display(command: &str) -> String {
    let lower = command.to_ascii_lowercase();
    let secret_flags = [
        "--token",
        "--password",
        "--passwd",
        "--api-key",
        "--apikey",
        "--secret",
        "--authorization",
    ];
    let secret_assignments = [
        "token=",
        "password=",
        "passwd=",
        "api_key=",
        "apikey=",
        "secret=",
        "authorization=",
    ];
    let contains_secret = secret_flags.iter().any(|flag| {
        lower.match_indices(flag).any(|(index, _)| {
            lower[index + flag.len()..]
                .chars()
                .next()
                .is_some_and(|next| matches!(next, '=' | ' ' | '\t'))
        })
    }) || secret_assignments
        .iter()
        .any(|marker| lower.contains(marker))
        || lower.contains("bearer ")
        || (lower.contains("://") && lower.contains('@'));
    let display = if contains_secret {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        command.hash(&mut hasher);
        format!("[redacted command {:016x}]", hasher.finish())
    } else {
        command.to_owned()
    };
    if display.chars().count() <= MAX_FAILED_CHECK_COMMAND_CHARS {
        display
    } else {
        format!(
            "{}…",
            display
                .chars()
                .take(MAX_FAILED_CHECK_COMMAND_CHARS)
                .collect::<String>()
        )
    }
}

fn child_execution_evidence_eligible(
    policy: &RencrowExecutionGuard,
    state: &GuardState,
    has_diff: bool,
) -> bool {
    has_diff
        && state.checked
        && (policy.requires_edit() && policy.requires_check()
            || (policy.task_kind == Some(RencrowTaskKind::Orchestrate)
                && state.requires_check_for_turn(policy, has_diff)))
}

fn read_only(command: &str) -> bool {
    let commands = parse_shell_script(command);
    !commands.is_empty()
        && commands.iter().all(|command| match command {
            ParsedCommand::Read { .. }
            | ParsedCommand::ListFiles { .. }
            | ParsedCommand::Search { .. } => true,
            ParsedCommand::Unknown { cmd } => shlex::split(cmd).is_some_and(|words| {
                matches!(
                    words.first().map(String::as_str),
                    Some(
                        "cd" | "pwd"
                            | "strings"
                            | "wc"
                            | "stat"
                            | "file"
                            | "echo"
                            | "printf"
                            | "head"
                            | "tail"
                            | "cut"
                    )
                ) && !cmd.contains([';', '|', '&', '>', '<', '$', '`', '\n'])
            }),
        })
}

/// Deliberately conservative: a passing shell wrapper must not hide a failed check.
fn check_command(command: &str) -> bool {
    if command.contains(['\n', '\r']) {
        return false;
    }

    // A lone stderr-to-stdout redirect preserves the command's exit status.
    // Strip only this exact unquoted suffix; all other redirection syntax
    // remains rejected by the conservative scanner below.
    let command = command.strip_suffix(" 2>&1").unwrap_or(command);

    // Split only on real shell `&&` operators. Quoted metacharacters are
    // arguments, while expansions in double quotes remain active shell syntax.
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                '$' | '`' => return false,
                _ => {}
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => escaped = true,
                '$' | '`' | ';' | '|' | '>' | '<' | '(' | ')' | '{' | '}' | '*' | '?' | '['
                | '~' | '#' => return false,
                '&' => {
                    if chars.peek().is_some_and(|(_, next)| *next == '&') {
                        chars.next();
                        parts.push(&command[start..index]);
                        start = index + 2;
                    } else {
                        return false;
                    }
                }
                _ => {}
            },
        }
    }
    if quote.is_some() || escaped {
        return false;
    }
    parts.push(&command[start..]);
    if parts
        .iter()
        .take(parts.len().saturating_sub(1))
        .any(|part| !shlex::split(part).is_some_and(|words| words.len() == 2 && words[0] == "cd"))
    {
        return false;
    }
    let Some(words) = parts.last().and_then(|part| shlex::split(part)) else {
        return false;
    };
    let words = words
        .iter()
        .skip_while(|word| {
            word.split_once('=').is_some_and(|(key, _)| {
                key.chars().next().is_some_and(|first| {
                    (first.is_ascii_alphabetic() || first == '_')
                        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            })
        })
        .map(String::as_str)
        .collect::<Vec<_>>();
    match words.as_slice() {
        [script] if is_execution_guard_script(script) => return true,
        ["bash", script] if is_execution_guard_script(script) => return true,
        _ => {}
    }
    let program = words.first().copied().unwrap_or_default();
    let base_program = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let normalized_program = if is_python_executable(base_program) {
        "python3"
    } else {
        base_program
    };
    let words = std::iter::once(normalized_program)
        .chain(words.into_iter().skip(1))
        .collect::<Vec<_>>();
    match words.as_slice() {
        ["go", "test" | "build" | "vet", ..] => true,
        ["cargo", "test" | "check" | "build" | "clippy", ..] => true,
        ["cargo", "nextest", "run", ..] => true,
        ["cargo", version, "test" | "check" | "build" | "clippy", ..]
            if version.starts_with('+') =>
        {
            true
        }
        ["node", "--check", ..] | ["node", "--test", ..] | ["pytest", ..] | ["npm", "test", ..] => {
            true
        }
        ["rencrow-resource-check", "--", ..] => true,
        [
            "python" | "python3",
            "-B",
            "-m",
            "pytest" | "unittest" | "compileall" | "py_compile",
            ..,
        ] => true,
        [
            "python" | "python3",
            "-m",
            "pytest" | "unittest" | "compileall",
            ..,
        ] => true,
        ["python" | "python3", "-m", "py_compile", ..] => true,
        ["just", "test" | "fmt-check", ..] | ["git", "diff", "--check", ..] => true,
        _ => false,
    }
}

pub(crate) fn is_task_evidence_check_command(command: &str) -> bool {
    check_command(command)
}

fn direct_check_in_compound_command(command: &str) -> Option<String> {
    if check_command(command) {
        return None;
    }
    codex_shell_command::bash::parse_shell_script_into_commands(command)?
        .into_iter()
        .map(|words| codex_shell_command::parse_command::shlex_join(&words))
        .find(|part| check_command(part))
}

/// Returns whether a Python test-named script was run directly without a recognized test runner.
/// This is diagnostic only; it never makes the invocation count as a check or blocks it.
fn unrecognized_python_test_script(command: &str) -> bool {
    codex_shell_command::bash::parse_shell_script_into_commands(command).is_some_and(|commands| {
        commands.into_iter().any(|words| {
            let words = words
                .iter()
                .skip_while(|word| {
                    word.split_once('=').is_some_and(|(key, _)| {
                        key.chars()
                            .next()
                            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                })
                .map(String::as_str)
                .collect::<Vec<_>>();
            let Some(program) = words.first().and_then(|word| word.rsplit('/').next()) else {
                return false;
            };
            if !is_python_executable(program) {
                return false;
            }
            let script_index = if words.get(1) == Some(&"-B") { 2 } else { 1 };
            let Some(script) = words.get(script_index) else {
                return false;
            };
            let Some(name) = std::path::Path::new(script)
                .file_name()
                .and_then(|name| name.to_str())
            else {
                return false;
            };
            name == "tests.py"
                || (name.ends_with(".py")
                    && (name
                        .strip_suffix(".py")
                        .is_some_and(|stem| stem.starts_with("test_") || stem.ends_with("_test"))))
        })
    })
}

fn is_execution_guard_script(path: &str) -> bool {
    let path = path.strip_prefix("./").unwrap_or(path);
    let Some(name) = path.strip_prefix("scripts/") else {
        return false;
    };
    if name == "e2e-static.sh" {
        return true;
    }
    let Some(stem) = name.strip_suffix(".sh") else {
        return false;
    };
    let Some(suffix) = stem
        .strip_prefix("test-")
        .or_else(|| stem.strip_prefix("e2e-"))
    else {
        return false;
    };
    !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_python_executable(program: &str) -> bool {
    let program = program.to_ascii_lowercase();
    if program == "python" || program == "python3" {
        return true;
    }
    let Some(version) = program.strip_prefix("python3.") else {
        return false;
    };
    !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
}

fn nested_agent_cli(command: &str) -> bool {
    parse_shell_script(command).iter().any(|command| {
        let text = match command {
            ParsedCommand::Unknown { cmd } | ParsedCommand::Read { cmd, .. }
            | ParsedCommand::ListFiles { cmd, .. } | ParsedCommand::Search { cmd, .. } => cmd,
        };
        let Some(words) = shlex::split(text) else { return false; };
        let Some(program) = words.first().and_then(|word| word.rsplit('/').next()) else { return false; };
        matches!(program, "codex" | "codex-switch" | "rencrow-switch-core" | "claude")
            && !matches!(words.as_slice(), [_, option] if matches!(option.as_str(), "--version" | "-V" | "--help" | "-h"))
    })
}

fn arguments(invocation: &ToolInvocation) -> Value {
    match &invocation.payload {
        ToolPayload::Function { arguments } => {
            serde_json::from_str(arguments).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

fn implementation_child_requires_edit(effective_requires_edit: Option<bool>) -> bool {
    effective_requires_edit.unwrap_or(true)
}

pub(crate) async fn before_tool(invocation: &ToolInvocation) -> Result<(), FunctionCallError> {
    let Some(policy) = invocation.turn.config.rencrow_execution_guard.as_ref() else {
        return Ok(());
    };
    if matches!(invocation.turn.session_source, SessionSource::Internal(_)) {
        return Ok(());
    }
    let Some(command) = shell_script_for_invocation(invocation) else {
        return Ok(());
    };
    let is_shell_edit =
        policy.requires_edit() && crate::rencrow_shell_edit_guard::is_obvious_shell_edit(&command);
    if policy.deny_nested_agent_cli && nested_agent_cli(&command) {
        return Err(FunctionCallError::RespondToModel("RENCROW_NESTED_AGENT_DENIED: executor cannot start another agent CLI; command not executed".into()));
    }
    let is_read = read_only(&command);
    let is_check = check_command(&command);
    let embedded_direct_check = (!is_check)
        .then(|| direct_check_in_compound_command(&command))
        .flatten();
    let diff = invocation.tracker.lock().await.get_unified_diff();
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        active.as_ref().map(|active| active.turn_state.clone())
    };
    let Some(turn_state) = turn_state else {
        return Err(FunctionCallError::RespondToModel(
            "RENCROW_EXECUTION_GUARD: no active turn".into(),
        ));
    };
    let mut turn = turn_state.lock().await;
    turn.rencrow_execution_guard.observe_diff(diff.as_deref());
    if is_read || !is_check {
        // Cwd is part of the identity; identical relative reads in different roots are distinct.
        let args = arguments(invocation);
        let key = format!(
            "{:?}:{}:{}",
            invocation.turn.config.cwd,
            args["workdir"],
            command.trim()
        );
        if let Err(message) = turn.rencrow_execution_guard.charge_read(policy, &key) {
            let state = &mut turn.rencrow_execution_guard;
            if state.record_budget_rejection() {
                return Err(FunctionCallError::Fatal(format!(
                    "RENCROW_READ_BUDGET_EXCEEDED: repeated host refusals stopped the turn; {message}"
                )));
            }
            return Err(FunctionCallError::RespondToModel(message));
        }
    }
    if is_shell_edit {
        return Err(FunctionCallError::RespondToModel(
            crate::rencrow_shell_edit_guard::EDIT_METHOD_REQUIRED.into(),
        ));
    }
    if let Some(check) = embedded_direct_check {
        let check = check.chars().take(240).collect::<String>();
        return Err(FunctionCallError::RespondToModel(format!(
            "RENCROW_DIRECT_CHECK_REQUIRED: command not executed; run the check as a separate direct shell invocation with the same working directory and environment. Detected check (up to 240 characters): {check}"
        )));
    }
    if is_check {
        let state = &mut turn.rencrow_execution_guard;
        let args = arguments(invocation);
        let key = if invocation.turn.config.rencrow_autonomous_completion
            && let Some(evidence) = args.get("task_evidence")
            && let Ok(request) =
                serde_json::from_value::<crate::task_check::TaskEvidenceRequest>(evidence.clone())
            && let Some(scope) = invocation.session.active_task_scope().await
            && scope.task_id == request.task_id
            && scope.task_revision == request.task_revision
        {
            let configured_cwd = invocation.turn.config.cwd.to_path_buf();
            let requested_cwd = args["workdir"]
                .as_str()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| configured_cwd.clone());
            let cwd = if requested_cwd.is_absolute() {
                requested_cwd
            } else {
                configured_cwd.join(requested_cwd)
            };
            crate::task_check::TaskCheckObligation::new(
                scope,
                cwd,
                command.trim(),
                request.artifact_paths,
            )
            .map(|obligation| format!("task-evidence:{}", obligation.key()))
            .unwrap_or_else(|| {
                format!(
                    "{:?}:{}:{}",
                    invocation.turn.config.cwd,
                    args["workdir"],
                    command.trim()
                )
            })
        } else {
            format!(
                "{:?}:{}:{}",
                invocation.turn.config.cwd,
                args["workdir"],
                command.trim()
            )
        };
        state
            .started_checks
            .insert(invocation.call_id.clone(), (key, state.check_stamp()));
    }
    Ok(())
}

pub(crate) async fn cancel_unexecuted_task_evidence_check(session: &Session, call_id: &str) {
    let turn_state = {
        let active = session.active_turn.lock().await;
        active.as_ref().map(|active| active.turn_state.clone())
    };
    let Some(turn_state) = turn_state else {
        return;
    };
    turn_state
        .lock()
        .await
        .rencrow_execution_guard
        .cancel_started_check(call_id);
}

pub(crate) async fn patch_verification_failure(
    invocation: &ToolInvocation,
    error: FunctionCallError,
) -> FunctionCallError {
    let message = match error {
        FunctionCallError::RespondToModel(message) => message,
        other => return other,
    };
    if !message.starts_with(PATCH_RETRYABLE_ERROR_MARKER) {
        return FunctionCallError::RespondToModel(message);
    }
    let retryable_message = message
        .trim_start_matches(PATCH_RETRYABLE_ERROR_MARKER)
        .trim_start()
        .to_owned();
    let (failure_kind, message) =
        if let Some(message) = retryable_message.strip_prefix(PATCH_SYNTAX_FAILURE_KIND) {
            (PATCH_SYNTAX_FAILURE_KIND, message.trim_start().to_owned())
        } else if let Some(message) = retryable_message.strip_prefix(PATCH_CONTEXT_FAILURE_KIND) {
            (PATCH_CONTEXT_FAILURE_KIND, message.trim_start().to_owned())
        } else {
            return FunctionCallError::RespondToModel(retryable_message);
        };
    if invocation.turn.config.rencrow_execution_guard.is_none()
        || matches!(invocation.turn.session_source, SessionSource::Internal(_))
        || invocation.tool_name.name != "apply_patch"
        || !matches!(&invocation.payload, ToolPayload::Custom { .. })
    {
        return FunctionCallError::RespondToModel(message);
    }

    let diff = invocation.tracker.lock().await.get_unified_diff();
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        active.as_ref().map(|active| active.turn_state.clone())
    };
    let Some(turn_state) = turn_state else {
        return FunctionCallError::RespondToModel(message);
    };
    let mut turn = turn_state.lock().await;
    if turn
        .rencrow_execution_guard
        .record_patch_verification_failure(diff.as_deref())
    {
        return FunctionCallError::Fatal(format!(
            "RENCROW_PATCH_RETRY_EXCEEDED: three apply_patch syntax or verification failures without a real diff change; {message}"
        ));
    }
    FunctionCallError::RespondToModel(format!("{message}\n{}", patch_failure_hint(failure_kind)))
}

fn patch_failure_hint(failure_kind: &str) -> &'static str {
    match failure_kind {
        PATCH_SYNTAX_FAILURE_KIND => {
            "RENCROW_PATCH_FORMAT_HINT: Use exactly one Begin Patch / End Patch pair per call; all file sections go inside it. Add File: prefix every content line with + and do not use @@. Update File: use a bare @@ hunk header (or valid @@ context), never unified-diff line ranges such as @@ -1 +1 @@. Keep file headers exact. Example with two files:\n*** Begin Patch\n*** Add File: new.txt\n+new file content\n*** Update File: existing.txt\n@@\n-old\n+new\n*** End Patch"
        }
        PATCH_CONTEXT_FAILURE_KIND => {
            "RENCROW_PATCH_CONTEXT_HINT: Replace only exact existing source lines, copied in full from the current file. Preserve every line prefix, including Markdown line prefixes such as `- `. If context no longer matches, inspect the file and use its current line text. Do not retry the same mismatched context."
        }
        _ => "",
    }
}

pub(crate) struct ToolReceipt {
    pub(crate) value: Value,
    pub(crate) successful: bool,
}

pub(crate) async fn after_tool(invocation: &ToolInvocation, receipt: ToolReceipt) {
    if invocation.turn.config.rencrow_autonomous_completion
        && receipt.successful
        && invocation.tool_name.namespace.as_deref() == Some("multi_agent_v1")
    {
        invocation
            .session
            .observe_completion_child_tool(
                &invocation.tool_name.name,
                &arguments(invocation),
                &receipt.value,
            )
            .await;
    }
    if invocation.turn.config.rencrow_execution_guard.is_none() {
        return;
    }
    let diff = invocation.tracker.lock().await.get_unified_diff();
    let value = receipt.value;
    let command = shell_script_for_invocation(invocation);
    let args = arguments(invocation);
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        active.as_ref().map(|active| active.turn_state.clone())
    };
    let Some(turn_state) = turn_state else {
        return;
    };
    let child_evidence = if receipt.successful
        && invocation.tool_name.namespace.as_deref() == Some("multi_agent_v1")
    {
        let ids = completed_child_ids(&invocation.tool_name.name, &args, &value);
        let parent_thread_id = invocation.session.thread_id();
        let mut evidence = Vec::new();
        for id in ids {
            let Ok(thread_id) = codex_protocol::ThreadId::try_from(id.as_str()) else {
                continue;
            };
            if let Some(receipt) = invocation
                .session
                .services
                .agent_control
                .get_rencrow_execution_evidence(thread_id, parent_thread_id)
                .await
            {
                evidence.push(receipt);
            }
        }
        let mut fresh_evidence = Vec::new();
        for evidence in evidence {
            if invocation
                .session
                .consume_rencrow_child_execution_evidence(&evidence)
                .await
            {
                fresh_evidence.push(evidence);
            }
        }
        fresh_evidence
    } else {
        Vec::new()
    };
    let implementation_child_delegated = if receipt.successful
        && invocation.tool_name.namespace.as_deref() == Some("multi_agent_v1")
    {
        let ids = match invocation.tool_name.name.as_str() {
            "spawn_agent" => value["agent_id"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect(),
            "resume_agent" => args["id"].as_str().map(str::to_owned).into_iter().collect(),
            "send_input" => args["target"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        let mut delegated = false;
        for id in ids {
            let Ok(thread_id) = codex_protocol::ThreadId::try_from(id.as_str()) else {
                delegated = true;
                continue;
            };
            let child_requires_edit = invocation
                .session
                .services
                .agent_control
                .get_rencrow_child_requires_edit(thread_id)
                .await;
            if implementation_child_requires_edit(child_requires_edit) {
                delegated = true;
            }
        }
        delegated
    } else {
        false
    };
    let terminal_child_ids = if receipt.successful
        && invocation.tool_name.namespace.as_deref() == Some("multi_agent_v1")
    {
        let mut ids = reported_terminal_child_ids(&invocation.tool_name.name, &args, &value);
        if invocation.tool_name.name == "send_input"
            && let Some(id) = args["target"].as_str()
            && let Ok(thread_id) = codex_protocol::ThreadId::try_from(id)
            && matches!(
                invocation
                    .session
                    .services
                    .agent_control
                    .get_status(thread_id)
                    .await,
                AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
            )
        {
            ids.push(id.to_owned());
        }
        ids
    } else {
        Vec::new()
    };
    let mut turn = turn_state.lock().await;
    let state = &mut turn.rencrow_execution_guard;
    state.observe_diff(diff.as_deref());
    if command
        .as_ref()
        .is_some_and(|command| check_command(command))
        && let Some((command, stamp)) = state.started_checks.remove(&invocation.call_id)
    {
        if let Some(exit) = value["exit_code"].as_i64() {
            if receipt.successful {
                state.finish_check(&command, stamp, exit);
            }
        } else if let Some(id) = value["session_id"].as_i64() {
            state.pending_checks.insert(id, (command, stamp));
        }
    } else if invocation.tool_name.name == "write_stdin"
        && let Some(id) = args["session_id"].as_i64()
        && let Some(exit) = value["exit_code"].as_i64()
        && let Some((key, stamp)) = state.pending_checks.remove(&id)
        && receipt.successful
    {
        state.finish_check(&key, stamp, exit);
    }
    if receipt.successful
        && value["exit_code"].as_i64() == Some(0)
        && let Some(command) = command.as_deref().filter(|command| !check_command(command))
        && unrecognized_python_test_script(command)
    {
        state.record_uncredited_python_test(command);
    }
    if receipt.successful && invocation.tool_name.namespace.as_deref() == Some("multi_agent_v1") {
        state.observe_child_result(&invocation.tool_name.name, &args, &value);
    }
    state.implementation_child_delegated |= implementation_child_delegated;
    for id in terminal_child_ids {
        state.children.remove(&id);
    }
    for evidence in child_evidence {
        if state
            .child_results
            .insert((evidence.thread_id, evidence.turn_id))
        {
            // A successful host proof is itself evidence that the child did
            // implementation work, even when its static role is Orchestrate.
            state.implementation_child_delegated = true;
            state.record_child_progress();
        }
    }
}

fn completed_child_ids(tool: &str, args: &Value, value: &Value) -> Vec<String> {
    let completed = |value: &Value| {
        serde_json::from_value::<AgentStatus>(value.clone())
            .is_ok_and(|status| matches!(status, AgentStatus::Completed(_)))
    };
    match tool {
        "wait_agent" => value["status"]
            .as_object()
            .into_iter()
            .flat_map(|statuses| statuses.iter())
            .filter(|(_, status)| completed(status))
            .map(|(id, _)| id.clone())
            .collect(),
        "resume_agent" if completed(&value["status"]) => {
            args["id"].as_str().map(str::to_owned).into_iter().collect()
        }
        "send_input" => args["target"]
            .as_str()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn reported_terminal_child_ids(tool: &str, args: &Value, value: &Value) -> Vec<String> {
    let terminal = |value: &Value| {
        serde_json::from_value::<AgentStatus>(value.clone()).is_ok_and(|status| {
            matches!(
                status,
                AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
            )
        })
    };
    match tool {
        "wait_agent" => value["status"]
            .as_object()
            .into_iter()
            .flat_map(|statuses| statuses.iter())
            .filter(|(_, status)| terminal(status))
            .map(|(id, _)| id.clone())
            .collect(),
        "resume_agent" if terminal(&value["status"]) => {
            args["id"].as_str().map(str::to_owned).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

pub(crate) async fn child_recovery_prompt(
    session: &Session,
    turn: &TurnContext,
    attempt: &mut u8,
) -> Option<String> {
    let policy = turn.config.rencrow_execution_guard.as_ref()?;
    if !policy.requires_child_results() || matches!(turn.session_source, SessionSource::Internal(_))
    {
        return None;
    }
    let state = {
        let active = session.active_turn.lock().await;
        active.as_ref()?.turn_state.clone()
    };
    let state = state.lock().await;
    state.rencrow_execution_guard.child_recovery_prompt(attempt)
}

pub(crate) async fn check_completion(
    session: &Session,
    turn: &TurnContext,
    tracker: &SharedTurnDiffTracker,
) -> Result<(), CodexErr> {
    if matches!(turn.session_source, SessionSource::Internal(_)) {
        return Ok(());
    }
    if turn.config.rencrow_autonomous_completion
        && let Some(root_task_id) = session.task_check_completion_root_id().await
        && session
            .unresolved_task_check_obligations_for_root(&root_task_id)
            .await
            > 0
    {
        return Err(CodexErr::InvalidRequest(
            "RENCROW_EXECUTION_CONTRACT_INCOMPLETE: unresolved host task_evidence verification obligations".into(),
        ));
    }
    let Some(policy) = turn.config.rencrow_execution_guard.as_ref() else {
        return Ok(());
    };
    if policy.requires_child_results()
        && turn.multi_agent_version == codex_protocol::protocol::MultiAgentVersion::V2
    {
        return Err(CodexErr::InvalidRequest(
            "RENCROW_EXECUTION_GUARD: require_child_results currently requires multi_agent_v1"
                .into(),
        ));
    }
    let diff = tracker.lock().await.get_unified_diff();
    let turn_state = {
        let active = session.active_turn.lock().await;
        active.as_ref().map(|active| active.turn_state.clone())
    };
    let Some(turn_state) = turn_state else {
        return Err(CodexErr::InvalidRequest(
            "RENCROW_EXECUTION_GUARD: no active turn at completion".into(),
        ));
    };
    let outcome = {
        let mut state = turn_state.lock().await;
        state.rencrow_execution_guard.observe_diff(diff.as_deref());
        let mut missing = state
            .rencrow_execution_guard
            .missing(policy, diff.as_ref().is_some_and(|diff| !diff.is_empty()));
        if missing.is_empty() {
            let evidence = turn
                .parent_thread_id
                .filter(|_| {
                    child_execution_evidence_eligible(
                        policy,
                        &state.rencrow_execution_guard,
                        diff.as_ref().is_some_and(|diff| !diff.is_empty()),
                    )
                })
                .map(|parent_thread_id| GuardExecutionEvidence {
                    thread_id: session.thread_id(),
                    turn_id: turn.sub_id.clone(),
                    parent_thread_id,
                    diff_stamp: state.rencrow_execution_guard.diff_stamp,
                    has_diff: true,
                    checked: true,
                });
            Ok(evidence)
        } else {
            let check_diagnostic = state
            .rencrow_execution_guard
            .uncredited_python_test()
            .filter(|_| {
                missing.iter().any(|item| {
                    *item == "successful check after the last edit (failed checks must be rerun)"
                })
            })
            .map(|command| {
                format!(
                    "; host observed exit code 0 for `{command}`, but direct Python test scripts are not recorded as checks. Run a recognized test runner as a separate direct command, such as `python3 -m unittest -v tests` or `pytest -q <test_path>`"
                )
            })
            .unwrap_or_default();
            Err(CodexErr::InvalidRequest(format!(
                "RENCROW_EXECUTION_CONTRACT_INCOMPLETE: {}{}{}",
                missing.join("; "),
                check_diagnostic,
                state.rencrow_execution_guard.failed_check_diagnostic()
            )))
        }
    };
    match outcome {
        Ok(evidence) => {
            if let Some(evidence) = evidence {
                session.store_rencrow_execution_evidence(evidence).await;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn completion_recovery_prompt(
    session: &Session,
    turn: &TurnContext,
    tracker: &SharedTurnDiffTracker,
    attempt: &mut u8,
) -> Option<String> {
    let policy = turn.config.rencrow_execution_guard.as_ref()?;
    if (!policy.requires_edit()
        && !policy.requires_check()
        && policy.task_kind != Some(RencrowTaskKind::Orchestrate))
        || !matches!(
            turn.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
    {
        return None;
    }
    let diff = tracker.lock().await.get_unified_diff();
    let turn_state = {
        let active = session.active_turn.lock().await;
        active.as_ref()?.turn_state.clone()
    };
    let mut state = turn_state.lock().await;
    state.rencrow_execution_guard.observe_diff(diff.as_deref());
    state.rencrow_execution_guard.completion_recovery_prompt(
        policy,
        diff.is_some_and(|diff| !diff.is_empty()),
        attempt,
    )
}

#[cfg(test)]
#[path = "rencrow_execution_guard_tests.rs"]
mod tests;

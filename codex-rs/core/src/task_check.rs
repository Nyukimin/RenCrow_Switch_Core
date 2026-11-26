// Modified by RenCrow Switch Core, 2026-11-16: compile test helpers only for tests.
// Modified by RenCrow Switch Core, 2026-11-26: bind expected failures to host-owned obligations.
//! Host-owned receipts for task-scoped verification commands.
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

#[path = "task_check_expected_negative.rs"]
mod expected_negative;
pub(crate) use expected_negative::ExpectedNegativeBindingV1;
pub(crate) use expected_negative::ExpectedNegativeResolutionV1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskEvidenceRequest {
    pub task_id: String,
    pub task_revision: u64,
    pub artifact_paths: Vec<PathBuf>,
    pub check_kind: TaskCheckKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskCheckKind {
    Verification,
}

#[derive(Clone, Debug)]
pub(crate) struct TaskCheckExecution {
    pub request: TaskEvidenceRequest,
    pub workspace_root: PathBuf,
    pub before: Vec<TaskArtifactStamp>,
    pub obligation: Option<TaskCheckObligation>,
    pub execution_environment_id: Option<String>,
}

/// One observed native completion, before deriving a task-check receipt.
pub(crate) struct TaskCheckCompletion<'a> {
    pub(crate) command: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) output: &'a str,
    pub(crate) exit_code: i32,
    pub(crate) completed: bool,
    pub(crate) output_truncated: bool,
    pub(crate) after: Vec<TaskArtifactStamp>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckObligation {
    pub(crate) scope: crate::state::ActiveTaskScope,
    pub(crate) workspace_root: PathBuf,
    pub(crate) command_identity: String,
    pub(crate) artifact_paths: Vec<PathBuf>,
}

impl TaskCheckExecution {
    pub(crate) fn new(request: TaskEvidenceRequest, workspace_root: &Path) -> Option<Self> {
        if request.task_id.trim().is_empty() || request.artifact_paths.is_empty() {
            return None;
        }
        let mut execution = Self {
            request,
            workspace_root: std::fs::canonicalize(workspace_root).ok()?,
            before: Vec::new(),
            obligation: None,
            execution_environment_id: None,
        };
        execution.before = execution.hash_artifacts()?;
        Some(execution)
    }

    pub(crate) fn bind_obligation(&mut self, scope: crate::state::ActiveTaskScope, command: &str) {
        self.obligation = TaskCheckObligation::new(
            scope,
            self.workspace_root.clone(),
            command,
            self.request.artifact_paths.clone(),
        );
    }

    pub(crate) fn hash_artifacts(&self) -> Option<Vec<TaskArtifactStamp>> {
        hash_workspace_artifacts(&self.workspace_root, &self.request.artifact_paths)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskArtifactStamp {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckReceipt {
    pub call_id: String,
    pub task_id: String,
    pub task_revision: u64,
    pub workspace_root: PathBuf,
    pub artifact_before: Vec<TaskArtifactStamp>,
    pub artifact_after: Vec<TaskArtifactStamp>,
    pub exit_code: i32,
    #[serde(default = "default_completed_receipt")]
    pub completed: bool,
    pub succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Box<TaskCheckObservation>>,
}

/// Bounded command facts observed by the host when a verification call finishes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckObservation {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_environment_id: Option<String>,
    pub diagnostic_excerpt: String,
    pub diagnostic_sha256: String,
    pub diagnostic_complete: bool,
    pub diagnostic_truncated: bool,
    pub output_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_failure: Option<TaskCheckLaunchFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskCheckLaunchFailure {
    GoModuleDiscovery,
    GitOutsideRepository,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckLaunchCorrection {
    pub kind: TaskCheckLaunchFailure,
    pub target_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_event_ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_handoff_projection_sha256: Option<String>,
    pub proof_ref: String,
    pub target_command: Vec<String>,
    pub target_cwd: PathBuf,
    pub target_output_sha256: String,
    pub target_output_ref: String,
}

const MAX_RETAINED_TASK_CHECK_DIAGNOSTIC: usize = 16 * 1024;
const MAX_TASK_CHECK_TERMINAL_EVENTS: usize = 256;
const GO_MODULE_DISCOVERY_DIAGNOSTIC: &str = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";
const GIT_OUTSIDE_REPOSITORY_DIAGNOSTIC_SHA256: &str =
    "fae2951cf6eebb96d76be13591b758c4f90e39580d2a88a4464cb8ab03e15164";

impl TaskCheckObservation {
    pub(crate) fn from_execution(
        call_id: &str,
        command: &[String],
        cwd: &Path,
        output: &str,
        exit_code: i32,
        completed: bool,
        producer_truncated: bool,
    ) -> Self {
        let diagnostic_sha256 = format!("{:x}", sha2::Sha256::digest(output.as_bytes()));
        let diagnostic_truncated = producer_truncated
            || output.len() > MAX_RETAINED_TASK_CHECK_DIAGNOSTIC
            || output.starts_with("Warning: truncated output")
            || output.contains("[output omitted ");
        let excerpt_end = (0..=output.len().min(MAX_RETAINED_TASK_CHECK_DIAGNOSTIC))
            .rev()
            .find(|index| output.is_char_boundary(*index))
            .unwrap_or(0);
        let diagnostic_excerpt = output[..excerpt_end].to_owned();
        let launch_failure = (!diagnostic_truncated && completed && exit_code != 0)
            .then(|| {
                if command.last().is_some_and(|shell_command| {
                    shlex::split(shell_command).is_some_and(|words| {
                        words.first().is_some_and(|word| word == "go")
                            && words.get(1).is_some_and(|word| word == "vet")
                            && words.len() == 3
                            && words[2].starts_with("./")
                            && !normalized_check_identity(shell_command).starts_with("raw:")
                    })
                }) && output == GO_MODULE_DISCOVERY_DIAGNOSTIC
                {
                    Some(TaskCheckLaunchFailure::GoModuleDiscovery)
                } else if command.iter().map(String::as_str).eq([
                    "/bin/zsh",
                    "-lc",
                    "git diff --check",
                ]) && exit_code == 129
                    && diagnostic_sha256 == GIT_OUTSIDE_REPOSITORY_DIAGNOSTIC_SHA256
                {
                    Some(TaskCheckLaunchFailure::GitOutsideRepository)
                } else {
                    None
                }
            })
            .flatten();
        Self {
            command: command.to_vec(),
            cwd: std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            execution_environment_id: None,
            diagnostic_excerpt,
            diagnostic_sha256,
            diagnostic_complete: !diagnostic_truncated,
            diagnostic_truncated,
            output_ref: call_id.to_owned(),
            launch_failure,
        }
    }
}

fn canonical_artifact_hashes(receipt: &TaskCheckReceipt) -> Option<Vec<(PathBuf, String)>> {
    if receipt.artifact_before.is_empty() || receipt.artifact_before != receipt.artifact_after {
        return None;
    }
    let root = std::fs::canonicalize(&receipt.workspace_root).ok()?;
    let mut stamps = receipt
        .artifact_before
        .iter()
        .map(|stamp| {
            if stamp.path.is_absolute()
                || stamp
                    .path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return None;
            }
            Some((
                std::fs::canonicalize(root.join(&stamp.path)).ok()?,
                stamp.sha256.clone(),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    stamps.sort();
    Some(stamps)
}

fn launch_correction_has_target_correspondence(
    kind: &TaskCheckLaunchFailure,
    observation: &TaskCheckObservation,
    target_receipt: &TaskCheckReceipt,
) -> bool {
    if observation.cwd == target_receipt.workspace_root {
        return false;
    }
    let Ok(target_root) = std::fs::canonicalize(&target_receipt.workspace_root) else {
        return false;
    };
    let Some(target_stamps) = canonical_artifact_hashes(target_receipt) else {
        return false;
    };
    match kind {
        TaskCheckLaunchFailure::GoModuleDiscovery => {
            let Some(shell_command) = observation.command.last() else {
                return false;
            };
            let Some(words) = shlex::split(shell_command) else {
                return false;
            };
            if words.len() != 3 || !words[2].starts_with("./") {
                return false;
            }
            let package = &words[2];
            let Ok(package_root) = std::fs::canonicalize(target_root.join(package)) else {
                return false;
            };
            target_stamps
                .iter()
                .all(|(path, _)| path.starts_with(&package_root))
        }
        TaskCheckLaunchFailure::GitOutsideRepository => {
            target_root.join(".git").exists()
                && target_stamps
                    .iter()
                    .all(|(path, _)| path.starts_with(&target_root))
        }
    }
}

fn default_completed_receipt() -> bool {
    true
}

#[derive(Default)]
pub(crate) struct TaskCheckLedger {
    attempts: HashMap<String, TaskCheckAttempt>,
    attempt_scope: HashMap<String, (String, u64)>,
    attempt_obligation: HashMap<String, Option<TaskCheckObligation>>,
    latest_attempt: HashMap<(String, u64), String>,
    pending_data: HashMap<String, (TaskEvidenceRequest, PathBuf, Vec<TaskArtifactStamp>)>,
    expected_negative: HashMap<String, expected_negative::ExpectedNegativeRecordV1>,
    terminal_event_order: HashMap<String, u64>,
    imported_launch_projection: HashMap<String, String>,
    next_terminal_event_order: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckAttempt {
    pub(crate) call_id: String,
    #[serde(default)]
    pub(crate) resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) launch_correction: Option<TaskCheckLaunchCorrection>,
    pub(crate) status: TaskCheckAttemptStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum TaskCheckAttemptStatus {
    Pending,
    ChangedArtifacts,
    Finished(TaskCheckReceipt),
}

/// Original host verification facts, retained across process restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckSnapshot {
    pub(crate) task_id: String,
    pub(crate) task_revision: u64,
    pub(crate) attempt: TaskCheckAttempt,
    #[serde(default = "default_snapshot_latest")]
    pub(crate) latest: bool,
    #[serde(default)]
    pub(crate) obligation: Option<TaskCheckObligation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_negative: Option<expected_negative::ExpectedNegativeRecordV1>,
}

fn default_snapshot_latest() -> bool {
    true
}

impl TaskCheckLedger {
    pub(crate) fn snapshot(&self) -> Vec<TaskCheckSnapshot> {
        self.snapshot_with_references(&std::collections::HashSet::new())
    }

    pub(crate) fn snapshot_with_references(
        &self,
        referenced_calls: &std::collections::HashSet<String>,
    ) -> Vec<TaskCheckSnapshot> {
        let mut retained_calls = self
            .attempts
            .values()
            .filter_map(|attempt| {
                let (task_id, task_revision) = self.attempt_scope.get(&attempt.call_id)?;
                let latest_task = self
                    .latest_attempt
                    .get(&(task_id.clone(), *task_revision))
                    .is_some_and(|call| call == &attempt.call_id);
                let unresolved = self.attempt_is_unresolved(attempt);
                (latest_task
                    || unresolved
                    || attempt.launch_correction.is_some()
                    || self.expected_negative.contains_key(&attempt.call_id)
                    || referenced_calls.contains(&attempt.call_id))
                .then_some(attempt.call_id.clone())
            })
            .collect::<std::collections::HashSet<_>>();
        let mut pending_targets = retained_calls.iter().cloned().collect::<Vec<_>>();
        while let Some(call) = pending_targets.pop() {
            if let Some(target) = self
                .attempts
                .get(&call)
                .and_then(|attempt| attempt.resolved_by.as_ref())
                && retained_calls.insert(target.clone())
            {
                pending_targets.push(target.clone());
            }
        }
        let mut entries = self
            .attempts
            .values()
            .filter_map(|attempt| {
                if !retained_calls.contains(&attempt.call_id) {
                    return None;
                }
                let (task_id, task_revision) = self.attempt_scope.get(&attempt.call_id)?.clone();
                let latest = self
                    .latest_attempt
                    .get(&(task_id.clone(), task_revision))
                    .is_some_and(|call| call == &attempt.call_id);
                let obligation = self
                    .attempt_obligation
                    .get(&attempt.call_id)
                    .cloned()
                    .flatten();
                Some(TaskCheckSnapshot {
                    latest,
                    task_id,
                    task_revision,
                    attempt: attempt.clone(),
                    obligation,
                    expected_negative: self.expected_negative.get(&attempt.call_id).cloned(),
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            (&a.task_id, a.task_revision, a.latest, &a.attempt.call_id).cmp(&(
                &b.task_id,
                b.task_revision,
                b.latest,
                &b.attempt.call_id,
            ))
        });
        entries
    }

    pub(crate) fn restore_snapshot(
        &mut self,
        entries: Vec<TaskCheckSnapshot>,
    ) -> Result<(), String> {
        let mut restored = Self::default();
        for entry in entries {
            let call = &entry.attempt.call_id;
            if entry.task_id.is_empty()
                || entry.task_revision == 0
                || call.is_empty()
                || restored.attempts.contains_key(call)
                || entry.obligation.as_ref().is_some_and(|obligation| {
                    obligation.scope.root_task_id.is_empty()
                        || obligation.scope.task_id != entry.task_id
                        || obligation.scope.task_revision != entry.task_revision
                        || obligation.scope.begin_call_id.is_empty()
                        || !obligation.workspace_root.is_absolute()
                        || obligation.command_identity.is_empty()
                        || obligation.artifact_paths.is_empty()
                        || obligation.artifact_paths.iter().any(|path| {
                            path.is_absolute()
                                || path.components().any(|component| {
                                    matches!(
                                        component,
                                        std::path::Component::ParentDir
                                            | std::path::Component::RootDir
                                            | std::path::Component::Prefix(_)
                                    )
                                })
                        })
                        || obligation
                            .artifact_paths
                            .windows(2)
                            .any(|pair| pair[0] >= pair[1])
                })
            {
                return Err("invalid or duplicate host verification snapshot".into());
            }
            if let TaskCheckAttemptStatus::Finished(receipt) = &entry.attempt.status
                && (receipt.call_id != *call
                    || receipt.task_id != entry.task_id
                    || receipt.task_revision != entry.task_revision
                    || !receipt.workspace_root.is_absolute()
                    || receipt.artifact_before.is_empty()
                    || receipt.artifact_before != receipt.artifact_after
                    || (receipt.succeeded && (receipt.exit_code != 0 || !receipt.completed)))
            {
                return Err("host verification snapshot identity or outcome mismatch".into());
            }
            if entry.latest
                && restored
                    .latest_attempt
                    .insert((entry.task_id.clone(), entry.task_revision), call.clone())
                    .is_some()
            {
                return Err("multiple latest host verification attempts in snapshot".into());
            }
            restored
                .attempt_scope
                .insert(call.clone(), (entry.task_id.clone(), entry.task_revision));
            restored
                .attempt_obligation
                .insert(call.clone(), entry.obligation);
            if let Some(expected_negative) = entry.expected_negative {
                restored
                    .expected_negative
                    .insert(call.clone(), expected_negative);
            }
            restored.attempts.insert(call.clone(), entry.attempt);
        }
        restored.validate_expected_negative_snapshot()?;
        for ((task_id, task_revision), call) in &restored.latest_attempt {
            if !restored.attempts.get(call).is_some_and(|attempt| {
                match &attempt.status {
                    TaskCheckAttemptStatus::Finished(receipt) => {
                        receipt.task_id == *task_id && receipt.task_revision == *task_revision
                    }
                    TaskCheckAttemptStatus::Pending | TaskCheckAttemptStatus::ChangedArtifacts => {
                        // These legacy states carry task identity only in the snapshot row.
                        true
                    }
                }
            }) {
                return Err("latest host verification attempt is missing".into());
            }
        }
        for attempt in restored.attempts.values() {
            let Some(resolved_by) = attempt.resolved_by.as_ref() else {
                continue;
            };
            let Some(source_obligation) = restored
                .attempt_obligation
                .get(&attempt.call_id)
                .and_then(Option::as_ref)
            else {
                return Err("legacy verification attempt cannot claim resolution".into());
            };
            let Some(target) = restored.attempts.get(resolved_by) else {
                return Err("verification resolution target is missing".into());
            };
            let target_is_success = matches!(
                &target.status,
                TaskCheckAttemptStatus::Finished(receipt) if receipt.succeeded
            );
            let same_obligation = restored
                .attempt_obligation
                .get(resolved_by)
                .and_then(Option::as_ref)
                .is_some_and(|obligation| obligation.key() == source_obligation.key());
            let correction_matches = attempt
                .launch_correction
                .as_ref()
                .is_some_and(|correction| {
                    let source_receipt = match &attempt.status {
                        TaskCheckAttemptStatus::Finished(receipt) => receipt,
                        TaskCheckAttemptStatus::Pending
                        | TaskCheckAttemptStatus::ChangedArtifacts => {
                            return false;
                        }
                    };
                    let source_observation = source_receipt.observation.as_ref();
                    let source_scope = restored
                        .attempt_obligation
                        .get(&attempt.call_id)
                        .and_then(Option::as_ref)
                        .map(|obligation| &obligation.scope);
                    let target_scope = restored
                        .attempt_obligation
                        .get(resolved_by)
                        .and_then(Option::as_ref)
                        .map(|obligation| &obligation.scope);
                    let target_receipt = restored.attempts.get(resolved_by).and_then(|target| {
                        match &target.status {
                            TaskCheckAttemptStatus::Finished(receipt) => Some(receipt),
                            TaskCheckAttemptStatus::Pending
                            | TaskCheckAttemptStatus::ChangedArtifacts => None,
                        }
                    });
                    correction.target_call_id == *resolved_by
                        && match (
                            correction.source_event_ordinal,
                            correction.target_event_ordinal,
                            correction.source_handoff_projection_sha256.as_deref(),
                        ) {
                            (Some(source), Some(target), None) => source < target,
                            (None, None, None) => true,
                            (None, None, Some(hash)) => {
                                hash.len() == 64
                                    && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            }
                            _ => false,
                        }
                        && correction.proof_ref == attempt.call_id
                        && !correction.target_command.is_empty()
                        && correction.target_cwd.is_absolute()
                        && correction.target_output_sha256.len() == 64
                        && correction
                            .target_output_sha256
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        && correction.target_output_ref == correction.target_call_id
                        && restored
                            .attempts
                            .get(resolved_by)
                            .and_then(|target| match &target.status {
                                TaskCheckAttemptStatus::Finished(receipt) => Some(receipt),
                                TaskCheckAttemptStatus::Pending
                                | TaskCheckAttemptStatus::ChangedArtifacts => None,
                            })
                            .is_some_and(|receipt| receipt.workspace_root == correction.target_cwd)
                        && source_observation.is_some_and(|observation| {
                            observation.diagnostic_complete
                                && observation.launch_failure.as_ref() == Some(&correction.kind)
                                && observation.command == correction.target_command
                                && observation.command.last().is_some_and(|command| {
                                    normalized_check_identity(command)
                                        == source_obligation.command_identity
                                })
                                && target_receipt.is_some_and(|target_receipt| {
                                    canonical_artifact_hashes(source_receipt)
                                        == canonical_artifact_hashes(target_receipt)
                                        && launch_correction_has_target_correspondence(
                                            &correction.kind,
                                            observation,
                                            target_receipt,
                                        )
                                })
                        })
                        && source_scope
                            .zip(target_scope)
                            .is_some_and(|(source, target)| source.same_execution(target))
                });
            if !matches!(
                &attempt.status,
                TaskCheckAttemptStatus::Finished(receipt)
                    if receipt.completed
                        && receipt.exit_code != 0
                        && receipt.exit_code != -1
                        && !receipt.succeeded
            ) || !target_is_success
                || (!same_obligation && !correction_matches)
            {
                return Err("verification resolution link is inconsistent".into());
            }
        }
        // Pending work stays pending/unknown; restart never manufactures a process or result.
        *self = restored;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn get(&self, call_id: &str) -> Option<&TaskCheckReceipt> {
        match &self.attempts.get(call_id)?.status {
            TaskCheckAttemptStatus::Finished(receipt) => Some(receipt),
            TaskCheckAttemptStatus::Pending | TaskCheckAttemptStatus::ChangedArtifacts => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn begin(
        &mut self,
        call_id: String,
        request: TaskEvidenceRequest,
        workspace_root: PathBuf,
        before: Vec<TaskArtifactStamp>,
    ) {
        self.begin_with_obligation(call_id, request, workspace_root, before, None);
    }

    pub(crate) fn begin_with_obligation(
        &mut self,
        call_id: String,
        request: TaskEvidenceRequest,
        workspace_root: PathBuf,
        before: Vec<TaskArtifactStamp>,
        obligation: Option<TaskCheckObligation>,
    ) {
        // A reserved expected-negative call has an immutable pre-launch identity and raw result.
        if self.expected_negative.contains_key(&call_id) {
            return;
        }
        if request.artifact_paths.is_empty()
            || before.is_empty()
            || request.artifact_paths.len() != before.len()
            || request
                .artifact_paths
                .iter()
                .any(|p| !before.iter().any(|s| &s.path == p))
        {
            return;
        }
        let key = (request.task_id.clone(), request.task_revision);
        self.terminal_event_order.remove(&call_id);
        self.latest_attempt.insert(key, call_id.clone());
        self.attempts.insert(
            call_id.clone(),
            TaskCheckAttempt {
                call_id: call_id.clone(),
                resolved_by: None,
                launch_correction: None,
                status: TaskCheckAttemptStatus::Pending,
            },
        );
        self.attempt_scope.insert(
            call_id.clone(),
            (request.task_id.clone(), request.task_revision),
        );
        self.attempt_obligation.insert(call_id.clone(), obligation);
        self.pending_data
            .insert(call_id, (request, workspace_root, before));
    }

    pub(crate) fn observe_terminal_event(&mut self, call_id: &str) {
        if !self.attempts.contains_key(call_id)
            || self.terminal_event_order.contains_key(call_id)
            || self.terminal_event_order.len() >= MAX_TASK_CHECK_TERMINAL_EVENTS
        {
            return;
        }
        self.next_terminal_event_order = self.next_terminal_event_order.saturating_add(1);
        self.terminal_event_order
            .insert(call_id.to_owned(), self.next_terminal_event_order);
        self.imported_launch_projection.remove(call_id);
    }

    pub(crate) fn seed_terminal_event_orders(&mut self, orders: Vec<(String, u64)>) {
        for (call_id, order) in orders {
            if self.terminal_event_order.len() >= MAX_TASK_CHECK_TERMINAL_EVENTS {
                break;
            }
            if self.attempts.contains_key(&call_id)
                && !self.terminal_event_order.contains_key(&call_id)
            {
                self.next_terminal_event_order = self.next_terminal_event_order.max(order);
                self.terminal_event_order.insert(call_id, order);
            }
        }
    }

    pub(crate) fn seed_imported_preflight_launch_failures(
        &mut self,
        sources: Vec<(String, String)>,
    ) {
        for (call_id, projection_sha256) in sources {
            if projection_sha256.len() == 64
                && projection_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && !self.terminal_event_order.contains_key(&call_id)
                && self.has_unresolved_launch_failure(&call_id)
            {
                self.imported_launch_projection
                    .insert(call_id, projection_sha256);
            }
        }
    }

    fn has_unresolved_launch_failure(&self, call_id: &str) -> bool {
        let Some(attempt) = self.attempts.get(call_id) else {
            return false;
        };
        if attempt.resolved_by.is_some() || attempt.launch_correction.is_some() {
            return false;
        }
        let is_launch_failure = matches!(
            &attempt.status,
            TaskCheckAttemptStatus::Finished(receipt)
                if !receipt.succeeded
                    && receipt.completed
                    && receipt.exit_code != 0
                    && receipt.exit_code != -1
                    && receipt.observation.as_ref().is_some_and(|observation| {
                        observation.diagnostic_complete
                            && !observation.diagnostic_truncated
                            && observation.launch_failure.is_some()
                    })
        );
        is_launch_failure
            && self
                .attempt_obligation
                .get(call_id)
                .and_then(Option::as_ref)
                .is_some()
    }

    #[cfg(test)]
    pub(crate) fn finish(
        &mut self,
        call_id: &str,
        exit_code: i32,
        completed: bool,
        after: Vec<TaskArtifactStamp>,
    ) -> Option<TaskCheckReceipt> {
        self.finish_observed(call_id, exit_code, completed, after, None)
    }

    pub(crate) fn finish_observed(
        &mut self,
        call_id: &str,
        exit_code: i32,
        completed: bool,
        after: Vec<TaskArtifactStamp>,
        observation: Option<TaskCheckObservation>,
    ) -> Option<TaskCheckReceipt> {
        let (request, workspace_root, before) = self.pending_data.remove(call_id)?;
        if before
            .iter()
            .map(|s| (&s.path, &s.sha256))
            .ne(after.iter().map(|s| (&s.path, &s.sha256)))
        {
            if let Some(attempt) = self.attempts.get_mut(call_id) {
                attempt.status = TaskCheckAttemptStatus::ChangedArtifacts;
            }
            return None;
        }
        let succeeded = completed && exit_code == 0;
        let receipt = TaskCheckReceipt {
            call_id: call_id.to_owned(),
            task_id: request.task_id,
            task_revision: request.task_revision,
            workspace_root,
            artifact_before: before,
            artifact_after: after,
            exit_code,
            completed,
            succeeded,
            observation: if succeeded {
                None
            } else {
                observation.clone().map(Box::new)
            },
        };
        if let Some(attempt) = self.attempts.get_mut(call_id) {
            attempt.status = TaskCheckAttemptStatus::Finished(receipt.clone());
        }
        if receipt.succeeded
            && let Some(obligation_key) = self
                .attempt_obligation
                .get(call_id)
                .and_then(Option::as_ref)
                .map(TaskCheckObligation::key)
        {
            for (previous_call, attempt) in &mut self.attempts {
                if previous_call != call_id
                    && attempt.resolved_by.is_none()
                    && self
                        .attempt_obligation
                        .get(previous_call)
                        .and_then(Option::as_ref)
                        .is_some_and(|obligation| obligation.key() == obligation_key)
                    && matches!(
                        &attempt.status,
                        TaskCheckAttemptStatus::Finished(previous)
                            if previous.completed
                                && previous.exit_code != 0
                                && previous.exit_code != -1
                                && !previous.succeeded
                    )
                {
                    attempt.resolved_by = Some(call_id.to_owned());
                }
            }
        }
        if receipt.succeeded
            && let Some(target_observation) = observation.as_ref()
        {
            self.reconcile_live_launch_failures(call_id, target_observation);
        }
        Some(receipt)
    }

    fn reconcile_live_launch_failures(
        &mut self,
        target_call_id: &str,
        target_observation: &TaskCheckObservation,
    ) {
        if !target_observation.diagnostic_complete || target_observation.diagnostic_truncated {
            return;
        }
        let Some(target_order) = self.terminal_event_order.get(target_call_id).copied() else {
            return;
        };
        let Some(target_attempt) = self.attempts.get(target_call_id).cloned() else {
            return;
        };
        let Some((task_id, task_revision)) = self.attempt_scope.get(target_call_id).cloned() else {
            return;
        };
        let Some(target_obligation) = self
            .attempt_obligation
            .get(target_call_id)
            .cloned()
            .flatten()
        else {
            return;
        };
        let target = TaskCheckSnapshot {
            latest: false,
            task_id,
            task_revision,
            expected_negative: self.expected_negative.get(target_call_id).cloned(),
            attempt: target_attempt,
            obligation: Some(target_obligation),
        };
        let source_calls = self
            .attempts
            .values()
            .filter_map(|attempt| {
                let source_order = self.terminal_event_order.get(&attempt.call_id);
                let imported_projection = self.imported_launch_projection.get(&attempt.call_id);
                let observation = match &attempt.status {
                    TaskCheckAttemptStatus::Finished(receipt) => receipt.observation.as_ref()?,
                    TaskCheckAttemptStatus::Pending | TaskCheckAttemptStatus::ChangedArtifacts => {
                        return None;
                    }
                };
                ((source_order.is_some_and(|source_order| source_order < &target_order)
                    || imported_projection.is_some())
                    && attempt.resolved_by.is_none()
                    && observation.launch_failure.is_some())
                .then(|| (attempt.call_id.clone(), imported_projection.cloned()))
            })
            .collect::<Vec<_>>();
        for (source_call_id, source_handoff_projection_sha256) in source_calls {
            let Some(kind) =
                self.attempts
                    .get(&source_call_id)
                    .and_then(|attempt| match &attempt.status {
                        TaskCheckAttemptStatus::Finished(receipt) => receipt.observation.as_ref(),
                        TaskCheckAttemptStatus::Pending
                        | TaskCheckAttemptStatus::ChangedArtifacts => None,
                    })
                    .and_then(|observation| observation.launch_failure.clone())
            else {
                continue;
            };
            let correction = TaskCheckLaunchCorrection {
                kind,
                target_call_id: target_call_id.to_owned(),
                source_event_ordinal: None,
                target_event_ordinal: None,
                source_handoff_projection_sha256,
                proof_ref: source_call_id.clone(),
                target_command: target_observation.command.clone(),
                target_cwd: target_observation.cwd.clone(),
                target_output_sha256: target_observation.diagnostic_sha256.clone(),
                target_output_ref: target_observation.output_ref.clone(),
            };
            if self.apply_launch_correction(&source_call_id, target.clone(), correction) {
                self.imported_launch_projection.remove(&source_call_id);
            }
        }
    }

    pub(crate) fn latest_for_task(
        &self,
        task_id: &str,
        task_revision: u64,
    ) -> Option<&TaskCheckAttempt> {
        let call_id = self
            .latest_attempt
            .get(&(task_id.to_owned(), task_revision))?;
        self.attempts.get(call_id)
    }

    pub(crate) fn attempt_for_call(&self, call_id: &str) -> Option<&TaskCheckAttempt> {
        self.attempts.get(call_id)
    }

    pub(crate) fn resolved_launch_obligation_keys(&self) -> Vec<String> {
        let mut keys = self
            .attempts
            .values()
            .filter_map(|source| {
                let correction = source.launch_correction.as_ref()?;
                if source.resolved_by.as_deref() != Some(correction.target_call_id.as_str())
                    || correction.proof_ref != source.call_id
                {
                    return None;
                }
                let TaskCheckAttemptStatus::Finished(source_receipt) = &source.status else {
                    return None;
                };
                let observation = source_receipt.observation.as_ref()?;
                if source_receipt.succeeded
                    || !source_receipt.completed
                    || source_receipt.exit_code == 0
                    || source_receipt.exit_code == -1
                    || observation.launch_failure.as_ref() != Some(&correction.kind)
                {
                    return None;
                }
                let target = self.attempts.get(&correction.target_call_id)?;
                let TaskCheckAttemptStatus::Finished(target_receipt) = &target.status else {
                    return None;
                };
                let source_obligation = self
                    .attempt_obligation
                    .get(&source.call_id)
                    .and_then(Option::as_ref)?;
                let target_obligation = self
                    .attempt_obligation
                    .get(&target.call_id)
                    .and_then(Option::as_ref)?;
                let source_stamps = canonical_artifact_hashes(source_receipt)?;
                let target_stamps = canonical_artifact_hashes(target_receipt)?;
                (target_receipt.call_id == correction.target_call_id
                    && target_receipt.succeeded
                    && target_receipt.completed
                    && target_receipt.exit_code == 0
                    && source_obligation
                        .scope
                        .same_execution(&target_obligation.scope)
                    && source_obligation.command_identity == target_obligation.command_identity
                    && source_stamps == target_stamps)
                    .then(|| source_obligation.key())
            })
            .collect::<Vec<_>>();
        let unresolved_keys = self
            .attempts
            .values()
            .filter_map(|attempt| {
                if attempt.resolved_by.is_some() || attempt.launch_correction.is_some() {
                    return None;
                }
                if !self.attempt_is_unresolved(attempt) {
                    return None;
                }
                self.attempt_obligation
                    .get(&attempt.call_id)
                    .and_then(Option::as_ref)
                    .map(TaskCheckObligation::key)
            })
            .collect::<std::collections::HashSet<_>>();
        keys.retain(|key| !unresolved_keys.contains(key));
        keys.sort();
        keys.dedup();
        keys
    }

    pub(crate) fn apply_launch_correction(
        &mut self,
        source_call_id: &str,
        mut target: TaskCheckSnapshot,
        correction: TaskCheckLaunchCorrection,
    ) -> bool {
        let Some(source) = self.attempts.get(source_call_id) else {
            return false;
        };
        let Some(source_obligation) = self
            .attempt_obligation
            .get(source_call_id)
            .and_then(Option::as_ref)
        else {
            return false;
        };
        let TaskCheckAttemptStatus::Finished(source_receipt) = &source.status else {
            return false;
        };
        let Some(observation) = source_receipt.observation.as_ref() else {
            return false;
        };
        let Some(target_obligation) = target.obligation.as_ref() else {
            return false;
        };
        let TaskCheckAttemptStatus::Finished(target_receipt) = &target.attempt.status else {
            return false;
        };
        let source_stamps = canonical_artifact_hashes(source_receipt);
        let target_stamps = canonical_artifact_hashes(target_receipt);
        let existing_target_matches = self
            .attempts
            .get(&target.attempt.call_id)
            .and_then(|attempt| match &attempt.status {
                TaskCheckAttemptStatus::Finished(receipt) => Some(receipt),
                TaskCheckAttemptStatus::Pending | TaskCheckAttemptStatus::ChangedArtifacts => None,
            })
            .is_some_and(|receipt| receipt == target_receipt)
            && self
                .attempt_obligation
                .get(&target.attempt.call_id)
                .and_then(Option::as_ref)
                .is_some_and(|obligation| obligation.key() == target_obligation.key());
        if source.resolved_by.is_some()
            || source.launch_correction.is_some()
            || source_receipt.succeeded
            || !source_receipt.completed
            || source_receipt.exit_code == 0
            || source_receipt.exit_code == -1
            || observation.cwd != source_receipt.workspace_root
            || !observation.diagnostic_complete
            || observation.diagnostic_truncated
            || observation.launch_failure.as_ref() != Some(&correction.kind)
            || correction.target_call_id != target.attempt.call_id
            || correction.proof_ref != source_call_id
            || match (
                correction.source_event_ordinal,
                correction.target_event_ordinal,
                correction.source_handoff_projection_sha256.as_deref(),
            ) {
                (Some(source), Some(target), None) => source >= target,
                (None, None, None) => false,
                (None, None, Some(hash)) => {
                    hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                }
                _ => true,
            }
            || target.attempt.call_id == source_call_id
            || target.task_id != source_receipt.task_id
            || target.task_revision != source_receipt.task_revision
            || !target_receipt.succeeded
            || !target_receipt.completed
            || target_receipt.exit_code != 0
            || !source_obligation
                .scope
                .same_execution(&target_obligation.scope)
            || source_obligation.command_identity != target_obligation.command_identity
            || match self.imported_launch_projection.get(source_call_id) {
                Some(hash) => correction.source_handoff_projection_sha256.as_ref() != Some(hash),
                None => correction.source_handoff_projection_sha256.is_some(),
            }
            || observation.command != correction.target_command
            || observation.command.last().is_none_or(|command| {
                normalized_check_identity(command) != source_obligation.command_identity
            })
            || correction.target_cwd != target_receipt.workspace_root
            || correction.target_output_ref != target.attempt.call_id
            || correction.target_output_sha256.len() != 64
            || !correction
                .target_output_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || source_stamps.is_none()
            || source_stamps != target_stamps
            || !launch_correction_has_target_correspondence(
                &correction.kind,
                observation,
                target_receipt,
            )
            || (self.attempts.contains_key(&target.attempt.call_id) && !existing_target_matches)
        {
            return false;
        }
        target.latest = false;
        let target_call_id = target.attempt.call_id.clone();
        if let Some(source) = self.attempts.get_mut(source_call_id) {
            source.resolved_by = Some(target_call_id.clone());
            source.launch_correction = Some(correction);
        }
        if !existing_target_matches {
            self.attempt_scope.insert(
                target_call_id.clone(),
                (target.task_id.clone(), target.task_revision),
            );
            self.attempt_obligation
                .insert(target_call_id.clone(), target.obligation);
            self.attempts.insert(target_call_id, target.attempt);
        }
        true
    }

    pub(crate) fn diagnostics_for_root(&self, root_task_id: &str) -> serde_json::Value {
        const MAX_ATTEMPTS: usize = 4;
        const MAX_STATUS_DIAGNOSTIC_BYTES: usize = 2 * 1024;
        const MAX_STATUS_BYTES: usize = 8 * 1024;
        let mut attempts = self
            .attempts
            .values()
            .filter_map(|attempt| {
                let obligation = self
                    .attempt_obligation
                    .get(&attempt.call_id)
                    .and_then(Option::as_ref)?;
                (obligation.scope.root_task_id == root_task_id).then_some((attempt, obligation))
            })
            .collect::<Vec<_>>();
        let priority =
            |attempt: &TaskCheckAttempt| {
                if self.attempt_is_unresolved(attempt) {
                    0
                } else if attempt.launch_correction.is_some() {
                    1
                } else if self.attempt_scope.get(&attempt.call_id).is_some_and(
                    |(task_id, revision)| {
                        self.latest_attempt
                            .get(&(task_id.clone(), *revision))
                            .is_some_and(|call_id| call_id == &attempt.call_id)
                    },
                ) {
                    2
                } else {
                    3
                }
            };
        attempts.sort_by(|(left, _), (right, _)| {
            priority(left)
                .cmp(&priority(right))
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        let count = attempts.len();
        let unresolved_count = attempts
            .iter()
            .filter(|(attempt, _)| self.attempt_is_unresolved(attempt))
            .count();
        let mut omitted = count.saturating_sub(MAX_ATTEMPTS);
        let mut remaining_diagnostic_bytes = MAX_STATUS_DIAGNOSTIC_BYTES;
        let mut rendered = attempts
            .into_iter()
            .take(MAX_ATTEMPTS)
            .map(|(attempt, obligation)| {
                let (status, receipt) = match &attempt.status {
                    TaskCheckAttemptStatus::Pending => ("pending", None),
                    TaskCheckAttemptStatus::ChangedArtifacts => ("changed_artifacts", None),
                    TaskCheckAttemptStatus::Finished(receipt) => ("finished", Some(receipt)),
                };
                let observation = receipt.and_then(|receipt| receipt.observation.as_ref());
                let diagnostic = observation.map(|observation| {
                    let available = observation.diagnostic_excerpt.len().min(remaining_diagnostic_bytes);
                    let end = (0..=available)
                        .rev()
                        .find(|index| observation.diagnostic_excerpt.is_char_boundary(*index))
                        .unwrap_or(0);
                    remaining_diagnostic_bytes = remaining_diagnostic_bytes.saturating_sub(end);
                    serde_json::json!({
                        "excerpt": &observation.diagnostic_excerpt[..end],
                        "truncated": observation.diagnostic_truncated || end < observation.diagnostic_excerpt.len(),
                        "complete": observation.diagnostic_complete && end == observation.diagnostic_excerpt.len(),
                        "sha256": observation.diagnostic_sha256,
                        "output_ref": observation.output_ref,
                    })
                });
                let bounded = |value: serde_json::Value, max_bytes: usize| {
                    let serialized = serde_json::to_vec(&value).unwrap_or_default();
                    let was_omitted = serialized.len() > max_bytes;
                    let sha256 = format!("{:x}", sha2::Sha256::digest(&serialized));
                    (if was_omitted { None } else { Some(value) }, sha256, was_omitted)
                };
                let (workspace_root, workspace_root_sha256, workspace_root_omitted) = bounded(
                    serde_json::json!(obligation.workspace_root),
                    384,
                );
                let (command_identity, command_identity_sha256, command_identity_omitted) =
                    bounded(serde_json::json!(obligation.command_identity), 768);
                let (artifact_paths, artifact_paths_sha256, artifact_paths_omitted) = bounded(
                    serde_json::json!(obligation.artifact_paths),
                    1024,
                );
                let (observed_command, observed_command_sha256, observed_command_omitted) =
                    bounded(
                        serde_json::json!(observation.map(|observation| &observation.command)),
                        768,
                    );
                let (observed_cwd, observed_cwd_sha256, observed_cwd_omitted) = bounded(
                    serde_json::json!(observation.map(|observation| &observation.cwd)),
                    384,
                );
                let (execution_environment_id, execution_environment_id_sha256, execution_environment_id_omitted) =
                    bounded(
                        serde_json::json!(observation.and_then(|observation| observation.execution_environment_id.as_ref())),
                        256,
                    );
                let correction = attempt.launch_correction.as_ref().map(|correction| {
                    let serialized = serde_json::to_vec(correction).unwrap_or_default();
                    serde_json::json!({
                        "kind": correction.kind,
                        "target_call_id": correction.target_call_id.chars().take(256).collect::<String>(),
                        "source_event_ordinal": correction.source_event_ordinal,
                        "target_event_ordinal": correction.target_event_ordinal,
                        "proof_ref": correction.proof_ref.chars().take(256).collect::<String>(),
                        "proof_sha256": format!("{:x}", sha2::Sha256::digest(serialized)),
                    })
                });
                serde_json::json!({
                    "call_id": attempt.call_id.chars().take(256).collect::<String>(),
                    "task_id": obligation.scope.task_id.chars().take(256).collect::<String>(),
                    "task_revision": obligation.scope.task_revision,
                    "root_task_id": obligation.scope.root_task_id.chars().take(256).collect::<String>(),
                    "begin_call_id": obligation.scope.begin_call_id.chars().take(256).collect::<String>(),
                    "workspace_root": workspace_root,
                    "workspace_root_sha256": workspace_root_sha256,
                    "workspace_root_omitted": workspace_root_omitted,
                    "command_identity": command_identity,
                    "command_identity_sha256": command_identity_sha256,
                    "command_identity_omitted": command_identity_omitted,
                    "artifact_paths": artifact_paths,
                    "artifact_paths_sha256": artifact_paths_sha256,
                    "artifact_paths_omitted": artifact_paths_omitted,
                    "status": status,
                    "exit_code": receipt.map(|receipt| receipt.exit_code),
                    "completed": receipt.map(|receipt| receipt.completed),
                    "succeeded": receipt.map(|receipt| receipt.succeeded),
                    "observed_command": observed_command,
                    "observed_command_sha256": observed_command_sha256,
                    "observed_command_omitted": observed_command_omitted,
                    "observed_cwd": observed_cwd,
                    "observed_cwd_sha256": observed_cwd_sha256,
                    "observed_cwd_omitted": observed_cwd_omitted,
                    "execution_environment_id": execution_environment_id,
                    "execution_environment_id_sha256": execution_environment_id_sha256,
                    "execution_environment_id_omitted": execution_environment_id_omitted,
                    "launch_phase": observation.and_then(|observation| observation.launch_failure.as_ref()),
                    "diagnostic": diagnostic,
                    "resolved_by": attempt.resolved_by,
                    "correction": correction,
                })
            })
            .collect::<Vec<_>>();
        let mut result = serde_json::json!({
            "count": count,
            "omitted": omitted,
            "unresolved_count": unresolved_count,
            "attempts": rendered,
        });
        while serde_json::to_vec(&result).is_ok_and(|bytes| bytes.len() > MAX_STATUS_BYTES)
            && !rendered.is_empty()
        {
            rendered.pop();
            omitted += 1;
            result["attempts"] = serde_json::json!(rendered);
            result["omitted"] = serde_json::json!(omitted);
        }
        result
    }

    pub(crate) fn unresolved_obligations(
        &self,
        scope: &crate::state::ActiveTaskScope,
    ) -> Vec<TaskCheckObligation> {
        let mut unresolved = HashMap::new();
        for (call, attempt) in &self.attempts {
            let Some(obligation) = self.attempt_obligation.get(call).and_then(Option::as_ref)
            else {
                continue;
            };
            if obligation.scope.same_execution(scope) && self.attempt_is_unresolved(attempt) {
                unresolved.insert(obligation.key(), obligation.clone());
            }
        }
        unresolved.into_values().collect()
    }

    pub(crate) fn unresolved_obligations_for_root(
        &self,
        root_task_id: &str,
    ) -> Vec<TaskCheckObligation> {
        let mut unresolved = HashMap::new();
        for (call, attempt) in &self.attempts {
            let Some(obligation) = self.attempt_obligation.get(call).and_then(Option::as_ref)
            else {
                continue;
            };
            if obligation.scope.root_task_id == root_task_id && self.attempt_is_unresolved(attempt)
            {
                unresolved.insert(obligation.key(), obligation.clone());
            }
        }
        unresolved.into_values().collect()
    }

    pub(crate) fn unresolved_count_for_scope(
        &self,
        scope: &crate::state::ActiveTaskScope,
    ) -> usize {
        self.unresolved_obligations(scope).len()
            + self
                .attempts
                .iter()
                .filter(|(call, attempt)| {
                    self.attempt_obligation
                        .get(*call)
                        .is_some_and(Option::is_none)
                        && self
                            .attempt_scope
                            .get(*call)
                            .is_some_and(|(task_id, revision)| {
                                task_id == &scope.task_id && *revision == scope.task_revision
                            })
                        && self.attempt_is_unresolved(attempt)
                })
                .count()
    }

    pub(crate) fn unresolved_count_for_root(&self, root_task_id: &str) -> usize {
        self.unresolved_obligations_for_root(root_task_id).len()
            + self
                .attempts
                .iter()
                .filter(|(call, attempt)| {
                    self.attempt_obligation
                        .get(*call)
                        .is_some_and(Option::is_none)
                        && self.attempt_is_unresolved(attempt)
                })
                .count()
    }
}

fn attempt_is_unresolved(attempt: &TaskCheckAttempt) -> bool {
    match &attempt.status {
        TaskCheckAttemptStatus::Pending | TaskCheckAttemptStatus::ChangedArtifacts => true,
        TaskCheckAttemptStatus::Finished(receipt) => {
            !receipt.succeeded && attempt.resolved_by.is_none()
        }
    }
}

impl TaskCheckObligation {
    pub(crate) fn new(
        scope: crate::state::ActiveTaskScope,
        workspace_root: PathBuf,
        command: &str,
        mut artifact_paths: Vec<PathBuf>,
    ) -> Option<Self> {
        let workspace_root = std::fs::canonicalize(workspace_root).ok()?;
        artifact_paths.sort();
        artifact_paths.dedup();
        Some(Self {
            scope,
            workspace_root,
            command_identity: normalized_check_identity(command),
            artifact_paths,
        })
    }

    pub(crate) fn key(&self) -> String {
        serde_json::to_string(&(
            &self.scope.root_task_id,
            &self.scope.task_id,
            self.scope.task_revision,
            &self.scope.begin_call_id,
            &self.workspace_root,
            &self.command_identity,
            &self.artifact_paths,
        ))
        .unwrap_or_default()
    }
}

pub(crate) fn normalized_check_identity(command: &str) -> String {
    if command.contains([';', '|', '&', '>', '<', '$', '`', '\n', '\r']) {
        return format!("raw:{command}");
    }
    let Some(mut words) = shlex::split(command) else {
        return format!("raw:{command}");
    };
    let is_python = words.first().is_some_and(|executable| {
        Path::new(executable)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "python" | "python3"))
    });
    let unittest_args = if is_python {
        let mut cursor = 1;
        loop {
            let Some(option) = words.get(cursor).map(String::as_str) else {
                break None;
            };
            if option == "-m" {
                break words
                    .get(cursor + 1)
                    .is_some_and(|module| module == "unittest")
                    .then_some(cursor + 2);
            }
            if matches!(option, "-W" | "-X") {
                cursor += 2;
                continue;
            }
            if option.starts_with('-')
                && (matches!(
                    option,
                    "-B" | "-b"
                        | "-d"
                        | "-E"
                        | "-h"
                        | "-i"
                        | "-I"
                        | "-O"
                        | "-OO"
                        | "-P"
                        | "-q"
                        | "-s"
                        | "-S"
                        | "-u"
                        | "-v"
                        | "-V"
                        | "-x"
                ) || option.starts_with("-W")
                    || option.starts_with("-X"))
            {
                cursor += 1;
                continue;
            }
            break None;
        }
    } else {
        None
    };
    if let Some(unittest_args) = unittest_args {
        let mut after_separator = false;
        let mut awaiting_option_value = false;
        let mut unknown_option = false;
        let mut index = 0;
        words.retain(|word| {
            let current = index;
            index += 1;
            if current < unittest_args {
                return true;
            }
            if word == "--" {
                after_separator = true;
                return true;
            }
            if after_separator {
                return true;
            }
            if awaiting_option_value {
                awaiting_option_value = false;
                return true;
            }
            if matches!(word.as_str(), "-v" | "--verbose") {
                return false;
            }
            if matches!(
                word.as_str(),
                "-k" | "-s"
                    | "-p"
                    | "-t"
                    | "--start-directory"
                    | "--pattern"
                    | "--top-level-directory"
            ) {
                awaiting_option_value = true;
                return true;
            }
            if word.starts_with('-')
                && !matches!(
                    word.as_str(),
                    "-q" | "--quiet"
                        | "-f"
                        | "--failfast"
                        | "-c"
                        | "--catch"
                        | "-b"
                        | "--buffer"
                        | "--locals"
                        | "-h"
                        | "--help"
                )
                && !["--start-directory=", "--pattern=", "--top-level-directory="]
                    .iter()
                    .any(|prefix| word.starts_with(prefix))
            {
                unknown_option = true;
            }
            true
        });
        if unknown_option {
            return format!("raw:{command}");
        }
    }
    format!(
        "tokens:{}",
        serde_json::to_string(&words).unwrap_or_default()
    )
}

pub(crate) fn hash_workspace_artifacts(
    root: &std::path::Path,
    paths: &[PathBuf],
) -> Option<Vec<TaskArtifactStamp>> {
    if paths.is_empty() || paths.len() > 16 {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let mut result = Vec::with_capacity(paths.len());
    for relative in paths {
        if relative.is_absolute()
            || relative.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return None;
        }
        let path = root.join(relative);
        let canonical = std::fs::canonicalize(&path).ok()?;
        if !canonical.starts_with(&root) {
            return None;
        }
        let metadata = std::fs::metadata(&canonical).ok()?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            return None;
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(&canonical).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            return None;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 16 * 1024 * 1024 {
            return None;
        }
        result.push(TaskArtifactStamp {
            path: relative.clone(),
            sha256: format!("{:x}", sha2::Sha256::digest(&bytes)),
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Some(result)
}

#[cfg(test)]
#[path = "task_check_tests.rs"]
mod tests;

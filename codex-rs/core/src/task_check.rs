//! Host-owned receipts for task-scoped verification commands.
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCheckAttempt {
    pub(crate) call_id: String,
    #[serde(default)]
    pub(crate) resolved_by: Option<String>,
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
                let unresolved = attempt_is_unresolved(attempt);
                (latest_task || unresolved || referenced_calls.contains(&attempt.call_id))
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
            restored.attempts.insert(call.clone(), entry.attempt);
        }
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
            let target_matches = restored
                .attempt_obligation
                .get(resolved_by)
                .and_then(Option::as_ref)
                .is_some_and(|obligation| obligation.key() == source_obligation.key());
            if !matches!(
                &attempt.status,
                TaskCheckAttemptStatus::Finished(receipt)
                    if receipt.completed
                        && receipt.exit_code != 0
                        && receipt.exit_code != -1
                        && !receipt.succeeded
            ) || !target_is_success
                || !target_matches
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
        self.latest_attempt.insert(key, call_id.clone());
        self.attempts.insert(
            call_id.clone(),
            TaskCheckAttempt {
                call_id: call_id.clone(),
                resolved_by: None,
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

    pub(crate) fn finish(
        &mut self,
        call_id: &str,
        exit_code: i32,
        completed: bool,
        after: Vec<TaskArtifactStamp>,
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
        let receipt = TaskCheckReceipt {
            call_id: call_id.to_owned(),
            task_id: request.task_id,
            task_revision: request.task_revision,
            workspace_root,
            artifact_before: before,
            artifact_after: after,
            exit_code,
            completed,
            succeeded: completed && exit_code == 0,
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
        Some(receipt)
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
            if obligation.scope.same_execution(scope) && attempt_is_unresolved(attempt) {
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
            if obligation.scope.root_task_id == root_task_id && attempt_is_unresolved(attempt) {
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
                        && attempt_is_unresolved(attempt)
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
                        && attempt_is_unresolved(attempt)
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

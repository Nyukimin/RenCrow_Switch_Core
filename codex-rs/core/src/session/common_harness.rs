use crate::common_harness_io::ArtifactBindingV1;
use crate::common_harness_io::ArtifactSetV1;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CheckOutputParserV1;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::ConditionKindV1;
use codex_history::task_context::ExecutionOwnerV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::OperationClaimKindV1;
use codex_history::task_context::OperationClaimV1;
use codex_history::task_context::OperationIntentStatusV1;
use codex_history::task_context::OperationIntentV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::StateFenceV1;
use codex_history::task_context::TaskRequestV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_history::task_context::UnitStatus;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

static COMMON_HARNESS_RUNTIME_OWNER_ID: OnceLock<String> = OnceLock::new();

/// Native operation inputs; writer identity, state fence and target are host-derived.
pub(crate) struct NativeOperationDefinition<'a> {
    pub(crate) operation_id: String,
    pub(crate) operation_kind: OperationKindV1,
    pub(crate) normalized_arguments: &'a [u8],
    pub(crate) normalized_arguments_ref: String,
    pub(crate) permission_decision_ref: String,
    pub(crate) executor_ref: String,
    pub(crate) timeout_ms: u64,
}

pub(crate) fn common_harness_runtime_owner_id(thread_id: &str) -> String {
    format!(
        "{}:thread:{thread_id}",
        COMMON_HARNESS_RUNTIME_OWNER_ID
            .get_or_init(|| format!("switch-runtime:{}", uuid::Uuid::now_v7()))
    )
}

impl Session {
    pub(crate) async fn reserve_common_harness_operation_guarded(
        &self,
        root_task_id: &str,
        definition: NativeOperationDefinition<'_>,
        guard: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(OperationIntentV1, String), String> {
        let NativeOperationDefinition {
            operation_id,
            operation_kind,
            normalized_arguments,
            normalized_arguments_ref,
            permission_decision_ref,
            executor_ref,
            timeout_ms,
        } = definition;
        if timeout_ms == 0 {
            return Err("common-harness operation timeout must be positive".into());
        }
        let state = self.current_common_harness_state(root_task_id).await?;
        if state.request.execution_owner != ExecutionOwnerV1::StandaloneSwitch {
            return Err("S1 common-harness cannot reserve native execution for an external runner-owned task".into());
        }
        let arguments_digest = format!("sha256:{:x}", Sha256::digest(normalized_arguments));
        let intent = OperationIntentV1 {
            operation_id: operation_id.clone(),
            task_id: state.request.task_id.clone(),
            request_revision: state.request.request_revision,
            attempt_no: state.run_state.attempt_no,
            owner_epoch: state.run_state.owner_epoch,
            expected_state_version: state.run_state.state_version,
            base_target_version: state.run_state.target_version.clone(),
            operation_kind,
            normalized_arguments_ref,
            normalized_arguments_digest: arguments_digest,
            permission_decision_ref,
            approval_ref: None,
            executor_ref,
            timeout_ms,
            status: OperationIntentStatusV1::Running,
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("read operation start time: {error}"))?
            .as_millis();
        let mut deadline_at_ms = i64::try_from(now_ms.saturating_add(u128::from(timeout_ms)))
            .map_err(|_| "common-harness operation deadline is out of range".to_string())?;
        if let Some(window) = state.run_state.work_window.as_ref() {
            deadline_at_ms = deadline_at_ms.min(window.deadline_at_ms);
            if i128::from(deadline_at_ms) <= now_ms as i128 {
                return Err(super::common_harness_deadline::WORK_WINDOW_EXPIRED.into());
            }
        }
        let intent_digest = intent.content_digest()?;
        let claim_id = format!("execution-claim:{operation_id}:{}", uuid::Uuid::now_v7());
        let claim = OperationClaimV1 {
            claim_id: claim_id.clone(),
            operation_id,
            previous_claim_id: None,
            owner_epoch: intent.owner_epoch,
            expected_state_version: intent.expected_state_version,
            intent_digest,
            claimed_at_ms: i64::try_from(now_ms)
                .map_err(|_| "common-harness claim timestamp is out of range".to_string())?,
            deadline_at_ms,
            kind: OperationClaimKindV1::Execution,
            supersedes_execution_claim_id: None,
            native_executor_dead_proof_ref: None,
        };
        self.apply_common_harness_host_event_guarded(
            root_task_id,
            common_harness_fence(&state),
            HostEventV1::ReserveOperation {
                intent: intent.clone(),
                claim,
            },
            guard,
        )
        .await?;
        Ok((intent, claim_id))
    }

    pub(crate) async fn current_common_harness_state(
        &self,
        root_task_id: &str,
    ) -> Result<CommonHarnessStateV1, String> {
        self.rencrow_completion_state(root_task_id)
            .await
            .and_then(|completion| completion.common_harness)
            .ok_or_else(|| "active completion state is not common-harness v1".to_string())
    }

    pub(crate) async fn common_harness_tool_state_for_turn(
        &self,
        turn: &TurnContext,
    ) -> Option<(String, CommonHarnessStateV1)> {
        let (root, common) = {
            let state = self.state.lock().await;
            let (root, completion) = state.rencrow_completion_state.as_ref()?;
            (root.clone(), completion.common_harness.clone()?)
        };
        let active_scope = self.active_task_scope().await;
        let scope_matches = active_scope
            .as_ref()
            .is_some_and(|scope| scope.root_task_id == root);
        let authorized_turn_matches = common.request.authorized_request_ref
            == format!("host-user-turn:{}:{}", self.thread_id, turn.sub_id);
        let nonterminal_state = !matches!(
            common.run_state.status,
            TaskRunStatusV1::Completed | TaskRunStatusV1::Cancelled | TaskRunStatusV1::Exhausted
        );
        // A nonterminal canonical request remains authoritative across turn IDs and cold
        // restore. Terminal history is in scope only for its bound execution or originating
        // user turn; a later unrelated read-only turn cannot revive it.
        if scope_matches || authorized_turn_matches || nonterminal_state {
            Some((root, common))
        } else {
            None
        }
    }

    pub(crate) async fn ensure_common_harness_attempt(
        &self,
        root_task_id: &str,
    ) -> Result<CommonHarnessStateV1, String> {
        let state = self.current_common_harness_state(root_task_id).await?;
        if state.run_state.status != TaskRunStatusV1::Queued {
            return Ok(state);
        }
        self.apply_common_harness_host_event(
            root_task_id,
            common_harness_fence(&state),
            HostEventV1::StartAttempt {
                target_version: state.run_state.target_version.clone(),
            },
        )
        .await
    }

    pub(crate) async fn begin_common_harness_task(
        &self,
        root_task_id: String,
        task_id: String,
        task_revision: u64,
        begin_call_id: String,
        mut request: TaskRequestV1,
        turn: &TurnContext,
    ) -> Result<(), String> {
        // S1 needs trace-backed settings receipts for every provider call.
        // Reject before target capture or persisted task/scope changes.
        if !self.services.rollout_thread_trace.is_enabled() {
            return Err(
                "common-harness/1.0 requires enabled rollout tracing to record inference settings receipts"
                    .into(),
            );
        }
        if root_task_id == task_id {
            return Err(
                "common-harness v1 root task ID must be distinct from its execution task ID".into(),
            );
        }
        let cwd = turn.config.cwd.as_path();
        if request.execution_owner != ExecutionOwnerV1::StandaloneSwitch {
            return Err(
                "S1 common-harness cannot execute tasks owned by an external runner".into(),
            );
        }
        let workspace_root = cwd
            .canonicalize()
            .map_err(|error| format!("resolve common-harness workspace: {error}"))?;
        let workspace_binding = ArtifactBindingV1::for_canonical_root(&workspace_root)
            .map_err(|error| format!("bind common-harness workspace: {error}"))?;
        let dependency_paths = request_dependency_paths(&request)?;
        let artifact_set =
            ArtifactSetV1::capture_bound(&workspace_root, &dependency_paths, &workspace_binding)
                .map_err(|error| format!("capture common-harness target: {error}"))?;
        if !artifact_set
            .matches_workspace()
            .map_err(|error| format!("recheck common-harness target: {error}"))?
        {
            return Err("common-harness target changed while Begin was binding it".into());
        }

        request.authorized_request_ref =
            format!("host-user-turn:{}:{}", self.thread_id, turn.sub_id);
        request.workspace_ref = workspace_binding.workspace_binding_ref.clone();
        request.start_target_version = artifact_set.digest.clone();
        request.permission_snapshot_ref = permission_snapshot_ref(&turn.config)?;
        request.operation_limits_ref = "switch-common-harness-operation-limits/1".into();
        request.skill_refs = self
            .bind_common_harness_skills(&request.skill_refs, turn)
            .await?;
        bind_common_harness_toolchains(&mut request).await?;

        let common_harness = CommonHarnessStateV1::queued(
            request.clone(),
            common_harness_runtime_owner_id(&self.thread_id.to_string()),
            1,
        )
        .map_err(|error| format!("invalid common-harness request: {error:?}"))?;
        let conditions = request
            .conditions
            .iter()
            .map(|condition| AcceptanceCondition {
                id: condition.condition_id.clone(),
                revision: condition.condition_revision,
                result: condition.expected.clone(),
                scope: condition.comparison_scope.clone(),
                verification: condition
                    .check
                    .as_ref()
                    .map(|check| check.argv.join(" "))
                    .unwrap_or_else(|| condition.verifier_ref.clone().unwrap_or_default()),
                acceptance_criteria: condition.expected.clone(),
                status: AcceptanceStatus::Open,
                evidence: Vec::new(),
                unmet_reason: None,
            })
            .collect::<Vec<_>>();
        let condition_refs = conditions
            .iter()
            .filter(|condition| {
                request
                    .conditions
                    .iter()
                    .find(|spec| spec.condition_id == condition.id)
                    .is_some_and(|spec| spec.required)
            })
            .map(|condition| condition.id.clone())
            .collect::<Vec<_>>();
        let completion_state = CompletionState {
            contract_revision: request.condition_set_revision,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused: false,
            consumed_host_events: Vec::new(),
            conditions,
            units: vec![CompletionUnit {
                id: task_id.clone(),
                purpose: request.goal.clone(),
                status: UnitStatus::Ready,
                kind: CompletionUnitKind::Execution,
                condition_refs,
                depends_on: Vec::new(),
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            }],
            current_unit: Some(task_id.clone()),
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: Some("Start the first common-harness attempt".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
            common_harness: Some(common_harness),
        };

        self.set_accepted_rencrow_completion_state(root_task_id.clone(), completion_state)
            .await?;
        self.begin_task_scope(
            root_task_id,
            task_id,
            task_revision,
            begin_call_id,
            /*allow_unfinished_root_rebind*/ false,
        )
        .await?;
        self.persist_autonomous_completion_snapshot().await
    }

    async fn bind_common_harness_skills(
        &self,
        requested: &[codex_history::task_context::SkillRefV1],
        turn: &TurnContext,
    ) -> Result<Vec<codex_history::task_context::SkillRefV1>, String> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let plugin_config = turn.config.plugins_config_input();
        let plugin_outcome = self
            .services
            .plugins_manager
            .plugins_for_config(&plugin_config)
            .await
            .without_plugins(&turn.disabled_plugin_ids);
        let skill_roots = plugin_outcome.effective_plugin_skill_roots();
        let plugin_snapshots = self
            .services
            .plugins_manager
            .plugin_skill_snapshots_for_config(&plugin_config);
        let skills_input = crate::skills_load_input_from_config(turn.config.as_ref(), skill_roots)
            .with_plugin_skill_snapshots(plugin_snapshots);
        let skill_snapshot = self
            .services
            .skills_service
            .for_request()
            .snapshot_for_cwd(&skills_input, /*force_reload*/ true, None)
            .await;
        let mut bound = Vec::with_capacity(requested.len());
        let mut seen = BTreeSet::new();
        for requested_skill in requested {
            if !seen.insert(requested_skill.id.as_str()) {
                return Err(format!(
                    "duplicate common-harness skill reference `{}`",
                    requested_skill.id
                ));
            }
            let skill = skill_snapshot
                .outcome()
                .skills
                .iter()
                .find(|skill| skill.name == requested_skill.id)
                .ok_or_else(|| {
                    format!(
                        "common-harness skill `{}` is not present in the host skill snapshot",
                        requested_skill.id
                    )
                })?;
            let content = skill_snapshot
                .read_skill_text(skill)
                .await
                .map_err(|error| format!("read host skill `{}`: {error}", skill.name))?;
            let content_digest = format!("sha256:{:x}", Sha256::digest(content.as_bytes()));
            bound.push(codex_history::task_context::SkillRefV1 {
                id: skill.name.clone(),
                version_ref: content_digest.clone(),
                content_digest,
                source_ref: skill.path_to_skills_md.to_string_lossy().into_owned(),
            });
        }
        Ok(bound)
    }
}

pub(super) fn common_harness_fence(state: &CommonHarnessStateV1) -> StateFenceV1 {
    StateFenceV1 {
        request_revision: state.request.request_revision,
        expected_state_version: state.run_state.state_version,
        owner_epoch: state.run_state.owner_epoch,
    }
}

pub(super) fn request_dependency_paths(request: &TaskRequestV1) -> Result<Vec<PathBuf>, String> {
    let mut paths = BTreeSet::new();
    for path in request.conditions.iter().flat_map(|condition| {
        condition.dependency_refs.iter().chain(
            condition
                .check
                .iter()
                .flat_map(|check| check.cwd_binding_refs.iter()),
        )
    }) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(format!(
                "common-harness dependency path `{}` must be workspace-relative",
                path.display()
            ));
        }
        paths.insert(path);
    }
    if paths.is_empty() {
        return Err("common-harness request must bind at least one dependency path".into());
    }
    Ok(paths.into_iter().collect())
}

pub(super) fn permission_snapshot_ref(config: &Config) -> Result<String, String> {
    let permissions = serde_json::to_vec(&serde_json::json!({
        "approval_policy": config.permissions.approval_policy.value(),
        "allow_login_shell": config.permissions.allow_login_shell,
        "effective_profile": config.permissions.effective_permission_profile(),
    }))
    .map_err(|error| format!("serialize current effective permission snapshot: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(permissions)))
}

async fn bind_common_harness_toolchains(request: &mut TaskRequestV1) -> Result<(), String> {
    const GO_VERIFIER_REF: &str = "codex-common-harness/go-test-json/1";
    if request.conditions.is_empty()
        || request
            .conditions
            .iter()
            .any(|condition| !condition.required)
    {
        return Err("common-harness v1 requires every registered condition to be required".into());
    }
    let mut host_go: Option<(PathBuf, String)> = None;
    for condition in &mut request.conditions {
        if !matches!(
            condition.kind,
            ConditionKindV1::TestSuite | ConditionKindV1::ReproduceFailure
        ) {
            return Err(format!(
                "common-harness condition kind {:?} has no S1 verifier adapter",
                condition.kind
            ));
        }
        if condition.verifier_ref.as_deref() != Some(GO_VERIFIER_REF)
            || condition.verifier_config_revision != Some(1)
        {
            return Err(format!(
                "common-harness condition `{}` must select verifier `{GO_VERIFIER_REF}` revision 1",
                condition.condition_id
            ));
        }
        let Some(check) = condition.check.as_mut() else {
            return Err(format!(
                "common-harness condition `{}` requires an executable verifier",
                condition.condition_id
            ));
        };
        if check.parser != CheckOutputParserV1::GoTestJson {
            return Err(format!(
                "common-harness condition `{}` selects an unsupported output parser",
                condition.condition_id
            ));
        }
        let selection = check.go_test_selection.as_ref().ok_or_else(|| {
            format!(
                "common-harness condition `{}` requires a selected Go test case set",
                condition.condition_id
            )
        })?;
        validate_go_test_selection(selection, &condition.condition_id)?;
        validate_go_expected_outcome(
            condition.kind,
            &check.expected_outcome,
            selection,
            &condition.condition_id,
        )?;
        if !check.allowed_environment_refs.is_empty() {
            return Err(format!(
                "common-harness Go verifier `{}` cannot select caller-defined environment variables",
                condition.condition_id
            ));
        }
        let package_arg = check.argv.get(6).ok_or_else(|| {
            format!(
                "Go verifier `{}` has no package argument",
                condition.condition_id
            )
        })?;
        validate_go_test_package(check, package_arg, &condition.condition_id)?;
        let expected_run_pattern = go_test_run_pattern(selection);
        if check.argv.len() != 7
            || check.argv[0] != "go"
            || check.argv[1] != "test"
            || check.argv[2] != "-json"
            || check.argv[3] != "-count=1"
            || check.argv[4] != "-run"
            || check.argv[5] != expected_run_pattern
        {
            return Err(format!(
                "Go JSON verifier `{}` must use the bounded direct argv form `go test -json -count=1 -run <selected-cases> <one-bound-package>`",
                condition.condition_id
            ));
        }
        if host_go.is_none() {
            let path_variable = std::env::var_os("PATH")
                .ok_or_else(|| "PATH is unavailable for the Go verifier".to_string())?;
            let executable_name = if cfg!(windows) { "go.exe" } else { "go" };
            let executable = std::env::split_paths(&path_variable)
                .map(|directory| directory.join(executable_name))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| "the host Go executable could not be resolved".to_string())?
                .canonicalize()
                .map_err(|error| format!("resolve host Go executable: {error}"))?;
            let version = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::process::Command::new(&executable)
                    .arg("version")
                    .output(),
            )
            .await
            .map_err(|_| "host Go toolchain version inspection timed out".to_string())?
            .map_err(|error| format!("inspect host Go toolchain: {error}"))?;
            if !version.status.success() {
                return Err("host Go toolchain version inspection failed".into());
            }
            let version = String::from_utf8(version.stdout)
                .map_err(|error| format!("decode host Go toolchain version: {error}"))?;
            host_go = Some((executable, version.trim().to_string()));
        }
        let (executable, version) = host_go
            .as_ref()
            .ok_or_else(|| "host Go toolchain resolution did not complete".to_string())?;
        check.argv[0] = executable.to_string_lossy().into_owned();
        check.toolchain_ref = Some(format!("{}:{version}", executable.display()));
    }
    Ok(())
}

fn validate_go_test_selection(
    selection: &codex_history::task_context::GoTestSelectionV1,
    condition_id: &str,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    if selection.expected_case_count == 0
        || selection.expected_case_count != selection.selected_cases.len()
        || selection.selected_cases.iter().any(|case| {
            !case.starts_with("Test")
                || case.len() <= 4
                || !case
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || !seen.insert(case.as_str())
        })
    {
        return Err(format!(
            "common-harness condition `{condition_id}` must select a unique bounded set of simple Go test names"
        ));
    }
    Ok(())
}

fn go_test_run_pattern(selection: &codex_history::task_context::GoTestSelectionV1) -> String {
    match selection.selected_cases.as_slice() {
        [case] => format!("^{case}$"),
        cases => format!("^({})$", cases.join("|")),
    }
}

fn validate_go_expected_outcome(
    kind: ConditionKindV1,
    outcome: &codex_history::task_context::ExpectedCheckOutcomeV1,
    selection: &codex_history::task_context::GoTestSelectionV1,
    condition_id: &str,
) -> Result<(), String> {
    use codex_history::task_context::ExpectedCheckOutcomeV1 as Outcome;
    let selected_count = u32::try_from(selection.expected_case_count)
        .map_err(|_| format!("condition `{condition_id}` selected case count is out of range"))?;
    match (kind, outcome) {
        (ConditionKindV1::TestSuite, Outcome::AllSelectedTestsPass) => Ok(()),
        (
            ConditionKindV1::TestSuite,
            Outcome::TestCounts {
                passed,
                failed: 0,
                skipped: 0,
            },
        ) if *passed == selected_count => Ok(()),
        (
            ConditionKindV1::ReproduceFailure,
            Outcome::SpecifiedTestFailure { test_name, reason }
            | Outcome::FailureIdentity { test_name, reason },
        ) if selection.selected_cases.len() == 1
            && selection.selected_cases[0] == *test_name
            && !reason.trim().is_empty() =>
        {
            Ok(())
        }
        _ => Err(format!(
            "common-harness condition `{condition_id}` has an expected outcome unsupported for its S1 Go test role"
        )),
    }
}

fn validate_go_test_package(
    check: &codex_history::task_context::CheckSpecV1,
    package_arg: &str,
    condition_id: &str,
) -> Result<(), String> {
    let package_prefix = if package_arg == "." {
        ""
    } else if let Some(package) = package_arg.strip_prefix("./")
        && !package.is_empty()
        && !package.contains('/')
        && package != "."
        && package != ".."
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        package
    } else {
        return Err(format!(
            "common-harness Go verifier `{condition_id}` must select one workspace-bound package"
        ));
    };
    let paths = &check.cwd_binding_refs;
    let has_module = paths.iter().any(|path| path == "go.mod");
    if !package_prefix.is_empty() {
        let package_path_prefix = format!("{package_prefix}/");
        return if paths
            .iter()
            .any(|path| path.starts_with(&package_path_prefix) && path.ends_with("_test.go"))
            && has_module
        {
            Ok(())
        } else {
            Err(format!(
                "common-harness Go verifier `{condition_id}` package is not covered by its path bindings"
            ))
        };
    }
    let has_test_file = paths
        .iter()
        .any(|path| !path.contains('/') && path.ends_with("_test.go"));
    if has_module && has_test_file {
        Ok(())
    } else {
        Err(format!(
            "common-harness Go verifier `{condition_id}` package is not covered by its path bindings"
        ))
    }
}

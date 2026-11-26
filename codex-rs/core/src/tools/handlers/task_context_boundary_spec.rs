// Modified by RenCrow Switch Core, 2026-11-26: expose required acceptance condition ID syntax.
// Modified by RenCrow Switch Core, 2026-11-24: describe exact revalidation scope and accepted execution dependency.
// Modified by RenCrow Switch Core, 2026-11-11: describe execution and review completion separately.
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const TASK_CONTEXT_BOUNDARY_TOOL_NAME: &str = "task_context_boundary";

fn action_shape(action: &str, fields: &[&str], required: &[&str]) -> JsonSchema {
    let mut properties = BTreeMap::from([(
        "action".to_string(),
        JsonSchema::string_enum(vec![json!(action)], None),
    )]);
    properties.extend(
        fields
            .iter()
            .map(|field| ((*field).to_string(), JsonSchema::default())),
    );
    JsonSchema::object(
        properties,
        Some(required.iter().map(|field| (*field).to_string()).collect()),
        Some(false.into()),
    )
}

fn common_harness_request_schema() -> JsonSchema {
    let object = |properties: BTreeMap<String, JsonSchema>, required: &[&str]| {
        JsonSchema::object(
            properties,
            Some(required.iter().map(|field| (*field).to_string()).collect()),
            Some(false.into()),
        )
    };
    let outcome_object =
        |name: &str, payload: JsonSchema| object(BTreeMap::from([(name.into(), payload)]), &[name]);
    let counts = object(
        BTreeMap::from([
            ("passed".into(), JsonSchema::number(None)),
            ("failed".into(), JsonSchema::number(None)),
            ("skipped".into(), JsonSchema::number(None)),
        ]),
        &["passed", "failed", "skipped"],
    );
    let failure = object(
        BTreeMap::from([
            ("test_name".into(), JsonSchema::string(None)),
            ("reason".into(), JsonSchema::string(None)),
        ]),
        &["test_name", "reason"],
    );
    let expected_outcome = JsonSchema::one_of(
        vec![
            outcome_object("all_selected_tests_pass", JsonSchema::null(None)),
            outcome_object("test_counts", counts),
            outcome_object("failure_identity", failure.clone()),
            outcome_object("specified_test_failure", failure),
        ],
        Some("Expected structured Go test result: all selected tests pass, exact counts, or a named expected failure.".into()),
    );
    let selection = object(
        BTreeMap::from([
            (
                "selected_cases".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
            ("expected_case_count".into(), JsonSchema::number(None)),
        ]),
        &["selected_cases", "expected_case_count"],
    );
    let skill_ref = object(
        BTreeMap::from([("id".into(), JsonSchema::string(None))]),
        &["id"],
    );
    let check = JsonSchema::object(
        BTreeMap::from([
            (
                "argv".into(),
                JsonSchema::array(
                    JsonSchema::string(None),
                    Some("Structured argv for the registered host Go test adapter; no shell command string.".into()),
                ),
            ),
            (
                "parser".into(),
                JsonSchema::string_enum(vec![json!("go_test_json")], None),
            ),
            ("expected_outcome".into(), expected_outcome),
            ("go_test_selection".into(), selection),
            (
                "cwd_binding_refs".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
            (
                "allowed_environment_refs".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
            (
                "toolchain_ref".into(),
                JsonSchema::string(Some("Optional host-owned metadata. Omit it and the host will resolve and bind the actual Go executable and version.".into())),
            ),
            ("timeout_ms".into(), JsonSchema::number(None)),
        ]),
        Some(vec![
            "argv".into(),
            "parser".into(),
            "expected_outcome".into(),
            "go_test_selection".into(),
            "cwd_binding_refs".into(),
            "allowed_environment_refs".into(),
            "timeout_ms".into(),
        ]),
        Some(false.into()),
    );
    let condition = JsonSchema::object(
        BTreeMap::from([
            ("condition_id".into(), JsonSchema::string(None)),
            ("condition_revision".into(), JsonSchema::number(None)),
            (
                "kind".into(),
                JsonSchema::string_enum(
                    vec![json!("test_suite"), json!("reproduce_failure")],
                    None,
                ),
            ),
            ("comparison_scope".into(), JsonSchema::string(None)),
            ("expected".into(), JsonSchema::string(None)),
            ("required".into(), JsonSchema::boolean(None)),
            (
                "verifier_ref".into(),
                JsonSchema::string_enum(vec![json!("codex-common-harness/go-test-json/1")], None),
            ),
            (
                "verifier_config_revision".into(),
                JsonSchema::number(Some("Registered verifier configuration revision 1.".into())),
            ),
            ("check".into(), check),
            (
                "dependency_refs".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
        ]),
        Some(vec![
            "condition_id".into(),
            "condition_revision".into(),
            "kind".into(),
            "comparison_scope".into(),
            "expected".into(),
            "required".into(),
            "verifier_ref".into(),
            "verifier_config_revision".into(),
            "check".into(),
            "dependency_refs".into(),
        ]),
        Some(false.into()),
    );
    JsonSchema::object(
        BTreeMap::from([
            (
                "contract_version".into(),
                JsonSchema::string_enum(vec![json!("common-harness/1.0")], None),
            ),
            ("task_id".into(), JsonSchema::string(None)),
            ("request_revision".into(), JsonSchema::number(None)),
            ("goal".into(), JsonSchema::string(None)),
            (
                "constraints".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
            ("conditions".into(), JsonSchema::array(condition, None)),
            ("condition_set_revision".into(), JsonSchema::number(None)),
            ("max_attempts".into(), JsonSchema::number(None)),
            (
                "operation_limits_ref".into(),
                JsonSchema::string(Some(
                    "Optional host-owned metadata; omit it and the host will bind current limits."
                        .into(),
                )),
            ),
            (
                "skill_refs".into(),
                JsonSchema::array(skill_ref, Some("Requested skill identifiers only. The host binds each skill's current version, digest, and source.".into())),
            ),
            (
                "execution_owner".into(),
                JsonSchema::string_enum(vec![json!("standalone_switch")], None),
            ),
            (
                "authorized_request_ref".into(),
                JsonSchema::string(Some("Optional host-owned metadata; omit it and the host binds the current user request.".into())),
            ),
            (
                "workspace_ref".into(),
                JsonSchema::string(Some("Optional host-owned metadata; omit it and the host binds the current workspace.".into())),
            ),
            (
                "start_target_version".into(),
                JsonSchema::string(Some("Optional host-owned metadata; omit it and the host captures the current target version.".into())),
            ),
            (
                "permission_snapshot_ref".into(),
                JsonSchema::string(Some("Optional host-owned metadata; omit it and the host binds effective current permissions.".into())),
            ),
        ]),
        Some(vec![
            "contract_version".into(),
            "task_id".into(),
            "request_revision".into(),
            "goal".into(),
            "constraints".into(),
            "conditions".into(),
            "condition_set_revision".into(),
            "max_attempts".into(),
            "skill_refs".into(),
            "execution_owner".into(),
        ]),
        Some(false.into()),
    )
}

pub(crate) fn create_task_context_boundary_tool(autonomous_completion: bool) -> ToolSpec {
    let evidence_claim = JsonSchema::object(
        BTreeMap::from([
            ("task_id".into(), JsonSchema::string(None)),
            ("task_revision".into(), JsonSchema::number(None)),
        ]),
        Some(vec!["task_id".into(), "task_revision".into()]),
        Some(false.into()),
    );
    let completion_state = JsonSchema::object(
        BTreeMap::from([
            ("contract_revision".into(), JsonSchema::number(None)),
            ("paused".into(), JsonSchema::boolean(None)),
            (
                "consumed_host_events".into(),
                JsonSchema::array(JsonSchema::string(None), None),
            ),
            (
                "conditions".into(),
                JsonSchema::array(
                    JsonSchema::object(
                        BTreeMap::from([
                            (
                                "id".into(),
                                JsonSchema::string(Some(
                                    "Unique acceptance condition ID starting with AC- (for example AC-1). Copy this exact ID into unit condition_refs.".into(),
                                )),
                            ),
                            ("revision".into(), JsonSchema::number(None)),
                            (
                                "result".into(),
                                JsonSchema::string(Some(
                                    "Target outcome requested by the user for this condition: state what should be true when it is satisfied, using the user's requested result. This is a target, not a claim that the work is already complete.".into(),
                                )),
                            ),
                            (
                                "scope".into(),
                                JsonSchema::string(Some(
                                    "The part of the user's requested work this condition covers.".into(),
                                )),
                            ),
                            (
                                "verification".into(),
                                JsonSchema::string(Some(
                                    "How this condition will be checked against the requested outcome.".into(),
                                )),
                            ),
                            (
                                "acceptance_criteria".into(),
                                JsonSchema::string(Some(
                                    "Observable pass criteria derived from the user's requested outcome.".into(),
                                )),
                            ),
                            (
                                "status".into(),
                                JsonSchema::string_enum(
                                    vec![
                                        json!("open"),
                                        json!("satisfied"),
                                        json!("rejected"),
                                        json!("waived"),
                                    ],
                                    None,
                                ),
                            ),
                            (
                                "evidence".into(),
                                JsonSchema::array(evidence_claim.clone(), None),
                            ),
                            ("unmet_reason".into(), JsonSchema::string(None)),
                        ]),
                        Some(vec![
                            "id".into(),
                            "revision".into(),
                            "result".into(),
                            "scope".into(),
                            "verification".into(),
                            "acceptance_criteria".into(),
                            "status".into(),
                        ]),
                        Some(false.into()),
                    ),
                    None,
                ),
            ),
            (
                "units".into(),
                JsonSchema::array(
                    JsonSchema::object(
                        BTreeMap::from([
                            ("id".into(), JsonSchema::string(None)),
                            ("purpose".into(), JsonSchema::string(None)),
                            (
                                "kind".into(),
                                JsonSchema::string_enum(
                                    vec![json!("execution"), json!("review")],
                                    Some("Execution units perform work; review units collect independent child review evidence.".into()),
                                ),
                            ),
                            (
                                "status".into(),
                                JsonSchema::string_enum(
                                    vec![
                                        json!("planned"),
                                        json!("ready"),
                                        json!("running"),
                                        json!("blocked"),
                                        json!("done"),
                                        json!("failed"),
                                        json!("cancelled"),
                                    ],
                                    None,
                                ),
                            ),
                            (
                                "condition_refs".into(),
                                JsonSchema::array(JsonSchema::string(None), None),
                            ),
                            (
                                "depends_on".into(),
                                JsonSchema::array(JsonSchema::string(None), None),
                            ),
                            (
                                "evidence".into(),
                                JsonSchema::array(evidence_claim.clone(), None),
                            ),
                            (
                                "task_refs".into(),
                                JsonSchema::array(evidence_claim.clone(), None),
                            ),
                            (
                                "review_target".into(),
                                JsonSchema::object(
                                    BTreeMap::from([
                                        ("task_id".into(), JsonSchema::string(None)),
                                        ("task_revision".into(), JsonSchema::number(None)),
                                    ]),
                                    Some(vec!["task_id".into(), "task_revision".into()]),
                                    Some(false.into()),
                                ),
                            ),
                            (
                                "review_revalidation_of".into(),
                                JsonSchema::string(Some(
                                    "On an explicit update, name the exact previously adopted Done review unit whose current artifact acceptance must be revalidated. Copy that source unit's condition_refs exactly, without adding or removing conditions. Set review_target to the current accepted task revision, which may belong to a different task, and include its execution unit in depends_on. The host validates the one-to-one relation; same-target reviews are independent unless this field names the source unit.".into(),
                                )),
                            ),
                            (
                                "review_decision_reason".into(),
                                JsonSchema::string(Some("Parent-authored reason for adopting the collected review.".into())),
                            ),
                        ]),
                        Some(vec!["id".into(), "purpose".into(), "status".into()]),
                        Some(false.into()),
                    ),
                    None,
                ),
            ),
            ("current_unit".into(), JsonSchema::string(None)),
            (
                "failed_approaches".into(),
                JsonSchema::array(
                    JsonSchema::object(
                        BTreeMap::from([
                            ("summary".into(), JsonSchema::string(None)),
                            ("cause".into(), JsonSchema::string(None)),
                            ("next_action".into(), JsonSchema::string(None)),
                            ("recorded_at".into(), JsonSchema::number(None)),
                        ]),
                        Some(vec![
                            "summary".into(),
                            "cause".into(),
                            "next_action".into(),
                            "recorded_at".into(),
                        ]),
                        Some(false.into()),
                    ),
                    None,
                ),
            ),
            (
                "blocker".into(),
                JsonSchema::object(
                    BTreeMap::from([
                        (
                            "category".into(),
                            JsonSchema::string_enum(
                                vec![
                                    json!("user_input"),
                                    json!("authority"),
                                    json!("external_service"),
                                    json!("missing_evidence"),
                                    json!("unknown"),
                                ],
                                None,
                            ),
                        ),
                        ("reason".into(), JsonSchema::string(None)),
                        ("required_input".into(), JsonSchema::string(None)),
                        ("authority".into(), JsonSchema::string(None)),
                        ("evidence".into(), JsonSchema::array(evidence_claim, None)),
                        ("recheck_condition".into(), JsonSchema::string(None)),
                    ]),
                    Some(vec!["category".into(), "reason".into()]),
                    Some(false.into()),
                ),
            ),
            ("next_action".into(), JsonSchema::string(None)),
            ("product_progress_at".into(), JsonSchema::number(None)),
            ("runtime_recovery_at".into(), JsonSchema::number(None)),
            ("activity_at".into(), JsonSchema::number(None)),
        ]),
        Some(vec!["contract_revision".into(), "conditions".into()]),
        Some(false.into()),
    );
    let task_properties = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some("Stable task ID.".into())),
        ),
        (
            "purpose".to_string(),
            JsonSchema::string(Some("Short task purpose.".into())),
        ),
        (
            "status".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("pending"),
                    json!("in_progress"),
                    json!("completed"),
                    json!("blocked"),
                ],
                Some("Task status.".into()),
            ),
        ),
        (
            "depends_on".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Task IDs this task depends on.".into()),
            ),
        ),
    ]);
    let mut properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                if autonomous_completion {
                    vec![
                        json!("begin"),
                        json!("update"),
                        json!("status"),
                        json!("plan_review"),
                        json!("accept"),
                        json!("review"),
                        json!("adopt_review"),
                    ]
                } else {
                    vec![json!("begin"), json!("accept")]
                },
                Some(if autonomous_completion {
                    "Begin a host-tracked task scope, update its finite acceptance ledger, review a changed plan, inspect host contract state, prepare acceptance from host-verified evidence, or request and adopt a host-routed review."
                } else {
                    "Begin a host-tracked task scope or prepare acceptance after host verification."
                }.into()),
            ),
        ),
        (
            "root_task_id".to_string(),
            JsonSchema::string(Some("Stable root task identifier.".into())),
        ),
        (
            "task_id".to_string(),
            JsonSchema::string(Some("Stable task identifier for this scope.".into())),
        ),
        (
            "task_revision".to_string(),
            JsonSchema::number(Some("Revision of the task being started.".into())),
        ),
        (
            "result".to_string(),
            JsonSchema::string(Some("Parent's concise result description.".into())),
        ),
    ]);

    if autonomous_completion {
        properties.insert(
            "common_harness_request".into(),
            common_harness_request_schema(),
        );
        properties.insert("completion_state".into(), completion_state);
        properties.insert(
            "satisfied_conditions".into(),
            JsonSchema::array(JsonSchema::string(None), Some("Acceptance condition IDs explicitly adopted by the parent from this scope's verified receipt.".into())),
        );
        properties.insert(
            "previous_plan".into(),
            JsonSchema::string(Some("Host-registered plan before review.".into())),
        );
        properties.insert(
            "revised_plan".into(),
            JsonSchema::string(Some("Changed plan after review.".into())),
        );
        properties.insert(
            "reason".into(),
            JsonSchema::string(Some("Evidence-based reason for the changed plan.".into())),
        );
        properties.insert(
            "target_task_id".into(),
            JsonSchema::string(Some("Accepted execution task ID to review.".into())),
        );
        properties.insert(
            "unit_id".into(),
            JsonSchema::string(Some(
                "Review unit ID returned by the host review operation.".into(),
            )),
        );
        properties.insert(
            "review_retry".into(),
            JsonSchema::object(
                BTreeMap::from([
                    ("unit_id".into(), JsonSchema::string(None)),
                    ("expected_attempt_id".into(), JsonSchema::string(None)),
                ]),
                Some(vec!["unit_id".into(), "expected_attempt_id".into()]),
                Some(false.into()),
            ),
        );
    } else {
        properties.insert(
            "goal".into(),
            JsonSchema::string(Some("Root task goal.".into())),
        );
        properties.insert(
            "tasks".into(),
            JsonSchema::array(
                JsonSchema::object(
                    task_properties,
                    Some(vec![
                        "id".to_string(),
                        "purpose".to_string(),
                        "status".to_string(),
                    ]),
                    Some(false.into()),
                ),
                Some("Current task board.".into()),
            ),
        );
        properties.insert(
            "current_task_id".into(),
            JsonSchema::string(Some("Next active task ID, if any.".into())),
        );
        properties.insert(
            "decisions".into(),
            JsonSchema::array(JsonSchema::string(None), Some("Accepted decisions.".into())),
        );
        properties.insert(
            "constraints".into(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some("Continuing constraints.".into()),
            ),
        );
    }

    let mut parameters = JsonSchema::object(
        properties,
        Some(vec!["action".to_string()]),
        Some(false.into()),
    );
    if autonomous_completion {
        parameters.one_of = Some(vec![
            action_shape(
                "begin",
                &[
                    "root_task_id",
                    "task_id",
                    "task_revision",
                    "common_harness_request",
                ],
                &[
                    "action",
                    "root_task_id",
                    "task_id",
                    "task_revision",
                    "common_harness_request",
                ],
            ),
            action_shape(
                "begin",
                &[
                    "root_task_id",
                    "task_id",
                    "task_revision",
                    "completion_state",
                ],
                &["action", "root_task_id", "task_id", "task_revision"],
            ),
            action_shape(
                "update",
                &["root_task_id", "completion_state"],
                &["action", "root_task_id", "completion_state"],
            ),
            action_shape("status", &["root_task_id"], &["action"]),
            action_shape(
                "plan_review",
                &[
                    "root_task_id",
                    "previous_plan",
                    "revised_plan",
                    "reason",
                    "review_retry",
                ],
                &[
                    "action",
                    "root_task_id",
                    "previous_plan",
                    "revised_plan",
                    "reason",
                ],
            ),
            action_shape(
                "accept",
                &["result", "satisfied_conditions"],
                &["action", "result"],
            ),
            action_shape(
                "review",
                &["target_task_id", "unit_id"],
                &["action", "target_task_id"],
            ),
            action_shape(
                "adopt_review",
                &["unit_id", "reason"],
                &["action", "unit_id", "reason"],
            ),
        ]);
    }

    ToolSpec::Function(ResponsesApiTool {
        name: TASK_CONTEXT_BOUNDARY_TOOL_NAME.to_string(),
        description: if autonomous_completion {
            "Workflow: register the root with completion_state initially and when changing the plan. For a bounded common-harness task, call begin with common_harness_request containing the goal, constraints, acceptance conditions, a typed Go JSON test check, selected cases, and requested skill IDs. Omit host-owned references such as authorized_request_ref, workspace_ref, start_target_version, permission_snapshot_ref, and operation_limits_ref; the host binds them from the current session. For execution units only: begin the task, perform work, run checks with exec task_evidence, then accept and confirm the host checkpoint committed. For review, after an execution task is accepted, call review(target_task_id[, unit_id]); the host resolves its accepted latest revision and exact review obligation, applies the read-only reviewer route, and persists a reservation before spawning. Wait for that child result, then call adopt_review(unit_id, reason) to record whether to adopt the review. To retry an exact failed attempt, first call plan_review with review_retry containing the review unit_id and expected_attempt_id; then call review for that same unit. Use update to explicitly withdraw only a never-launched planned review that the host can validate; an attempt with a spawn or unknown outcome cannot be withdrawn. When changed artifact bytes invalidate a previously adopted review, add a new planned review with review_revalidation_of naming that exact adopted unit in the update; the host binds the relationship and the new review must be adopted before the current obligation is complete. Same-target reviews remain independent unless this exact relation is provided. Do not resend host review evidence. Use status to retrieve host facts. Once all canonical conditions and required review obligations are complete and no unresolved work or uncollected children remain, give the final report with evidence references; no additional root check or root accept is required. Receipt hashes, task board, goal, decisions, constraints, and timestamps are host-generated. Actions: begin(root_task_id, task_id, task_revision, common_harness_request); update(root_task_id, completion_state); status([root_task_id]); plan_review(root_task_id, previous_plan, revised_plan, reason[, review_retry]); accept(result[, satisfied_conditions]); review(target_task_id[, unit_id]); adopt_review(unit_id, reason)."
        } else {
            "Begin an execution scope or prepare acceptance after host verification."
        }
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

pub(crate) fn create_common_harness_root_tool() -> ToolSpec {
    let ToolSpec::Function(mut tool) = create_task_context_boundary_tool(true) else {
        unreachable!("task_context_boundary is a function tool")
    };
    tool.description = "Inspect bounded common-harness status or request acceptance after native host verification. Other task-board and review actions are unavailable while common-harness v1 is active.".into();
    tool.parameters.one_of = Some(vec![
        action_shape("status", &["root_task_id"], &["action"]),
        action_shape("accept", &["result"], &["action", "result"]),
    ]);
    ToolSpec::Function(tool)
}

pub(crate) fn create_common_harness_status_tool() -> ToolSpec {
    let ToolSpec::Function(mut tool) = create_task_context_boundary_tool(true) else {
        unreachable!("task_context_boundary is a function tool")
    };
    tool.description = "Read current common-harness status. This reviewer route is read-only and supports no other task action.".into();
    tool.parameters.one_of = Some(vec![action_shape("status", &["root_task_id"], &["action"])]);
    ToolSpec::Function(tool)
}

pub(crate) fn create_common_harness_reserved_begin_tool() -> ToolSpec {
    let ToolSpec::Function(mut tool) = create_task_context_boundary_tool(true) else {
        unreachable!("task_context_boundary is a function tool")
    };
    tool.description = "Begin only the exact execution unit reserved by the host after a completed target changed. Omit common_harness_request and completion_state; the host validates this real Begin call against its saved reservation.".into();
    tool.parameters.one_of = Some(vec![
        action_shape("status", &["root_task_id"], &["action"]),
        action_shape(
            "begin",
            &["root_task_id", "task_id", "task_revision"],
            &["action", "root_task_id", "task_id", "task_revision"],
        ),
    ]);
    ToolSpec::Function(tool)
}

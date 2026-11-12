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
                            ("id".into(), JsonSchema::string(None)),
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
                &["root_task_id", "previous_plan", "revised_plan", "reason"],
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
            action_shape("review", &["target_task_id"], &["action", "target_task_id"]),
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
            "Workflow: register the root with completion_state initially and when changing the plan. For execution units only: begin the task, perform work, run checks with exec task_evidence, then accept and confirm the host checkpoint committed. For review, after an execution task is accepted, call review(target_task_id); the host resolves its accepted latest revision and review unit, applies the read-only reviewer route, and spawns and binds the reviewer. Wait for that child result, then call adopt_review(unit_id, reason) to record whether to adopt the review. Do not resend completion_state for review. Use status to retrieve host facts. Once all canonical conditions and required units are complete and no unresolved work or uncollected children remain, give the final report with evidence references; no additional root check or root accept is required. Receipt hashes, task board, goal, decisions, constraints, and timestamps are host-generated. Actions: begin(root_task_id, task_id, task_revision[, completion_state]); update(root_task_id, completion_state); status([root_task_id]); plan_review(root_task_id, previous_plan, revised_plan, reason); accept(result[, satisfied_conditions]); review(target_task_id); adopt_review(unit_id, reason)."
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

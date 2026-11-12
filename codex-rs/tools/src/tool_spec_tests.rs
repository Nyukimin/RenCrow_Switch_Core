use super::ResponsesApiNamespace;
use super::ResponsesApiWebSearchFilters;
use super::ResponsesApiWebSearchUserLocation;
use super::ToolSpec;
use crate::AdditionalProperties;
use crate::FreeformTool;
use crate::FreeformToolFormat;
use crate::JsonSchema;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::concise_tool_descriptions;
use crate::create_tools_json_for_responses_api;
use crate::create_tools_json_for_responses_lite;
use crate::create_tools_raw_json_for_responses_api;
use crate::tool_spec::create_tools_json_for_responses_api_with_description_policy;
use crate::tool_spec::create_tools_json_for_responses_lite_with_description_policy;
use crate::tool_spec::create_tools_raw_json_for_responses_api_with_description_policy;
use codex_protocol::config_types::WebSearchContextSize;
use codex_protocol::config_types::WebSearchFilters as ConfigWebSearchFilters;
use codex_protocol::config_types::WebSearchUserLocation as ConfigWebSearchUserLocation;
use codex_protocol::config_types::WebSearchUserLocationType;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn tool_spec_name_covers_all_variants() {
    assert_eq!(
        ToolSpec::Function(ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: None,
        })
        .name(),
        "lookup_order"
    );
    assert_eq!(
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "mcp__demo__".to_string(),
            description: "Demo tools".to_string(),
            tools: Vec::new(),
        })
        .name(),
        "mcp__demo__"
    );
    assert_eq!(
        ToolSpec::ToolSearch {
            execution: "sync".to_string(),
            description: "Search for tools".to_string(),
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None
            ),
        }
        .name(),
        "tool_search"
    );
    assert_eq!(
        ToolSpec::WebSearch {
            external_web_access: Some(true),
            indexed_web_access: None,
            filters: None,
            user_location: None,
            search_context_size: None,
            search_content_types: None,
        }
        .name(),
        "web_search"
    );
    assert_eq!(
        ToolSpec::Freeform(FreeformTool {
            name: "exec".to_string(),
            description: "Run a command".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: \"exec\"".to_string(),
            },
        })
        .name(),
        "exec"
    );
}

#[test]
fn web_search_config_converts_to_responses_api_types() {
    assert_eq!(
        ResponsesApiWebSearchFilters::from(ConfigWebSearchFilters {
            allowed_domains: Some(vec!["example.com".to_string()]),
        }),
        ResponsesApiWebSearchFilters {
            allowed_domains: Some(vec!["example.com".to_string()]),
        }
    );
    assert_eq!(
        ResponsesApiWebSearchUserLocation::from(ConfigWebSearchUserLocation {
            r#type: WebSearchUserLocationType::Approximate,
            country: Some("US".to_string()),
            region: Some("California".to_string()),
            city: Some("San Francisco".to_string()),
            timezone: Some("America/Los_Angeles".to_string()),
        }),
        ResponsesApiWebSearchUserLocation {
            r#type: WebSearchUserLocationType::Approximate,
            country: Some("US".to_string()),
            region: Some("California".to_string()),
            city: Some("San Francisco".to_string()),
            timezone: Some("America/Los_Angeles".to_string()),
        }
    );
}

#[test]
fn create_tools_json_for_responses_api_includes_top_level_name() {
    assert_eq!(
        create_tools_json_for_responses_api(&[ToolSpec::Function(ResponsesApiTool {
            name: "demo".to_string(),
            description: "A demo tool".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([("foo".to_string(), JsonSchema::string(/*description*/ None),)]),
                /*required*/ None,
                /*additional_properties*/ None
            ),
            output_schema: None,
        })])
        .expect("serialize tools"),
        vec![json!({
            "type": "function",
            "name": "demo",
            "description": "A demo tool",
            "strict": false,
            "parameters": {
                "type": "object",
                "properties": {
                    "foo": { "type": "string" }
                },
            },
        })]
    );
}

fn responses_lite_function(name: &str) -> ResponsesApiTool {
    ResponsesApiTool {
        name: name.to_string(),
        description: format!("{name} tool"),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::new(),
            /*required*/ None,
            /*additional_properties*/ None,
        ),
        output_schema: None,
    }
}

#[test]
fn responses_lite_groups_default_function_and_custom_tools() {
    let tools = create_tools_json_for_responses_lite(&[
        ToolSpec::ToolSearch {
            execution: "client".to_string(),
            description: "Search tools".to_string(),
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
        },
        ToolSpec::Function(responses_lite_function("lookup_order")),
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "editor".to_string(),
            description: "Editing tools".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(
                responses_lite_function("edit"),
            )],
        }),
        ToolSpec::Freeform(FreeformTool {
            name: "exec".to_string(),
            description: "Run code".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: /.+/".to_string(),
            },
        }),
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "functions".to_string(),
            description: "Existing default tools".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(
                responses_lite_function("existing"),
            )],
        }),
        ToolSpec::Function(responses_lite_function("last")),
    ])
    .expect("serialize Responses Lite tools");

    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0]["type"], "tool_search");
    assert_eq!(tools[2]["type"], "namespace");
    assert_eq!(tools[2]["name"], "editor");
    assert_eq!(tools[2]["tools"][0]["name"], "edit");
    assert_eq!(tools[1]["type"], "namespace");
    assert_eq!(tools[1]["name"], "functions");
    assert_eq!(tools[1]["description"], "Existing default tools");
    assert_eq!(
        tools[1]["tools"]
            .as_array()
            .expect("functions namespace should contain tools")
            .iter()
            .map(|tool| (tool["type"].as_str(), tool["name"].as_str()))
            .collect::<Vec<_>>(),
        [
            (Some("function"), Some("lookup_order")),
            (Some("custom"), Some("exec")),
            (Some("function"), Some("existing")),
            (Some("function"), Some("last")),
        ]
    );
}

#[test]
fn responses_lite_preserves_empty_functions_namespace_description() {
    let tools = create_tools_json_for_responses_lite(&[ToolSpec::Function(
        responses_lite_function("lookup_order"),
    )])
    .expect("serialize Responses Lite tools");
    assert_eq!(tools[0]["description"], "");
}

#[test]
fn responses_lite_does_not_add_an_empty_functions_namespace() {
    let tools =
        create_tools_json_for_responses_lite(&[ToolSpec::Namespace(ResponsesApiNamespace {
            name: "functions".to_string(),
            description: "Empty default tools".to_string(),
            tools: Vec::new(),
        })])
        .expect("serialize Responses Lite tools");
    assert!(tools.is_empty());
}

#[test]
fn raw_tool_json_matches_value_encoding() {
    let specs = vec![ToolSpec::Function(ResponsesApiTool {
        name: "demo".to_string(),
        description: "A demo tool".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::new(),
            /*required*/ None,
            /*additional_properties*/ None,
        ),
        output_schema: None,
    })];
    let expected = create_tools_json_for_responses_api(&specs).expect("serialize tools");
    let raw = create_tools_raw_json_for_responses_api(&specs).expect("serialize raw tools");

    assert_eq!(
        serde_json::from_str::<Vec<serde_json::Value>>(raw.get()).expect("parse raw tools"),
        expected,
    );
}

const UPDATE_PLAN_ORIGINAL: &str = "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n";
const UPDATE_PLAN_CONCISE: &str = "Update the task plan with an optional explanation and step statuses. At most one step may be in progress.";
const SLEEP_ORIGINAL: &str = "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time.";
const SLEEP_CONCISE: &str = "Pause for the requested duration; new input ends the wait early. Returns the elapsed wall-clock time.";

fn concise_description_fixture() -> Vec<ToolSpec> {
    vec![
        ToolSpec::Function(ResponsesApiTool {
            name: "update_plan".to_string(),
            description: UPDATE_PLAN_ORIGINAL.to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "plan".to_string(),
                    JsonSchema::array(JsonSchema::string(Some("Plan item".to_string())), None),
                )]),
                Some(vec!["plan".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        }),
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "clock".to_string(),
            description: "Clock tools".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "sleep".to_string(),
                description: SLEEP_ORIGINAL.to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::new(),
                    Some(vec!["duration_ms".to_string()]),
                    Some(false.into()),
                ),
                output_schema: None,
            })],
        }),
        // Same name and description in another namespace is not the builtin.
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "mcp__clock".to_string(),
            description: "External tools".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: "sleep".to_string(),
                description: SLEEP_ORIGINAL.to_string(),
                strict: true,
                defer_loading: Some(false),
                parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
                output_schema: None,
            })],
        }),
        // A changed builtin description is an unknown version and stays intact.
        ToolSpec::Function(ResponsesApiTool {
            name: "update_plan".to_string(),
            description: format!("{}Changed.", UPDATE_PLAN_ORIGINAL),
            strict: true,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), None, None),
            output_schema: None,
        }),
        // Freeform exec can carry nested tool declarations in description and grammar.
        ToolSpec::Freeform(FreeformTool {
            name: "exec".to_string(),
            description: format!("Exec grammar with nested declaration: {}", SLEEP_ORIGINAL),
            defer_loading: Some(true),
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: nested_tool_call\nnested_tool_call: /.*\\/v1\\/responses.*/"
                    .to_string(),
            },
        }),
        ToolSpec::Function(ResponsesApiTool {
            name: "spawn_agent".to_string(),
            description:
                "Available roles and currently selectable models: explorer, verifier, worker"
                    .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
            output_schema: None,
        }),
    ]
}

#[test]
fn concise_builtin_catalog_entries_match_fixed_source_descriptions() {
    use crate::concise_tool_descriptions::ToolKind;

    let cases = [
        (
            None,
            "wait_agent",
            "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. The wait also ends early when new user input is steered into the active turn. Does not return the content; returns either a summary of which agents have updates (if any), an interruption summary for steered input, or a timeout summary if no activity arrives before the deadline.",
            "Wait for live-agent mailbox updates. Returns a summary, not update contents, and ends early if user input is steered in. Set a timeout to avoid indefinite waits.",
        ),
        (
            None,
            "followup_task",
            "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes.",
            "Send a follow-up to an existing non-root agent. It starts a new turn if idle; a running agent receives it at a sampling boundary or after its current tool call.",
        ),
        (
            None,
            "view_image",
            "View a local image file from the filesystem when visual inspection is needed. Use this tool when images are available on the local filesystem.",
            "View a local image file already on disk when visual inspection is needed.",
        ),
        (
            None,
            "list_agents",
            "List live agents in the current root thread tree. Optionally filter by task-path prefix.",
            "List live agents, optionally filtered by task-path prefix.",
        ),
        (
            Some("multi_agent_v1"),
            "send_input",
            "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task.",
            "Send a message to an existing agent. Set interrupt=true to redirect immediately; otherwise it is queued. Reuse the agent when the task depends on its context.",
        ),
        (
            Some("multi_agent_v1"),
            "wait_agent",
            "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status.",
            "Wait for agents to finish. A completed status may include the agent's final message; timeout returns an empty status. Completion also sends a notification with the same status.",
        ),
        (
            Some("multi_agent_v1"),
            "close_agent",
            "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Completed agents remain open and count toward the concurrency limit until closed. Don't keep agents open for too long if they are not needed anymore.",
            "Close an agent and its open descendants. Returns its status before shutdown. Completed agents count against the concurrency limit until closed, so close unneeded agents.",
        ),
        (
            Some("multi_agent_v1"),
            "resume_agent",
            "Resume a previously closed agent by id so it can receive send_input and wait_agent calls.",
            "Resume a closed agent so it can receive send_input and wait_agent calls.",
        ),
    ];

    for (namespace, name, original, concise) in cases {
        assert_eq!(
            concise_tool_descriptions::concise_description(
                ToolKind::Function,
                namespace,
                name,
                original,
            ),
            Some(concise),
            "catalog entry for {namespace:?}/{name}"
        );
        assert!(concise.len() < original.len(), "{namespace:?}/{name}");
    }
}

#[test]
fn concise_builtin_catalog_byte_report() {
    fn function(name: &str, description: &str) -> ResponsesApiTool {
        ResponsesApiTool {
            name: name.to_string(),
            description: description.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), None, Some(false.into())),
            output_schema: None,
        }
    }

    let multi_agent = ResponsesApiNamespace {
        name: "multi_agent_v1".to_string(),
        description: "Tools for spawning and managing sub-agents.".to_string(),
        tools: vec![
            ResponsesApiNamespaceTool::Function(function(
                "send_input",
                "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task.",
            )),
            ResponsesApiNamespaceTool::Function(function(
                "wait_agent",
                "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status.",
            )),
            ResponsesApiNamespaceTool::Function(function(
                "close_agent",
                "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Completed agents remain open and count toward the concurrency limit until closed. Don't keep agents open for too long if they are not needed anymore.",
            )),
            ResponsesApiNamespaceTool::Function(function(
                "resume_agent",
                "Resume a previously closed agent by id so it can receive send_input and wait_agent calls.",
            )),
        ],
    };
    let specs = vec![
        ToolSpec::Function(function("update_plan", UPDATE_PLAN_ORIGINAL)),
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: "clock".to_string(),
            description: "Tools for reading and waiting on time.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(function(
                "sleep",
                SLEEP_ORIGINAL,
            ))],
        }),
        ToolSpec::Namespace(multi_agent),
        ToolSpec::Function(function(
            "wait_agent",
            "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. The wait also ends early when new user input is steered into the active turn. Does not return the content; returns either a summary of which agents have updates (if any), an interruption summary for steered input, or a timeout summary if no activity arrives before the deadline.",
        )),
        ToolSpec::Function(function(
            "followup_task",
            "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes.",
        )),
        ToolSpec::Function(function(
            "view_image",
            "View a local image file from the filesystem when visual inspection is needed. Use this tool when images are available on the local filesystem.",
        )),
        ToolSpec::Function(function(
            "list_agents",
            "List live agents in the current root thread tree. Optionally filter by task-path prefix.",
        )),
    ];
    let before_value =
        create_tools_json_for_responses_api_with_description_policy(&specs, false).unwrap();
    let after_value =
        create_tools_json_for_responses_api_with_description_policy(&specs, true).unwrap();
    let before = serde_json::to_vec(&before_value).unwrap();
    let after = serde_json::to_vec(&after_value).unwrap();
    assert!(after.len() < before.len());

    let reductions = [
        (
            "functions/update_plan",
            UPDATE_PLAN_ORIGINAL,
            UPDATE_PLAN_CONCISE,
        ),
        ("clock/sleep", SLEEP_ORIGINAL, SLEEP_CONCISE),
        (
            "multi_agent_v1/send_input",
            "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task.",
            "Send a message to an existing agent. Set interrupt=true to redirect immediately; otherwise it is queued. Reuse the agent when the task depends on its context.",
        ),
        (
            "multi_agent_v1/wait_agent",
            "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status.",
            "Wait for agents to finish. A completed status may include the agent's final message; timeout returns an empty status. Completion also sends a notification with the same status.",
        ),
        (
            "multi_agent_v1/close_agent",
            "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Completed agents remain open and count toward the concurrency limit until closed. Don't keep agents open for too long if they are not needed anymore.",
            "Close an agent and its open descendants. Returns its status before shutdown. Completed agents count against the concurrency limit until closed, so close unneeded agents.",
        ),
        (
            "multi_agent_v1/resume_agent",
            "Resume a previously closed agent by id so it can receive send_input and wait_agent calls.",
            "Resume a closed agent so it can receive send_input and wait_agent calls.",
        ),
        (
            "wait_agent",
            "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. The wait also ends early when new user input is steered into the active turn. Does not return the content; returns either a summary of which agents have updates (if any), an interruption summary for steered input, or a timeout summary if no activity arrives before the deadline.",
            "Wait for live-agent mailbox updates. Returns a summary, not update contents, and ends early if user input is steered in. Set a timeout to avoid indefinite waits.",
        ),
        (
            "followup_task",
            "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes.",
            "Send a follow-up to an existing non-root agent. It starts a new turn if idle; a running agent receives it at a sampling boundary or after its current tool call.",
        ),
        (
            "view_image",
            "View a local image file from the filesystem when visual inspection is needed. Use this tool when images are available on the local filesystem.",
            "View a local image file already on disk when visual inspection is needed.",
        ),
        (
            "list_agents",
            "List live agents in the current root thread tree. Optionally filter by task-path prefix.",
            "List live agents, optionally filtered by task-path prefix.",
        ),
    ];
    let reduction_report: Vec<_> = reductions
        .iter()
        .map(|(name, original, concise)| {
            json!({
                "tool": name,
                "original_description_bytes": original.len(),
                "concise_description_bytes": concise.len(),
                "reduced_description_bytes": original.len() - concise.len(),
            })
        })
        .collect();
    let description_reduction_bytes: usize = reductions
        .iter()
        .map(|(_, original, concise)| original.len() - concise.len())
        .sum();
    println!(
        "CONCISE_TOOL_BYTE_REPORT={}",
        json!({
            "serializer": "create_tools_json_for_responses_api, compact serde_json bytes",
            "tool_count": 10,
            "before_json_bytes": before.len(),
            "after_json_bytes": after.len(),
            "reduced_json_bytes": before.len() - after.len(),
            "sum_description_byte_reductions": description_reduction_bytes,
            "json_encoding_additional_savings_bytes": before.len() - after.len() - description_reduction_bytes,
            "token_reduction_measured": false,
            "descriptions": reduction_report,
            "before_json": before_value,
            "after_json": after_value,
        })
    );
}

#[test]
fn concise_tool_descriptions_are_opt_in_and_exactly_scoped() {
    let specs = concise_description_fixture();
    let default_output = create_tools_json_for_responses_api_with_description_policy(&specs, false)
        .expect("serialize original descriptions");
    let mut expected = serde_json::to_value(&specs)
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    expected[0]["description"] = json!(UPDATE_PLAN_CONCISE);
    expected[1]["tools"][0]["description"] = json!(SLEEP_CONCISE);

    let concise_output = create_tools_json_for_responses_api_with_description_policy(&specs, true)
        .expect("serialize concise descriptions");
    assert_eq!(
        default_output,
        serde_json::to_value(&specs)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    );
    assert_eq!(concise_output, expected);
    assert_eq!(
        specs,
        concise_description_fixture(),
        "source ToolSpec is not mutated"
    );
    assert_eq!(concise_output[2]["tools"][0]["description"], SLEEP_ORIGINAL);
    assert_eq!(
        concise_output[3]["description"],
        format!("{}Changed.", UPDATE_PLAN_ORIGINAL)
    );
    assert!(
        concise_output[4]["description"]
            .as_str()
            .unwrap()
            .contains(SLEEP_ORIGINAL)
    );
    assert!(
        concise_output[4]["format"]["definition"]
            .as_str()
            .unwrap()
            .contains("nested_tool_call")
    );
    assert_eq!(
        concise_output[5]["description"],
        "Available roles and currently selectable models: explorer, verifier, worker"
    );
    assert_eq!(
        concise_output[0]["parameters"],
        default_output[0]["parameters"]
    );
    assert_eq!(
        concise_output[1]["tools"][0]["parameters"],
        default_output[1]["tools"][0]["parameters"]
    );
}

#[test]
fn concise_tool_descriptions_preserve_lite_and_raw_wire_shapes() {
    let specs = concise_description_fixture();
    let mut expected_lite = create_tools_json_for_responses_lite(&specs).expect("baseline lite");
    expected_lite[0]["tools"][0]["description"] = json!(UPDATE_PLAN_CONCISE);
    let concise_exec_description =
        format!("Exec grammar with nested declaration: {}", SLEEP_ORIGINAL);
    expected_lite[0]["tools"][2]["description"] = json!(concise_exec_description);
    expected_lite[0]["tools"][1]["description"] =
        json!(format!("{}Changed.", UPDATE_PLAN_ORIGINAL));
    expected_lite[1]["tools"][0]["description"] = json!(SLEEP_CONCISE);

    let concise_lite = create_tools_json_for_responses_lite_with_description_policy(&specs, true)
        .expect("concise lite");
    assert_eq!(concise_lite, expected_lite);
    assert_eq!(concise_lite[0]["tools"].as_array().unwrap().len(), 4);
    assert_eq!(
        concise_lite[0]["tools"][1]["description"],
        format!("{}Changed.", UPDATE_PLAN_ORIGINAL)
    );

    let raw = create_tools_raw_json_for_responses_api_with_description_policy(&specs, true)
        .expect("concise raw");
    let raw_value: serde_json::Value = serde_json::from_str(raw.get()).expect("valid raw JSON");
    let api = create_tools_json_for_responses_api_with_description_policy(&specs, true)
        .expect("concise API JSON");
    let mut expected_raw = serde_json::to_value(&specs).unwrap();
    expected_raw[0]["description"] = json!(UPDATE_PLAN_CONCISE);
    expected_raw[1]["tools"][0]["description"] = json!(SLEEP_CONCISE);
    assert_eq!(
        raw_value[5]["description"],
        "Available roles and currently selectable models: explorer, verifier, worker"
    );
    assert_eq!(raw_value, expected_raw);
    assert_eq!(api[0]["parameters"], raw_value[0]["parameters"]);
}

#[test]
fn concise_tool_catalog_requires_exact_kind_namespace_name_and_description() {
    use crate::concise_tool_descriptions::ToolKind;
    use std::ffi::OsStr;

    assert!(concise_tool_descriptions::enabled_from(Some(OsStr::new(
        "1"
    ))));
    assert!(!concise_tool_descriptions::enabled_from(None));
    assert!(!concise_tool_descriptions::enabled_from(Some(OsStr::new(
        "true"
    ))));
    assert!(!concise_tool_descriptions::enabled_from(Some(OsStr::new(
        "0"
    ))));
    assert_eq!(
        concise_tool_descriptions::concise_description(
            ToolKind::Function,
            None,
            "update_plan",
            UPDATE_PLAN_ORIGINAL,
        ),
        Some(UPDATE_PLAN_CONCISE),
    );
    assert_eq!(
        concise_tool_descriptions::concise_description(
            ToolKind::Function,
            Some("mcp__clock"),
            "sleep",
            SLEEP_ORIGINAL,
        ),
        None,
    );
    assert_eq!(
        concise_tool_descriptions::concise_description(
            ToolKind::Custom,
            Some("clock"),
            "sleep",
            SLEEP_ORIGINAL,
        ),
        None,
    );
    assert_eq!(
        concise_tool_descriptions::concise_description(
            ToolKind::Function,
            Some("clock"),
            "sleep",
            "A changed version.",
        ),
        None,
    );
}

#[test]
fn namespace_tool_spec_serializes_expected_wire_shape() {
    assert_eq!(
        serde_json::to_value(ToolSpec::Namespace(ResponsesApiNamespace {
            name: "mcp__demo__".to_string(),
            description: "Demo tools".to_string(),
            tools: vec![
                ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                    name: "lookup_order".to_string(),
                    description: "Look up an order".to_string(),
                    strict: false,
                    defer_loading: None,
                    parameters: JsonSchema::object(
                        BTreeMap::from([(
                            "order_id".to_string(),
                            JsonSchema::string(/*description*/ None),
                        )]),
                        /*required*/ None,
                        /*additional_properties*/ None,
                    ),
                    output_schema: None,
                }),
                ResponsesApiNamespaceTool::Custom(FreeformTool {
                    name: "apply_patch".to_string(),
                    description: "Apply a patch".to_string(),
                    defer_loading: None,
                    format: FreeformToolFormat {
                        r#type: "grammar".to_string(),
                        syntax: "lark".to_string(),
                        definition: "start: \"patch\"".to_string(),
                    },
                }),
            ],
        }))
        .expect("serialize namespace tool"),
        json!({
            "type": "namespace",
            "name": "mcp__demo__",
            "description": "Demo tools",
            "tools": [
                {
                    "type": "function",
                    "name": "lookup_order",
                    "description": "Look up an order",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "order_id": { "type": "string" },
                        },
                    },
                },
                {
                    "type": "custom",
                    "name": "apply_patch",
                    "description": "Apply a patch",
                    "format": {
                        "type": "grammar",
                        "syntax": "lark",
                        "definition": "start: \"patch\"",
                    },
                },
            ],
        })
    );
}

#[test]
fn web_search_tool_spec_serializes_expected_wire_shape() {
    assert_eq!(
        serde_json::to_value(ToolSpec::WebSearch {
            external_web_access: Some(true),
            indexed_web_access: Some(true),
            filters: Some(ResponsesApiWebSearchFilters {
                allowed_domains: Some(vec!["example.com".to_string()]),
            }),
            user_location: Some(ResponsesApiWebSearchUserLocation {
                r#type: WebSearchUserLocationType::Approximate,
                country: Some("US".to_string()),
                region: Some("California".to_string()),
                city: Some("San Francisco".to_string()),
                timezone: Some("America/Los_Angeles".to_string()),
            }),
            search_context_size: Some(WebSearchContextSize::High),
            search_content_types: Some(vec!["text".to_string(), "image".to_string()]),
        })
        .expect("serialize web_search"),
        json!({
            "type": "web_search",
            "external_web_access": true,
            "indexed_web_access": true,
            "filters": {
                "allowed_domains": ["example.com"],
            },
            "user_location": {
                "type": "approximate",
                "country": "US",
                "region": "California",
                "city": "San Francisco",
                "timezone": "America/Los_Angeles",
            },
            "search_context_size": "high",
            "search_content_types": ["text", "image"],
        })
    );
}

#[test]
fn tool_search_tool_spec_serializes_expected_wire_shape() {
    assert_eq!(
        serde_json::to_value(ToolSpec::ToolSearch {
            execution: "sync".to_string(),
            description: "Search app tools".to_string(),
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "query".to_string(),
                    JsonSchema::string(Some("Tool search query".to_string()),),
                )]),
                Some(vec!["query".to_string()]),
                Some(AdditionalProperties::Boolean(false))
            ),
        })
        .expect("serialize tool_search"),
        json!({
            "type": "tool_search",
            "execution": "sync",
            "description": "Search app tools",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Tool search query",
                    }
                },
                "required": ["query"],
                "additionalProperties": false,
            },
        })
    );
}

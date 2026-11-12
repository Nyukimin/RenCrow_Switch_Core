//! Opt-in descriptions for a small set of stable builtin tools.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Function,
    #[allow(dead_code)]
    Custom,
}

struct DescriptionEntry {
    kind: ToolKind,
    namespace: Option<&'static str>,
    name: &'static str,
    original: &'static str,
    concise: &'static str,
}

const CATALOG: &[DescriptionEntry] = &[
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: None,
        name: "update_plan",
        original: "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n",
        concise: "Update the task plan with an optional explanation and step statuses. At most one step may be in progress.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: Some("clock"),
        name: "sleep",
        original: "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time.",
        concise: "Pause for the requested duration; new input ends the wait early. Returns the elapsed wall-clock time.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: Some("multi_agent_v1"),
        name: "send_input",
        original: "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task.",
        concise: "Send a message to an existing agent. Set interrupt=true to redirect immediately; otherwise it is queued. Reuse the agent when the task depends on its context.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: Some("multi_agent_v1"),
        name: "wait_agent",
        original: "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status.",
        concise: "Wait for agents to finish. A completed status may include the agent's final message; timeout returns an empty status. Completion also sends a notification with the same status.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: Some("multi_agent_v1"),
        name: "close_agent",
        original: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Completed agents remain open and count toward the concurrency limit until closed. Don't keep agents open for too long if they are not needed anymore.",
        concise: "Close an agent and its open descendants. Returns its status before shutdown. Completed agents count against the concurrency limit until closed, so close unneeded agents.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: Some("multi_agent_v1"),
        name: "resume_agent",
        original: "Resume a previously closed agent by id so it can receive send_input and wait_agent calls.",
        concise: "Resume a closed agent so it can receive send_input and wait_agent calls.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: None,
        name: "wait_agent",
        original: "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. The wait also ends early when new user input is steered into the active turn. Does not return the content; returns either a summary of which agents have updates (if any), an interruption summary for steered input, or a timeout summary if no activity arrives before the deadline.",
        concise: "Wait for live-agent mailbox updates. Returns a summary, not update contents, and ends early if user input is steered in. Set a timeout to avoid indefinite waits.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: None,
        name: "followup_task",
        original: "Send a follow-up task to an existing non-root target agent and trigger a turn if it is idle. If the target is already running, deliver the task promptly at message boundaries while sampling, or after the pending tool call completes.",
        concise: "Send a follow-up to an existing non-root agent. It starts a new turn if idle; a running agent receives it at a sampling boundary or after its current tool call.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: None,
        name: "view_image",
        original: "View a local image file from the filesystem when visual inspection is needed. Use this tool when images are available on the local filesystem.",
        concise: "View a local image file already on disk when visual inspection is needed.",
    },
    DescriptionEntry {
        kind: ToolKind::Function,
        namespace: None,
        name: "list_agents",
        original: "List live agents in the current root thread tree. Optionally filter by task-path prefix.",
        concise: "List live agents, optionally filtered by task-path prefix.",
    },
];

pub(crate) fn concise_description(
    kind: ToolKind,
    namespace: Option<&str>,
    name: &str,
    original: &str,
) -> Option<&'static str> {
    CATALOG
        .iter()
        .find(|entry| {
            entry.kind == kind
                && entry.namespace == namespace
                && entry.name == name
                && entry.original == original
                && entry.concise.len() < original.len()
        })
        .map(|entry| entry.concise)
}

pub(crate) fn enabled_from(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

pub(crate) fn is_enabled() -> bool {
    enabled_from(std::env::var_os("RENCROW_SHORT_TOOL_DESCRIPTIONS").as_deref())
}

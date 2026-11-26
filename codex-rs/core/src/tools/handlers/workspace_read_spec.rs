use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const WORKSPACE_READ_TOOL_NAME: &str = "workspace_read";

pub(crate) fn create_workspace_read_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".into(),
            JsonSchema::string_enum(
                vec![json!("read"), json!("search"), json!("compare")],
                Some("Choose a bounded local read operation; this tool does not accept commands.".into()),
            ),
        ),
        (
            "paths".into(),
            JsonSchema::array(
                JsonSchema::string(Some(
                    "Workspace-relative path already declared by the active task request.".into(),
                )),
                Some("One path for read/search or two for compare. At most 16 declared task paths are available.".into()),
            ),
        ),
        (
            "query".into(),
            JsonSchema::string(Some(
                "Optional search text for the search action (up to 256 UTF-8 bytes).".into(),
            )),
        ),
        (
            "case_sensitive".into(),
            JsonSchema::boolean(Some("Whether search matches case exactly. Defaults to false.".into())),
        ),
        (
            "max_results".into(),
            JsonSchema::number(Some("Maximum matching lines to return, from 1 through 50.".into())),
        ),
        (
            "max_output_bytes".into(),
            JsonSchema::number(Some("Maximum returned text bytes, from 1 through 32768.".into())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: WORKSPACE_READ_TOOL_NAME.into(),
        description: "Read, search, or compare only files declared by the active common-harness task. Paths are workspace-relative, symlinks are rejected, results are bounded, and no command or arbitrary path is accepted.".into(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".into(), "paths".into()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

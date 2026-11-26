// Modified by RenCrow Switch Core, 2026-11-26: bound delegated instructions before dispatch.
use crate::config::DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
use crate::config::HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Minimum wait timeout to prevent tight polling loops from burning CPU.
pub(crate) const MIN_WAIT_TIMEOUT_MS: i64 = DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
pub(crate) const MAX_WAIT_TIMEOUT_MS: i64 = HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS;
pub(crate) fn function_arguments(payload: ToolPayload) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string(),
        )),
    }
}

pub(crate) fn tool_output_json_text<T>(value: &T, tool_name: &str) -> String
where
    T: Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}")).to_string()
    })
}

pub(crate) fn tool_output_response_item<T>(
    call_id: &str,
    payload: &ToolPayload,
    value: &T,
    success: Option<bool>,
    tool_name: &str,
) -> ResponseInputItem
where
    T: Serialize,
{
    FunctionToolOutput::from_text(tool_output_json_text(value, tool_name), success)
        .to_response_item(call_id, payload)
}

pub(crate) fn tool_output_code_mode_result<T>(value: &T, tool_name: &str) -> JsonValue
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}"))
    })
}

pub(crate) fn collab_spawn_error(err: CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::UnsupportedOperation(message) if message == "thread manager dropped" => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        CodexErrorDetails::UnsupportedOperation(message) => {
            FunctionCallError::RespondToModel(message.clone())
        }
        _ => FunctionCallError::RespondToModel(format!("collab spawn failed: {err}")),
    }
}

pub(crate) fn collab_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErrorDetails::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("agent with id {agent_id} is closed"))
        }
        CodexErrorDetails::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        _ => FunctionCallError::RespondToModel(format!("collab tool failed: {err}")),
    }
}

/// Keeps V2 request validation distinct from a dropped local manager.
pub(crate) fn collab_v2_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err.details() {
        CodexErrorDetails::UnsupportedOperation(message) if message != "thread manager dropped" => {
            FunctionCallError::RespondToModel(message.clone())
        }
        _ => collab_agent_error(agent_id, err),
    }
}

pub(crate) fn thread_spawn_source(
    parent_thread_id: ThreadId,
    parent_session_source: &SessionSource,
    depth: i32,
    agent_role: Option<&str>,
    task_name: Option<String>,
) -> Result<SessionSource, FunctionCallError> {
    let agent_path = task_name
        .as_deref()
        .map(|task_name| {
            parent_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .join(task_name)
                .map_err(FunctionCallError::RespondToModel)
        })
        .transpose()?;
    Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path,
        agent_nickname: None,
        agent_role: agent_role.map(str::to_string),
    }))
}

pub(crate) fn parse_collab_input(
    message: Option<String>,
    items: Option<Vec<UserInput>>,
) -> Result<Vec<UserInput>, FunctionCallError> {
    match (message, items) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "Provide one of: message or items".to_string(),
        )),
        (Some(message), None) => {
            if message.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Empty message can't be sent to an agent".to_string(),
                ));
            }
            Ok(vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }])
        }
        (None, Some(items)) => {
            if items.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Items can't be empty".to_string(),
                ));
            }
            Ok(items)
        }
    }
}

/// Bound the complete inline instruction, including quoted code and all text items.
/// Uses the same UTF-8 byte estimate as Switch tool-output budgets, not a model tokenizer.
pub(crate) fn validate_collab_input_budget(input: &[UserInput]) -> Result<(), FunctionCallError> {
    let bytes = input.iter().fold(0usize, |bytes, item| {
        bytes.saturating_add(match item {
            UserInput::Text { text, .. } => text.len(),
            _ => 0,
        })
    });
    validate_collab_text_budget(bytes)
}

pub(crate) fn validate_collab_text_budget(bytes: usize) -> Result<(), FunctionCallError> {
    let tokens = approx_tokens_from_byte_count(bytes);
    if tokens > 8_000 {
        return Err(FunctionCallError::RespondToModel(format!(
            "Delegated instruction is approximately {tokens} tokens; the limit is 8000 including all inline text, quotes, and code. Nothing was dispatched. Shorten the packet while preserving the goal, permissions, constraints, and acceptance criteria; provide source paths for additional context. Do not split the same oversized instruction across messages."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod delegation_budget_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn text_input(text: String) -> UserInput {
        UserInput::Text {
            text,
            text_elements: Vec::new(),
        }
    }

    #[test]
    fn accepts_boundary_without_truncating_instruction() {
        let text = "a".repeat(32_000);
        let input = vec![text_input(text.clone())];
        validate_collab_input_budget(&input).unwrap();
        assert_eq!(input, vec![text_input(text)]);
    }

    #[test]
    fn rejects_oversize_and_allows_shortened_retry() {
        let error = validate_collab_input_budget(&[text_input("a".repeat(32_001))]).unwrap_err();
        assert!(error.to_string().contains("8000"));
        assert!(error.to_string().contains("Nothing was dispatched"));
        assert!(
            validate_collab_input_budget(&[text_input(
                "Inspect the named function; report findings.".into()
            )])
            .is_ok()
        );
    }

    #[test]
    fn aggregates_items_and_counts_utf8_bytes() {
        let items = vec![
            text_input("あ".repeat(10_000)),
            text_input("a".repeat(2_001)),
        ];
        assert!(validate_collab_input_budget(&items).is_err());
        assert!(validate_collab_text_budget(32_000).is_ok());
        assert!(validate_collab_text_budget(32_001).is_err());
    }
}

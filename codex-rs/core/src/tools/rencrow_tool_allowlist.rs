// Modified by RenCrow Switch Core, 2026-09-28: role tool allowlist (ROLE_TOOL_POLICY.md).
//! `rencrow_tool_allowlist` narrows the session `ToolPolicy`, the upstream mechanism that keeps a
//! tool out of the registry: an unlisted tool is neither shown to the model nor executed when the
//! model names it. A model that follows an instruction embedded in data it reads therefore cannot
//! use a tool its role does not need. Entries are `name` for the default namespace and
//! `namespace::name` otherwise, for example `multi_agent_v1::spawn_agent`.

use codex_extension_api::ToolPolicy;
use codex_tools::ToolName;

const NAMESPACE_SEPARATOR: &str = "::";

/// Parses one allowlist entry into the tool name the registry compares.
pub(crate) fn parse_entry(entry: &str) -> ToolName {
    match entry.split_once(NAMESPACE_SEPARATOR) {
        Some((namespace, name)) => ToolName::namespaced(namespace, name),
        None => ToolName::plain(entry),
    }
}

/// Narrows a session tool policy to the allowlist. A tool the existing policy already excludes
/// stays excluded, so the allowlist can never add a tool.
pub(crate) fn narrow_policy(policy: &ToolPolicy, allowlist: &[String]) -> ToolPolicy {
    let allowed_tools = allowlist
        .iter()
        .map(|entry| parse_entry(entry))
        .filter(|tool| policy.allows(tool))
        .collect();
    ToolPolicy {
        allowed_tools: Some(allowed_tools),
        ..policy.clone()
    }
}

/// A role narrows the allowlist it inherits; it can never add a tool.
pub(crate) fn narrow(
    inherited: Option<Vec<String>>,
    role: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (inherited, role) {
        (inherited, None) => inherited,
        (None, role) => role,
        (Some(inherited), Some(role)) => Some(
            role.into_iter()
                .filter(|entry| inherited.contains(entry))
                .collect(),
        ),
    }
}

#[cfg(test)]
#[path = "rencrow_tool_allowlist_tests.rs"]
mod tests;

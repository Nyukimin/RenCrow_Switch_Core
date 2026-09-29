use super::*;
use pretty_assertions::assert_eq;

fn allowlist(entries: &[&str]) -> Vec<String> {
    entries.iter().map(ToString::to_string).collect()
}

#[test]
fn parse_entry_reads_plain_and_namespaced_names() {
    assert_eq!(parse_entry("exec_command"), ToolName::plain("exec_command"));
    assert_eq!(
        parse_entry("multi_agent_v1::spawn_agent"),
        ToolName::namespaced("multi_agent_v1", "spawn_agent")
    );
    assert_eq!(
        parse_entry("mcp__rencrow_advisor::delegate_llm"),
        ToolName::namespaced("mcp__rencrow_advisor", "delegate_llm")
    );
}

#[test]
fn narrowed_policy_allows_only_listed_tools() {
    let policy = narrow_policy(
        &ToolPolicy::default(),
        &allowlist(&["exec_command", "multi_agent_v1::spawn_agent"]),
    );

    assert!(policy.allows(&ToolName::plain("exec_command")));
    assert!(policy.allows(&ToolName::namespaced(
        codex_protocol::DEFAULT_FUNCTION_NAMESPACE,
        "exec_command"
    )));
    assert!(policy.allows(&ToolName::namespaced("multi_agent_v1", "spawn_agent")));
    assert!(!policy.allows(&ToolName::namespaced("multi_agent_v1", "close_agent")));
    assert!(!policy.allows(&ToolName::plain("apply_patch")));
    assert!(!policy.allows(&ToolName::plain("web_search")));
    assert!(!policy.allows(&ToolName::namespaced(
        "mcp__rencrow_advisor",
        "delegate_llm"
    )));
    // A namespaced tool cannot be reached by naming it as a plain tool.
    assert!(!policy.allows(&ToolName::plain("spawn_agent")));
}

#[test]
fn narrowed_policy_keeps_tools_an_existing_policy_excludes_out() {
    let existing = ToolPolicy {
        allowed_tools: Some(vec![
            ToolName::plain("exec_command"),
            ToolName::plain("view_image"),
        ]),
        require_managed_sandbox: true,
        ..ToolPolicy::default()
    };

    let policy = narrow_policy(&existing, &allowlist(&["exec_command", "apply_patch"]));

    assert_eq!(
        policy.allowed_tools,
        Some(vec![ToolName::plain("exec_command")])
    );
    // Other restrictions of the existing policy are kept.
    assert!(policy.require_managed_sandbox);
}

#[test]
fn empty_allowlist_permits_no_tools() {
    let policy = narrow_policy(&ToolPolicy::default(), &[]);

    assert!(!policy.allows(&ToolName::plain("exec_command")));
}

#[test]
fn narrow_intersects_role_with_inherited_and_never_adds_tools() {
    let inherited = Some(allowlist(&[
        "exec_command",
        "apply_patch",
        "multi_agent_v1::spawn_agent",
    ]));

    assert_eq!(
        narrow(
            inherited.clone(),
            Some(allowlist(&["exec_command", "web_search"]))
        ),
        Some(allowlist(&["exec_command"]))
    );
    assert_eq!(narrow(inherited.clone(), None), inherited);
    assert_eq!(
        narrow(None, Some(allowlist(&["exec_command"]))),
        Some(allowlist(&["exec_command"]))
    );
    assert_eq!(narrow(None, None), None);
}

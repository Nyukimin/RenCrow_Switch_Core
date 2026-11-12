// Modified by RenCrow: opt-in, host-enforced executor progress limits.
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::num::NonZeroU16;

/// The work contract selected by a role or inherited from its parent.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RencrowTaskKind {
    Implement,
    Verify,
    ReadOnly,
    Orchestrate,
}

/// Native execution limits; role layers can tighten but never relax inherited limits.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RencrowExecutionGuard {
    pub max_read_only_calls: NonZeroU16,
    pub max_repeated_reads: NonZeroU16,
    pub task_kind: Option<RencrowTaskKind>,
    pub require_edit: bool,
    pub require_check: bool,
    pub require_child_results: bool,
    pub deny_nested_agent_cli: bool,
}

impl Default for RencrowExecutionGuard {
    fn default() -> Self {
        Self {
            max_read_only_calls: NonZeroU16::new(24).expect("nonzero limit"),
            max_repeated_reads: NonZeroU16::new(3).expect("nonzero limit"),
            task_kind: None,
            require_edit: false,
            require_check: false,
            require_child_results: false,
            deny_nested_agent_cli: false,
        }
    }
}

impl RencrowExecutionGuard {
    /// Whether this task contract requires a current-turn edit.
    pub fn requires_edit(&self) -> bool {
        match self.task_kind {
            Some(RencrowTaskKind::Implement) => true,
            Some(RencrowTaskKind::Verify | RencrowTaskKind::ReadOnly) => false,
            Some(RencrowTaskKind::Orchestrate) => self.require_edit,
            None => self.require_edit,
        }
    }

    /// Whether this task contract requires a passing check.
    pub fn requires_check(&self) -> bool {
        match self.task_kind {
            Some(RencrowTaskKind::Implement | RencrowTaskKind::Verify) => true,
            Some(RencrowTaskKind::Orchestrate) => self.require_check,
            Some(RencrowTaskKind::ReadOnly) => false,
            None => self.require_check,
        }
    }

    /// Whether this task contract requires collecting child results.
    pub fn requires_child_results(&self) -> bool {
        self.require_child_results || self.task_kind == Some(RencrowTaskKind::Orchestrate)
    }

    pub fn narrow(parent: Option<Self>, role: Option<Self>) -> Option<Self> {
        match (parent, role) {
            (Some(parent), Some(role)) => Some(Self {
                max_read_only_calls: parent.max_read_only_calls.min(role.max_read_only_calls),
                max_repeated_reads: parent.max_repeated_reads.min(role.max_repeated_reads),
                task_kind: role.task_kind.or(parent.task_kind),
                require_edit: parent.require_edit || role.require_edit,
                require_check: parent.require_check || role.require_check,
                require_child_results: parent.require_child_results || role.require_child_results,
                deny_nested_agent_cli: parent.deny_nested_agent_cli || role.deny_nested_agent_cli,
            }),
            (parent, role) => parent.or(role),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_typed_task_contracts_resolve_requirements() {
        let legacy = RencrowExecutionGuard {
            require_edit: true,
            require_check: true,
            ..Default::default()
        };
        assert!(legacy.requires_edit());
        assert!(legacy.requires_check());
        assert!(!legacy.requires_child_results());

        let parent = RencrowExecutionGuard {
            require_edit: true,
            require_check: true,
            require_child_results: true,
            ..Default::default()
        };
        let readonly = RencrowExecutionGuard::narrow(
            Some(parent),
            Some(RencrowExecutionGuard {
                task_kind: Some(RencrowTaskKind::ReadOnly),
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(!readonly.requires_edit());
        assert!(!readonly.requires_check());
        assert!(readonly.requires_child_results());

        let verify = RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Verify),
            require_edit: true,
            ..Default::default()
        };
        assert!(!verify.requires_edit());
        assert!(verify.requires_check());

        let verify_with_children = RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Verify),
            require_child_results: true,
            ..Default::default()
        };
        assert!(verify_with_children.requires_child_results());

        let implement_with_children = RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Implement),
            require_child_results: true,
            ..Default::default()
        };
        assert!(implement_with_children.requires_child_results());

        let implement = RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Implement),
            ..Default::default()
        };
        assert!(implement.requires_edit());
        assert!(implement.requires_check());

        let orchestrate = RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Orchestrate),
            ..Default::default()
        };
        assert!(!orchestrate.requires_edit());
        assert!(!orchestrate.requires_check());
        assert!(orchestrate.requires_child_results());
    }

    #[test]
    fn typed_task_kind_serializes_as_snake_case_and_is_narrowly_inherited() {
        let parsed: RencrowExecutionGuard = toml::from_str(
            r#"task_kind = "read_only"
require_edit = true"#,
        )
        .unwrap();
        assert_eq!(parsed.task_kind, Some(RencrowTaskKind::ReadOnly));

        let inherited = RencrowExecutionGuard::narrow(
            Some(RencrowExecutionGuard {
                task_kind: Some(RencrowTaskKind::Verify),
                ..Default::default()
            }),
            Some(RencrowExecutionGuard {
                task_kind: Some(RencrowTaskKind::Implement),
                ..Default::default()
            }),
        )
        .unwrap();
        assert_eq!(inherited.task_kind, Some(RencrowTaskKind::Implement));
    }

    #[test]
    fn task_kind_does_not_relax_limits_or_nested_cli_restriction() {
        let narrowed = RencrowExecutionGuard::narrow(
            Some(RencrowExecutionGuard {
                max_read_only_calls: NonZeroU16::new(8).unwrap(),
                max_repeated_reads: NonZeroU16::new(2).unwrap(),
                deny_nested_agent_cli: true,
                ..Default::default()
            }),
            Some(RencrowExecutionGuard {
                max_read_only_calls: NonZeroU16::new(12).unwrap(),
                max_repeated_reads: NonZeroU16::new(1).unwrap(),
                task_kind: Some(RencrowTaskKind::ReadOnly),
                ..Default::default()
            }),
        )
        .unwrap();

        assert_eq!(narrowed.max_read_only_calls.get(), 8);
        assert_eq!(narrowed.max_repeated_reads.get(), 1);
        assert!(narrowed.deny_nested_agent_cli);
    }
}

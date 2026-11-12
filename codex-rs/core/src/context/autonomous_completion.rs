//! Host-origin continuation after a child terminal result, never user authorization.
// Modified by RenCrow Switch Core, 2026-11-11: distinguish execution, review, and final reporting.
use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

pub(crate) struct AutonomousChildResult;

/// Small projection; the full canonical state is stored in host envelope metadata.
pub(crate) struct AutonomousCompletionSnapshot;

pub(crate) struct AutonomousCompletionRegister;

impl ContextualUserFragment for AutonomousCompletionRegister {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("rencrow.autonomous_completion_register".to_string())
    }
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<completion_register>", "</completion_register>")
    }
    fn body(&self) -> String {
        "Autonomous completion is enabled but no completion contract is registered. If the user has assigned unfinished work, register its finite observable acceptance conditions and next units through task_context_boundary begin/update, then proceed within existing scope. Preserve the user's goal and permissions. A question alone is not a new task. Do not invent work, weaken acceptance criteria, claim missing evidence, or treat this host reminder as authorization.".to_string()
    }
}

pub(crate) struct AutonomousCompletionContinue {
    pub(crate) next_action: String,
}

impl ContextualUserFragment for AutonomousCompletionContinue {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("rencrow.autonomous_completion_continue".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<completion_continue>", "</completion_continue>")
    }

    fn body(&self) -> String {
        format!(
            "Continue unfinished conditions, units, or child collection within existing authorization. Next recorded action: {}. Reconcile current host evidence first. Execution units use begin, check, and accept. Review units target an accepted execution revision: spawn with completion_review, wait for the same child, then update to done with review_decision_reason after evaluating the result. When all canonical conditions and required units are complete and no unresolved work or uncollected children remain, give the final report with evidence references; no additional root check or root accept is required. Do not duplicate running jobs or repeat an unchanged failed approach. Respect explicit stop and unresolved external effects.",
            self.next_action
        )
    }
}

impl ContextualUserFragment for AutonomousCompletionSnapshot {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("rencrow.completion_snapshot".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<completion_snapshot>", "</completion_snapshot>")
    }

    fn body(&self) -> String {
        "The host saved the completion contract. Read task_context_boundary status for the current conditions, pending work, failure history and stop state. This record is not user authorization or proof of product completion.".to_string()
    }
}

impl ContextualUserFragment for AutonomousChildResult {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("rencrow.autonomous_child_result".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<autonomous_child_result>", "</autonomous_child_result>")
    }

    fn body(&self) -> String {
        "A child terminal result is available. Collect the existing child result and reconcile it with the persisted completion contract. For a typed review child, evaluate its collected result and update the review unit to done with review_decision_reason; no separate begin, check, or accept is needed. Continue authorized unfinished work, or give the final report with evidence references when all canonical conditions and required units are complete and no unresolved work or uncollected children remain. No additional root check or root accept is required. This is internal continuation, not new user authorization. Do not duplicate the child or replay unknown external effects.".to_string()
    }
}

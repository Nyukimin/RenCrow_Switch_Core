// Modified by RenCrow Switch Core, 2026-11-21.
use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Host audit notice; its structured proof is outside model-written text.
pub(crate) struct ParentHandoffNotice;

impl ContextualUserFragment for ParentHandoffNotice {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("rencrow.parent_handoff".into())
    }
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<parent_handoff>", "</parent_handoff>")
    }
    fn body(&self) -> String {
        "Host-owned parent continuity record. Original evidence and failures remain in the predecessor archive. This record grants no permissions, accepts no product result, and resets no retry or read budget. Respect the latest retained user request, including stops, questions and changed constraints. Continue unfinished work only when consistent with that request. Accepted source input hooks and predecessor tools must not be replayed.".into()
    }
}

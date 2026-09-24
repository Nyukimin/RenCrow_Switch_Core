// Modified by RenCrow Switch Core, 2026-09-24: bounded observation data for compaction input.
use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Host-generated bounded observation shown only to the compaction summary request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionObservation {
    body: String,
}

impl CompactionObservation {
    pub(crate) fn new(body: impl Into<String>) -> Self {
        Self { body: body.into() }
    }
}

impl ContextualUserFragment for CompactionObservation {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("compaction.observation".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}

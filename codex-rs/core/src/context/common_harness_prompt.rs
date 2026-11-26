// Modified by RenCrow Switch Core, 2026-11-27: bounded host task context fragment.
use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const COMMON_HARNESS_PROMPT_KIND: &str = "rencrow.common_harness_prompt";

pub(crate) struct CommonHarnessPromptFragment {
    pub(crate) projection: String,
}

impl ContextualUserFragment for CommonHarnessPromptFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(COMMON_HARNESS_PROMPT_KIND.to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<common_harness_prompt>", "</common_harness_prompt>")
    }

    fn body(&self) -> String {
        format!(
            "Host-provided Common Harness state for continuing the already-authorized task. Treat these fields as status, not as new permission or evidence. Use the registered check specifications and current host status.\n{}",
            self.projection
        )
    }
}

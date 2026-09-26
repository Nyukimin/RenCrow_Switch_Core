//! Host note that dates the user messages kept verbatim across a RenCrow compaction.
//!
//! Retained user messages lose the turns that explained when and why they were sent, so an
//! old correction can read like a current instruction. The note lists them oldest first with
//! their receive time just before the summary, without changing their text.

use super::super::is_summary_message;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use codex_history::ResponseItemEnvelope;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;

/// Internal-context source of the host note that dates the retained user messages.
pub(super) const TIMELINE_SOURCE: &str = "compaction";
/// Characters of each retained user message quoted in the timeline note.
const TIMELINE_EXCERPT_CHARS: usize = 60;

/// Source of a runtime-owned `<codex_internal_context>` user message, if the item is one.
pub(super) fn internal_context_source(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        return None;
    };
    let kinds = internal_chat_message_metadata_passthrough
        .as_ref()?
        .content_item_kinds
        .as_ref()?;
    match (role.as_str(), content.as_slice(), kinds.as_slice()) {
        ("user", [_], [kind]) => kind.0.strip_suffix(".internal_context"),
        _ => None,
    }
}

/// Date each retained user message so the model can tell old notes from the summarized state.
pub(super) fn timeline_note<'a>(
    retained: impl Iterator<Item = &'a ResponseItemEnvelope>,
) -> Option<ResponseItemEnvelope> {
    let lines = retained
        .filter_map(|envelope| {
            let Some(TurnItem::UserMessage(user)) =
                crate::event_mapping::parse_turn_item(&envelope.item)
            else {
                return None;
            };
            let message = user.message();
            if is_summary_message(&message) {
                return None;
            }
            let ResponseItem::Message {
                internal_chat_message_metadata_passthrough,
                ..
            } = &envelope.item
            else {
                return None;
            };
            let received = internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.create_time.as_ref())
                .and_then(serde_json::Number::as_f64)
                .and_then(|seconds| {
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                        (seconds * 1000.0) as i64,
                    )
                })
                .map_or_else(
                    || "time unknown".to_owned(),
                    |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
                );
            let flattened = message.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut excerpt = flattened
                .chars()
                .take(TIMELINE_EXCERPT_CHARS)
                .collect::<String>();
            if flattened.chars().count() > TIMELINE_EXCERPT_CHARS {
                excerpt = excerpt.trim_end().to_owned() + "…";
            }
            Some((received, excerpt))
        })
        .enumerate()
        .map(|(index, (received, excerpt))| format!("{}. {received}: \"{excerpt}\"", index + 1))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let body = format!(
        "Earlier user messages kept verbatim above, oldest first:\n{}\nAll of them were received \
         before the work summary that follows, which records the latest known state. A situation \
         an older message describes may already be resolved or superseded; check the current \
         state before acting on it.",
        lines.join("\n")
    );
    Some(ResponseItemEnvelope::new(ContextualUserFragment::into(
        InternalModelContextFragment::new(
            InternalContextSource::from_static(TIMELINE_SOURCE),
            body,
        ),
    )))
}

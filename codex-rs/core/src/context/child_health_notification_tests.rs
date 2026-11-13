use super::*;
use crate::agent::child_monitor::ChildMonitorPhase;
use crate::agent::child_monitor::health::ChildHealthNoticeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

#[test]
fn child_health_is_bounded_host_context_without_user_authorization() {
    let fragment = ChildHealthNotification::new(
        ThreadId::new(),
        ChildHealthNotice {
            kind: ChildHealthNoticeKind::ObservationUnavailable,
            turn_id: Some("\"".repeat(10_000)),
            phase: ChildMonitorPhase::Unknown,
            reason_code: "\"".repeat(10_000),
            observed_at_ms: 1,
            age_ms: None,
            progress_sequence: 0,
        },
    );
    let rendered = fragment.render();
    assert!(rendered.len() < 4096);
    assert!(crate::context::is_contextual_user_fragment(
        &ContentItem::InputText { text: rendered }
    ));
    let item: ResponseItem = ContextualUserFragment::into(fragment);
    assert!(!crate::context::is_user_authorization_message(&item));
}

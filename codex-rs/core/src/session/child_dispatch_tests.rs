use super::ChildDispatchGuardResult;
use crate::agent::child_dispatch::ChildDispatchKind;
use crate::agent::child_dispatch::fingerprint;
use codex_features::Feature;
use codex_protocol::ThreadId;

#[tokio::test]
async fn reservation_is_turn_scoped_and_same_turn_reentry_does_not_clear_it() {
    let (mut session, _) = crate::session::tests::make_session_and_context().await;
    let key = fingerprint(
        ThreadId::new(),
        ChildDispatchKind::V1SendInput { interrupt: false },
        &"same instructions",
    )
    .expect("serializable dispatch payload");
    for _ in 0..2 {
        assert_eq!(
            session.reserve_child_dispatch("turn-1", key.clone()).await,
            ChildDispatchGuardResult::Disabled
        );
    }
    session
        .features
        .set_enabled(Feature::RencrowChildMonitor, true)
        .expect("enable child monitor feature");
    session.record_started_turn("turn-1").await;

    assert_eq!(
        session.reserve_child_dispatch("turn-1", key.clone()).await,
        ChildDispatchGuardResult::Reserved
    );
    assert_eq!(
        session.reserve_child_dispatch("turn-1", key.clone()).await,
        ChildDispatchGuardResult::Duplicate
    );

    session.record_started_turn("turn-1").await;
    assert_eq!(
        session.reserve_child_dispatch("turn-1", key.clone()).await,
        ChildDispatchGuardResult::Duplicate
    );
    assert_eq!(
        session
            .reserve_child_dispatch("old-turn", key.clone())
            .await,
        ChildDispatchGuardResult::StaleTurn
    );

    session.record_started_turn("turn-2").await;
    assert_eq!(
        session.reserve_child_dispatch("turn-1", key.clone()).await,
        ChildDispatchGuardResult::StaleTurn
    );
    assert_eq!(
        session.reserve_child_dispatch("turn-2", key).await,
        ChildDispatchGuardResult::Reserved
    );
}

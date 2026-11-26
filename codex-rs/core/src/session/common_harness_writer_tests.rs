// Modified by RenCrow Switch Core, 2026-11-26: cover current local writer proof.

use codex_protocol::ThreadId;
use std::sync::Arc;

use super::verify_local_writer;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context;

#[tokio::test]
async fn proves_current_local_writer_and_canonical_paths() {
    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let thread_id = session.thread_id;
    let codex_home = session.get_config().await.codex_home.to_path_buf();

    let proof = verify_local_writer(&session)
        .await
        .expect("the attached local writer should be provable");

    assert_eq!(proof.thread_id(), thread_id);
    assert_eq!(proof.rollout_path(), rollout_path.canonicalize().unwrap());
    assert_eq!(proof.codex_home(), codex_home.canonicalize().unwrap());
}

#[tokio::test]
async fn rejects_missing_live_thread() {
    let (mut session, _) = make_session_and_context().await;
    session.services.live_thread = None;

    assert!(verify_local_writer(&session).await.is_err());
}

#[tokio::test]
async fn rejects_non_local_thread_store() {
    let (mut session, _) = make_session_and_context().await;
    attach_thread_persistence(&mut session).await;
    session.services.thread_store = Arc::new(codex_thread_store::InMemoryThreadStore::default());

    assert!(verify_local_writer(&session).await.is_err());
}

#[tokio::test]
async fn rejects_session_thread_id_that_does_not_own_live_writer() {
    let (mut session, _) = make_session_and_context().await;
    attach_thread_persistence(&mut session).await;
    session.thread_id = ThreadId::new();

    assert!(verify_local_writer(&session).await.is_err());
}

#[tokio::test]
async fn rejects_live_rollout_path_for_another_thread() {
    let (mut session, _) = make_session_and_context().await;
    let original_thread_id = session.thread_id;
    let original_path = attach_thread_persistence(&mut session).await;

    session.thread_id = ThreadId::new();
    let other_thread_path = attach_thread_persistence(&mut session).await;
    session.thread_id = original_thread_id;
    assert_ne!(
        original_path.canonicalize().unwrap(),
        other_thread_path.canonicalize().unwrap()
    );

    assert!(verify_local_writer(&session).await.is_err());
}

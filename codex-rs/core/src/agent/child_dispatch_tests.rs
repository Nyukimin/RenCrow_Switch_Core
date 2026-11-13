use super::ChildDispatchBook;
use super::ChildDispatchKind;
use super::ChildDispatchReservation;
use super::MAX_CHILD_DISPATCHES_PER_TURN;
use super::fingerprint;
use codex_protocol::ThreadId;

#[test]
fn fingerprint_deduplicates_same_action_but_distinguishes_target_and_mode() {
    let target = ThreadId::new();
    let same = fingerprint(target, ChildDispatchKind::V2FollowupTask, &"review plan")
        .expect("serializable dispatch payload");
    let duplicate = fingerprint(target, ChildDispatchKind::V2FollowupTask, &"review plan")
        .expect("serializable dispatch payload");
    let changed = fingerprint(target, ChildDispatchKind::V2FollowupTask, &"review code")
        .expect("serializable dispatch payload");
    let other_target = fingerprint(
        ThreadId::new(),
        ChildDispatchKind::V2FollowupTask,
        &"review plan",
    )
    .expect("serializable dispatch payload");
    let other_mode = fingerprint(
        target,
        ChildDispatchKind::V1SendInput { interrupt: false },
        &"review plan",
    )
    .expect("serializable dispatch payload");

    assert_eq!(same, duplicate);
    assert_ne!(same, changed);
    assert_ne!(same, other_target);
    assert_ne!(same, other_mode);
}

#[test]
fn book_keeps_reservations_and_fails_closed_at_its_cap() {
    let target = ThreadId::new();
    let mut book = ChildDispatchBook::default();
    let mut first_key = None;
    for index in 0..MAX_CHILD_DISPATCHES_PER_TURN {
        let key = fingerprint(
            target,
            ChildDispatchKind::V2FollowupTask,
            &format!("task {index}"),
        )
        .expect("serializable dispatch payload");
        if index == 0 {
            first_key = Some(key.clone());
        }
        assert_eq!(book.reserve(key), ChildDispatchReservation::Reserved);
    }
    assert_eq!(
        book.reserve(first_key.expect("first reservation")),
        ChildDispatchReservation::Duplicate
    );
    let overflow = fingerprint(target, ChildDispatchKind::V2FollowupTask, &"one more task")
        .expect("serializable dispatch payload");
    assert_eq!(
        book.reserve(overflow),
        ChildDispatchReservation::CapacityReached
    );
}

#[test]
fn clear_starts_a_fresh_reservation_generation() {
    let key = fingerprint(
        ThreadId::new(),
        ChildDispatchKind::V1SendInput { interrupt: false },
        &"same input",
    )
    .expect("serializable dispatch payload");
    let mut book = ChildDispatchBook::default();
    assert_eq!(
        book.reserve(key.clone()),
        ChildDispatchReservation::Reserved
    );
    book.clear();
    assert_eq!(book.reserve(key), ChildDispatchReservation::Reserved);
}

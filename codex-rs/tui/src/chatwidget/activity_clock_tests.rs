use super::*;

#[test]
fn activity_label_uses_fixed_jst_and_calendar_date() {
    let timestamp = chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
        .unwrap()
        .and_hms_opt(4, 21, 3)
        .unwrap()
        .and_utc()
        .timestamp();
    assert_eq!(
        activity_label(timestamp, timestamp + 65),
        "13:21 JST · 1m05s · 11-08 13:21:03 JST"
    );
}

#[test]
fn activity_label_marks_clock_reversal_as_unknown() {
    let timestamp = 1_800_000_000;
    assert!(activity_label(timestamp, timestamp - 1).contains("elapsed unknown"));
}

#[test]
fn replayed_activity_keeps_the_newest_recorded_timestamp_when_pages_arrive_out_of_order() {
    let newest = 1_800_000_100;
    assert_eq!(replay_activity_epoch(None, newest), Some(newest));
    assert_eq!(
        replay_activity_epoch(Some(newest), newest - 50),
        Some(newest)
    );
    assert_eq!(
        replay_activity_epoch(Some(newest - 50), newest),
        Some(newest)
    );
}

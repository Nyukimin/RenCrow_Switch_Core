//! Formatting for the optional RenCrow activity footer.

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::Utc;

pub(super) fn activity_label(activity_epoch_seconds: i64, now_epoch_seconds: i64) -> String {
    let jst = FixedOffset::east_opt(9 * 60 * 60).expect("JST offset is valid");
    let activity = DateTime::from_timestamp(activity_epoch_seconds, 0)
        .map(|timestamp| timestamp.with_timezone(&jst));
    let Some(activity) = activity else {
        return "Last activity: time unknown".to_string();
    };
    let Some(elapsed) = now_epoch_seconds.checked_sub(activity_epoch_seconds) else {
        return format!(
            "{} JST · elapsed unknown · {} JST",
            activity.format("%H:%M"),
            activity.format("%m-%d %H:%M:%S")
        );
    };
    if elapsed < 0 {
        return format!(
            "{} JST · elapsed unknown · {} JST",
            activity.format("%H:%M"),
            activity.format("%m-%d %H:%M:%S")
        );
    }
    let hours = elapsed / 3_600;
    let minutes = (elapsed % 3_600) / 60;
    let seconds = elapsed % 60;
    let duration = if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("0m{seconds:02}s")
    };
    format!(
        "{} JST · {duration} · {} JST",
        activity.format("%H:%M"),
        activity.format("%m-%d %H:%M:%S")
    )
}

pub(super) fn history_timestamp_label(epoch_seconds: i64) -> Option<String> {
    let jst = FixedOffset::east_opt(9 * 60 * 60).expect("JST offset is valid");
    DateTime::from_timestamp(epoch_seconds, 0).map(|timestamp| {
        timestamp
            .with_timezone(&jst)
            .format("[%H:%M:%S JST]")
            .to_string()
    })
}

pub(super) fn now_epoch_seconds() -> i64 {
    Utc::now().timestamp()
}

pub(super) fn replay_activity_epoch(
    current_epoch_seconds: Option<i64>,
    replayed_epoch_seconds: i64,
) -> Option<i64> {
    Some(
        current_epoch_seconds.map_or(replayed_epoch_seconds, |current| {
            current.max(replayed_epoch_seconds)
        }),
    )
}

impl super::ChatWidget {
    pub(crate) fn activity_timestamps_enabled(&self) -> bool {
        self.local_settings.tui.rencrow_tui_jst_timestamps
    }

    pub(crate) fn has_recorded_activity(&self) -> bool {
        self.last_activity_epoch_seconds.is_some()
    }

    pub(super) fn record_activity_epoch(&mut self, epoch_seconds: i64) {
        self.last_activity_epoch_seconds = Some(epoch_seconds);
        self.refresh_activity_footer();
    }

    pub(super) fn record_replay_activity_epoch(&mut self, epoch_seconds: i64) {
        self.last_activity_epoch_seconds =
            replay_activity_epoch(self.last_activity_epoch_seconds, epoch_seconds);
        self.refresh_activity_footer();
    }

    pub(super) fn refresh_activity_footer(&mut self) {
        let label = self.local_settings.tui.rencrow_tui_jst_timestamps.then(|| {
            self.last_activity_epoch_seconds.map_or_else(
                || "Last activity: time unknown".to_string(),
                |activity| activity_label(activity, now_epoch_seconds()),
            )
        });
        self.bottom_pane.set_activity_context(label);
    }
}

#[cfg(test)]
#[path = "activity_clock_tests.rs"]
mod tests;

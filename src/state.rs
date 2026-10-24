//! Persistent bot state and the pure decision rules built on it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::message::PostKind;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Kyiv date of the last successful schedule post.
    pub posted_date: Option<NaiveDate>,
    /// 48-char slot code of the last post: `1` on, `0` off, `m` maybe.
    pub posted_slots: Option<String>,
    pub posted_image_etag: Option<String>,
    pub consecutive_failures: u32,
    pub failure_alert_sent: bool,
    /// Unix seconds when we started waiting for a fresh image.
    pub pending_change_since: Option<i64>,
}

impl State {
    /// Loads the state; a missing or corrupt file yields an empty state.
    pub fn load(path: &Path) -> State {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(state) => state,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "state file is corrupt, starting with empty state");
                    State::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                info!(path = %path.display(), "no state file, starting with empty state");
                State::default()
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "cannot read state file, starting with empty state");
                State::default()
            }
        }
    }

    /// Writes the state atomically: temp file, fsync, rename.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = tmp_path(path);
        let mut file = std::fs::File::create(&tmp)?;
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    }

    /// What to post for today's slots, if anything.
    pub fn decide(&self, today: NaiveDate, slots: &str) -> Option<PostKind> {
        if self.posted_date != Some(today) {
            Some(PostKind::Daily)
        } else if self.posted_slots.as_deref() != Some(slots) {
            Some(PostKind::Updated)
        } else {
            None
        }
    }

    /// Whether the downloaded image may be posted now. An image identical to the
    /// last posted one is assumed stale (the CDN caches JSON and PNG separately)
    /// until `wait_max_secs` have passed since we started waiting.
    pub fn image_ready(&self, image_etag: Option<&str>, now: i64, wait_max_secs: u64) -> bool {
        match (self.posted_image_etag.as_deref(), image_etag) {
            (Some(posted), Some(current)) if posted == current => {
                let since = self.pending_change_since.unwrap_or(now);
                now - since >= i64::try_from(wait_max_secs).unwrap_or(i64::MAX)
            }
            _ => true,
        }
    }

    /// Counts a failed check; returns true when the failure alert is due.
    /// The flag is set by the caller only after the alert is actually delivered.
    pub fn record_failure(&mut self, threshold: u32) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures >= threshold && !self.failure_alert_sent
    }

    /// Counts a successful check; returns true when the recovery message is due.
    pub fn record_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.failure_alert_sent
    }

    pub fn mark_posted(&mut self, date: NaiveDate, slots: String, image_etag: Option<String>) {
        self.posted_date = Some(date);
        self.posted_slots = Some(slots);
        self.posted_image_etag = image_etag;
        self.pending_change_since = None;
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn posted(day: u32, slots: &str) -> State {
        State {
            posted_date: Some(date(day)),
            posted_slots: Some(slots.into()),
            posted_image_etag: Some("\"img1\"".into()),
            ..State::default()
        }
    }

    #[test]
    fn first_run_posts() {
        assert_eq!(
            State::default().decide(date(24), "1"),
            Some(PostKind::Daily)
        );
    }

    #[test]
    fn new_day_posts_even_with_same_slots() {
        assert_eq!(
            posted(23, "abc").decide(date(24), "abc"),
            Some(PostKind::Daily)
        );
    }

    #[test]
    fn change_during_day_posts_update() {
        assert_eq!(
            posted(24, "abc").decide(date(24), "abd"),
            Some(PostKind::Updated)
        );
    }

    #[test]
    fn unchanged_slots_do_not_post() {
        // Only fact.update/lastUpdated changed: slots are identical.
        assert_eq!(posted(24, "abc").decide(date(24), "abc"), None);
    }

    #[test]
    fn image_wait() {
        let mut s = posted(24, "abc");
        // A new image is posted right away.
        assert!(s.image_ready(Some("\"img2\""), 1000, 900));
        // No ETag to compare: post.
        assert!(s.image_ready(None, 1000, 900));
        // Same image as last time: wait...
        assert!(!s.image_ready(Some("\"img1\""), 1000, 900));
        s.pending_change_since = Some(1000);
        assert!(!s.image_ready(Some("\"img1\""), 1899, 900));
        // ...until the wait expires.
        assert!(s.image_ready(Some("\"img1\""), 1900, 900));
        // First run has nothing to compare against.
        assert!(State::default().image_ready(Some("\"img1\""), 0, 900));
    }

    #[test]
    fn failure_alert_once_and_recovery_once() {
        let mut s = State::default();
        assert!(!s.record_failure(3));
        assert!(!s.record_failure(3));
        assert!(s.record_failure(3));
        s.failure_alert_sent = true;
        assert!(!s.record_failure(3));
        assert!(!s.record_failure(3));
        assert_eq!(s.consecutive_failures, 5);

        assert!(s.record_success());
        s.failure_alert_sent = false;
        assert_eq!(s.consecutive_failures, 0);
        assert!(!s.record_success());
    }

    #[test]
    fn undelivered_alert_is_retried() {
        let mut s = State::default();
        s.record_failure(1);
        // Delivery failed: flag stays false, so the next failure asks again.
        assert!(s.record_failure(1));
    }

    #[test]
    fn success_before_threshold_sends_nothing() {
        let mut s = State::default();
        s.record_failure(3);
        s.record_failure(3);
        assert!(!s.record_success());
        assert!(!s.record_failure(3));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");
        let mut s = posted(24, "abc");
        s.consecutive_failures = 2;
        s.pending_change_since = Some(42);
        s.save(&path).unwrap();
        assert_eq!(State::load(&path), s);
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn corrupt_or_missing_file_gives_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(State::load(&path), State::default());
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(State::load(&path), State::default());
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let s: State =
            serde_json::from_str(r#"{"posted_date":"2026-10-24","json_etag":"x"}"#).unwrap();
        assert_eq!(s.posted_date, Some(date(24)));
        assert_eq!(s.consecutive_failures, 0);
    }
}

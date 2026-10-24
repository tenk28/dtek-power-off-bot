//! One check cycle: fetch, decide, post, alert, persist.

use std::fmt;

use chrono::{DateTime, Utc};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::error::CheckError;
use crate::message::{self, CaptionInput, PostKind};
use crate::schedule::{self, ExtractParams, Extracted, Today};
use crate::source::{Image, Source};
use crate::state::State;
use crate::telegram::Telegram;

#[derive(Debug)]
pub enum CheckOutcome {
    Unchanged,
    Posted(PostKind),
    WaitingForImage,
    WaitingForTodayData,
    /// Data was fine but the post could not be delivered; retried next cycle.
    SendFailed,
    Failed(CheckError),
}

impl fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckOutcome::Unchanged => f.write_str("unchanged"),
            CheckOutcome::Posted(PostKind::Daily) => f.write_str("posted (daily)"),
            CheckOutcome::Posted(PostKind::Updated) => f.write_str("posted (updated)"),
            CheckOutcome::WaitingForImage => f.write_str("waiting for image"),
            CheckOutcome::WaitingForTodayData => f.write_str("waiting for today's data"),
            CheckOutcome::SendFailed => f.write_str("post not delivered, will retry"),
            CheckOutcome::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

enum Plan {
    Nothing,
    WaitForToday,
    WaitForImage,
    Post {
        kind: PostKind,
        today: Today,
        image: Image,
    },
}

pub struct Bot {
    config: Config,
    source: Source,
    telegram: Telegram,
    state: State,
    /// Last state written to disk, to avoid rewriting an unchanged file every cycle.
    saved: State,
}

impl Bot {
    pub fn new(config: Config, source: Source, telegram: Telegram) -> Self {
        let state = State::load(&config.storage.state_path);
        Self {
            config,
            source,
            telegram,
            saved: state.clone(),
            state,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub async fn check(&mut self, now: DateTime<Utc>) -> CheckOutcome {
        let outcome = match self.plan(now).await {
            Ok(plan) => {
                if self.state.record_success() {
                    self.send_recovery().await;
                }
                self.execute(plan).await
            }
            Err(err) => {
                if self
                    .state
                    .record_failure(self.config.polling.failure_alert_threshold)
                {
                    self.send_failure_alert(&err).await;
                }
                CheckOutcome::Failed(err)
            }
        };
        self.persist();
        outcome
    }

    async fn plan(&mut self, now: DateTime<Utc>) -> Result<Plan, CheckError> {
        let body = self.source.fetch_json().await.map_err(CheckError::Json)?;
        let data = schedule::parse(&body)?;
        let queue_key = self.config.queue_key();
        let params = ExtractParams {
            queue_key: &queue_key,
            now,
            stale_after_secs: self.config.polling.stale_after_secs,
            missing_today_grace_secs: self.config.polling.missing_today_grace_secs,
        };
        let today = match schedule::extract(&data, &params)? {
            Extracted::Today(today) => today,
            Extracted::NotYetPublished { .. } => return Ok(Plan::WaitForToday),
        };

        let Some(kind) = self.state.decide(today.date, &today.schedule.code()) else {
            self.state.pending_change_since = None;
            return Ok(Plan::Nothing);
        };

        let image = self.source.fetch_image().await.map_err(CheckError::Image)?;
        let ts = now.timestamp();
        if !self.state.image_ready(
            image.etag.as_deref(),
            ts,
            self.config.polling.image_wait_max_secs,
        ) {
            self.state.pending_change_since.get_or_insert(ts);
            return Ok(Plan::WaitForImage);
        }
        if self.state.pending_change_since.is_some() && self.state.posted_image_etag == image.etag {
            warn!("image did not refresh in time, posting the current one");
        }
        Ok(Plan::Post { kind, today, image })
    }

    async fn execute(&mut self, plan: Plan) -> CheckOutcome {
        let (kind, today, image) = match plan {
            Plan::Nothing => return CheckOutcome::Unchanged,
            Plan::WaitForToday => return CheckOutcome::WaitingForTodayData,
            Plan::WaitForImage => return CheckOutcome::WaitingForImage,
            Plan::Post { kind, today, image } => (kind, today, image),
        };

        let region_title = self
            .config
            .schedule
            .region_title
            .clone()
            .or_else(|| today.region_affiliation.clone())
            .unwrap_or_else(|| self.config.schedule.region.clone());
        let caption = message::caption(&CaptionInput {
            kind,
            today: &today,
            queue: &self.config.schedule.queue,
            region_title: &region_title,
        });

        let sent = self
            .telegram
            .send_photo(
                &self.config.telegram.channel_id,
                &image.bytes,
                &caption,
                self.config.telegram.silent,
            )
            .await;
        match sent {
            Ok(()) => {
                self.state
                    .mark_posted(today.date, today.schedule.code(), image.etag);
                CheckOutcome::Posted(kind)
            }
            Err(e) => {
                error!(error = %e, "failed to post schedule to the channel");
                CheckOutcome::SendFailed
            }
        }
    }

    async fn send_failure_alert(&mut self, err: &CheckError) {
        let text = message::failure_alert(self.state.consecutive_failures, &err.reason_uk());
        match self
            .telegram
            .send_message(self.config.alert_chat(), &text, false)
            .await
        {
            Ok(()) => {
                info!(
                    failures = self.state.consecutive_failures,
                    "failure alert sent"
                );
                self.state.failure_alert_sent = true;
            }
            Err(e) => error!(error = %e, "failed to send failure alert"),
        }
    }

    async fn send_recovery(&mut self) {
        match self
            .telegram
            .send_message(self.config.alert_chat(), message::RECOVERY, false)
            .await
        {
            Ok(()) => {
                info!("recovery message sent");
                self.state.failure_alert_sent = false;
            }
            Err(e) => error!(error = %e, "failed to send recovery message"),
        }
    }

    fn persist(&mut self) {
        if self.state == self.saved {
            return;
        }
        match self.state.save(&self.config.storage.state_path) {
            Ok(()) => self.saved = self.state.clone(),
            Err(e) => error!(
                path = %self.config.storage.state_path.display(),
                error = %e,
                "failed to save state"
            ),
        }
    }
}

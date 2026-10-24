//! Minimal Telegram Bot API client: `sendMessage` and `sendPhoto`.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tracing::warn;

use crate::config::ChatId;

const API_BASE: &str = "https://api.telegram.org";
const MAX_RETRIES: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("Telegram API error {code}: {description}")]
    Api { code: u16, description: String },
    #[error("Telegram request failed: {0}")]
    Network(String),
}

impl TelegramError {
    fn from_reqwest(err: reqwest::Error) -> Self {
        // The URL contains the bot token: never include it.
        TelegramError::Network(err.without_url().to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<ResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct ResponseParameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

pub struct Telegram {
    client: reqwest::Client,
    api_base: String,
    token: String,
    backoff: Duration,
}

impl fmt::Debug for Telegram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telegram")
            .field("api_base", &self.api_base)
            .field("token", &"***")
            .finish()
    }
}

impl Telegram {
    pub fn new(token: String, timeout: Duration) -> anyhow::Result<Self> {
        Self::with_api_base(API_BASE.to_owned(), token, timeout)
    }

    pub fn with_api_base(
        api_base: String,
        token: String,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(crate::source::USER_AGENT)
            .timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            backoff: Duration::from_secs(1),
        })
    }

    /// Base pause for exponential backoff on 5xx/network errors (1, 2, 4 × base).
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    pub async fn send_message(
        &self,
        chat: &ChatId,
        html: &str,
        silent: bool,
    ) -> Result<(), TelegramError> {
        let body = json!({
            "chat_id": chat,
            "text": html,
            "parse_mode": "HTML",
            "disable_notification": silent,
            "link_preview_options": { "is_disabled": true },
        });
        self.call("sendMessage", || {
            self.client.post(self.url("sendMessage")).json(&body)
        })
        .await
    }

    pub async fn send_photo(
        &self,
        chat: &ChatId,
        photo: &[u8],
        caption: &str,
        silent: bool,
    ) -> Result<(), TelegramError> {
        self.call("sendPhoto", || {
            let part = Part::bytes(photo.to_vec())
                .file_name("schedule.png")
                .mime_str("image/png")
                .expect("static mime type is valid");
            let form = Form::new()
                .text("chat_id", chat.to_string())
                .text("caption", caption.to_owned())
                .text("parse_mode", "HTML")
                .text("disable_notification", silent.to_string())
                .part("photo", part);
            self.client.post(self.url("sendPhoto")).multipart(form)
        })
        .await
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.token)
    }

    async fn call(
        &self,
        method: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<(), TelegramError> {
        let mut retries = 0;
        loop {
            let (err, pause) = match build().send().await {
                Ok(response) => {
                    let status = response.status();
                    let parsed: Option<ApiResponse> = response.json().await.ok();
                    if status.is_success() && parsed.as_ref().is_some_and(|r| r.ok) {
                        return Ok(());
                    }
                    let description = parsed
                        .as_ref()
                        .and_then(|r| r.description.clone())
                        .unwrap_or_else(|| status.to_string());
                    let err = TelegramError::Api {
                        code: status.as_u16(),
                        description,
                    };
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        let after = parsed
                            .and_then(|r| r.parameters)
                            .and_then(|p| p.retry_after)
                            .map_or(self.backoff, Duration::from_secs);
                        (err, after.min(MAX_RETRY_AFTER))
                    } else if status.is_server_error() {
                        (err, self.backoff * 2u32.pow(retries))
                    } else {
                        return Err(err);
                    }
                }
                Err(e) => (
                    TelegramError::from_reqwest(e),
                    self.backoff * 2u32.pow(retries),
                ),
            };
            if retries >= MAX_RETRIES {
                return Err(err);
            }
            retries += 1;
            warn!(method, error = %err, retry_in = ?pause, "Telegram call failed, retrying");
            tokio::time::sleep(pause).await;
        }
    }
}

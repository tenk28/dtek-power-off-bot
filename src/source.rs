//! Fetching the schedule JSON and image from the mirror.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use tracing::{debug, warn};

use crate::error::FetchError;

pub const USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (Telegram bot; +https://github.com/Baskerville42/outage-data-ua)"
);

/// Pauses between quick in-check retries of a failed request.
pub const DEFAULT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

pub struct Image {
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
}

pub struct Source {
    client: reqwest::Client,
    json_url: String,
    image_url: String,
    retry_delays: Vec<Duration>,
    /// Last JSON body with its ETag. Kept in memory only, so after a restart the
    /// first request is unconditional and a 304 always has a body to fall back to.
    cached_json: Option<(String, Arc<str>)>,
}

impl Source {
    pub fn new(json_url: String, image_url: String, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            json_url,
            image_url,
            retry_delays: DEFAULT_RETRY_DELAYS.to_vec(),
            cached_json: None,
        })
    }

    pub fn with_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.retry_delays = delays;
        self
    }

    /// Returns the current JSON body; `304 Not Modified` yields the cached one.
    pub async fn fetch_json(&mut self) -> Result<Arc<str>, FetchError> {
        let cached_etag = self.cached_json.as_ref().map(|(etag, _)| etag.clone());
        let response = self
            .with_retries(|| {
                let mut req = self.client.get(&self.json_url);
                if let Some(etag) = &cached_etag {
                    req = req.header(IF_NONE_MATCH, etag);
                }
                req
            })
            .await?;

        if response.status() == StatusCode::NOT_MODIFIED {
            if let Some((_, body)) = &self.cached_json {
                debug!("schedule JSON not modified");
                return Ok(body.clone());
            }
            // We never send If-None-Match without a cached body; treat as protocol error.
            return Err(FetchError::Http(304));
        }

        let etag = etag_of(&response);
        let body: Arc<str> = response
            .text()
            .await
            .map_err(FetchError::from_reqwest)?
            .into();
        self.cached_json = etag.map(|etag| (etag, body.clone()));
        Ok(body)
    }

    pub async fn fetch_image(&self) -> Result<Image, FetchError> {
        let response = self
            .with_retries(|| self.client.get(&self.image_url))
            .await?;
        let etag = etag_of(&response);
        let bytes = response
            .bytes()
            .await
            .map_err(FetchError::from_reqwest)?
            .to_vec();
        if bytes.is_empty() {
            return Err(FetchError::Network("empty image body".to_owned()));
        }
        Ok(Image { bytes, etag })
    }

    /// Sends the request, retrying network errors and 5xx a few times.
    async fn with_retries(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, FetchError> {
        let mut delays = self.retry_delays.iter();
        loop {
            let err = match build().send().await {
                Ok(r) if r.status().is_success() || r.status() == StatusCode::NOT_MODIFIED => {
                    return Ok(r);
                }
                Ok(r) => FetchError::Http(r.status().as_u16()),
                Err(e) => FetchError::from_reqwest(e),
            };
            match delays.next() {
                Some(delay) if err.is_retryable() => {
                    warn!(error = %err, retry_in = ?delay, "request failed, retrying");
                    tokio::time::sleep(*delay).await;
                }
                _ => return Err(err),
            }
        }
    }
}

fn etag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

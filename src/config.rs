//! Configuration loaded from `config.toml`; the bot token comes from the environment.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};

pub const TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";
pub const CONFIG_PATH_ENV: &str = "CONFIG_PATH";
pub const DEFAULT_CONFIG_PATH: &str = "./config.toml";
/// Optional override of `storage.state_path`, handy for local runs outside Docker.
pub const STATE_PATH_ENV: &str = "STATE_PATH";

const DEFAULT_JSON_URL: &str =
    "https://raw.githubusercontent.com/Baskerville42/outage-data-ua/main/data/{region}.json";
const DEFAULT_IMAGE_URL: &str = "https://raw.githubusercontent.com/Baskerville42/outage-data-ua/main/images/{region}/gpv-{queue_dashed}.png";

/// Telegram chat identifier: a numeric id or a public `@username`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ChatId {
    Id(i64),
    Name(String),
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatId::Id(id) => write!(f, "{id}"),
            ChatId::Name(name) => f.write_str(name),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub source: SourceConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    pub channel_id: ChatId,
    #[serde(default)]
    pub alert_chat_id: Option<ChatId>,
    /// `disable_notification` for schedule posts.
    #[serde(default)]
    pub silent: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    pub region: String,
    pub queue: String,
    #[serde(default)]
    pub region_title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SourceConfig {
    pub json_url: String,
    pub image_url: String,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            json_url: DEFAULT_JSON_URL.to_owned(),
            image_url: DEFAULT_IMAGE_URL.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PollingConfig {
    pub check_interval_secs: u64,
    pub failure_alert_threshold: u32,
    pub stale_after_secs: u64,
    pub image_wait_max_secs: u64,
    pub http_timeout_secs: u64,
    /// After local midnight, a missing "today" entry is not a failure for this long.
    pub missing_today_grace_secs: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 300,
            failure_alert_threshold: 3,
            stale_after_secs: 3 * 3600,
            image_wait_max_secs: 900,
            http_timeout_secs: 15,
            missing_today_grace_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    pub state_path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            state_path: PathBuf::from("/data/state.json"),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        let mut config = Self::parse(&text)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        if let Some(state_path) = std::env::var_os(STATE_PATH_ENV).filter(|p| !p.is_empty()) {
            config.storage.state_path = PathBuf::from(state_path);
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            is_valid_queue(&self.schedule.queue),
            "schedule.queue must look like \"3.2\", got {:?}",
            self.schedule.queue
        );
        ensure!(
            !self.schedule.region.is_empty()
                && self
                    .schedule
                    .region
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "schedule.region must be a slug like \"kyiv-region\", got {:?}",
            self.schedule.region
        );
        validate_chat_id("telegram.channel_id", &self.telegram.channel_id)?;
        if let Some(alert) = &self.telegram.alert_chat_id {
            validate_chat_id("telegram.alert_chat_id", alert)?;
        }
        ensure!(
            self.source.json_url.contains("{region}"),
            "source.json_url must contain the {{region}} placeholder"
        );
        ensure!(
            self.source.image_url.contains("{queue_dashed}")
                || self.source.image_url.contains("{queue}"),
            "source.image_url must contain the {{queue_dashed}} or {{queue}} placeholder"
        );
        let p = &self.polling;
        ensure!(
            p.check_interval_secs >= 60,
            "polling.check_interval_secs must be >= 60"
        );
        ensure!(
            p.failure_alert_threshold >= 1,
            "polling.failure_alert_threshold must be >= 1"
        );
        ensure!(
            p.http_timeout_secs >= 1,
            "polling.http_timeout_secs must be >= 1"
        );
        ensure!(
            p.stale_after_secs >= 60,
            "polling.stale_after_secs must be >= 60"
        );
        Ok(())
    }

    pub fn alert_chat(&self) -> &ChatId {
        self.telegram
            .alert_chat_id
            .as_ref()
            .unwrap_or(&self.telegram.channel_id)
    }

    /// Queue key in the source JSON, e.g. `GPV3.2`.
    pub fn queue_key(&self) -> String {
        format!("GPV{}", self.schedule.queue)
    }

    pub fn json_url(&self) -> String {
        self.expand(&self.source.json_url)
    }

    pub fn image_url(&self) -> String {
        self.expand(&self.source.image_url)
    }

    fn expand(&self, template: &str) -> String {
        template
            .replace("{region}", &self.schedule.region)
            .replace("{queue_dashed}", &self.schedule.queue.replace('.', "-"))
            .replace("{queue}", &self.schedule.queue)
    }

    pub fn check_interval(&self) -> Duration {
        Duration::from_secs(self.polling.check_interval_secs)
    }

    pub fn http_timeout(&self) -> Duration {
        Duration::from_secs(self.polling.http_timeout_secs)
    }
}

fn is_valid_queue(queue: &str) -> bool {
    let b = queue.as_bytes();
    b.len() == 3 && b[0].is_ascii_digit() && b[1] == b'.' && b[2].is_ascii_digit()
}

fn validate_chat_id(field: &str, id: &ChatId) -> anyhow::Result<()> {
    match id {
        ChatId::Id(0) => bail!("{field} must not be 0"),
        ChatId::Id(_) => Ok(()),
        ChatId::Name(name) => {
            let ok = name.strip_prefix('@').is_some_and(|n| !n.is_empty())
                || name.parse::<i64>().is_ok_and(|n| n != 0);
            ensure!(
                ok,
                "{field} must be \"@username\" or a numeric id, got {name:?}"
            );
            Ok(())
        }
    }
}

/// Reads the bot token from the environment.
pub fn bot_token() -> anyhow::Result<String> {
    let token = std::env::var(TOKEN_ENV)
        .with_context(|| format!("environment variable {TOKEN_ENV} is not set"))?;
    let token = token.trim().to_owned();
    ensure!(
        token.contains(':'),
        "{TOKEN_ENV} does not look like a bot token (expected \"<id>:<secret>\")"
    );
    Ok(token)
}

/// Resolves the config path from `--config <path>`, `CONFIG_PATH`, or the default.
pub fn config_path_from_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<PathBuf> {
    const USAGE: &str = "usage: dtek-power-off-bot [--config <path>]";
    let mut args = args.into_iter().skip(1);
    let path = match args.next() {
        None => {
            return Ok(std::env::var_os(CONFIG_PATH_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)));
        }
        Some(arg) if arg == "--config" => args.next().context("--config requires a path")?,
        Some(arg) => match arg.strip_prefix("--config=") {
            Some(path) => path.to_owned(),
            None => bail!("unknown argument {arg:?}; {USAGE}"),
        },
    };
    if let Some(extra) = args.next() {
        bail!("unexpected argument {extra:?}; {USAGE}");
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[telegram]
channel_id = "@my_power_channel"

[schedule]
region = "kyiv-region"
queue = "3.2"
"#;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.polling.check_interval_secs, 300);
        assert_eq!(c.polling.failure_alert_threshold, 3);
        assert_eq!(c.alert_chat(), &ChatId::Name("@my_power_channel".into()));
        assert_eq!(c.queue_key(), "GPV3.2");
        assert_eq!(
            c.image_url(),
            "https://raw.githubusercontent.com/Baskerville42/outage-data-ua/main/images/kyiv-region/gpv-3-2.png"
        );
        assert_eq!(
            c.json_url(),
            "https://raw.githubusercontent.com/Baskerville42/outage-data-ua/main/data/kyiv-region.json"
        );
    }

    #[test]
    fn example_config_is_valid() {
        let text = include_str!("../config.example.toml");
        let c = Config::parse(text).unwrap();
        assert_eq!(c.alert_chat(), &ChatId::Id(123456789));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = MINIMAL.replace("queue = \"3.2\"", "queue = \"3.2\"\nqeueu = 1");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (from, to) in [
            ("queue = \"3.2\"", "queue = \"32\""),
            ("queue = \"3.2\"", "queue = \"3.22\""),
            ("region = \"kyiv-region\"", "region = \"../etc\""),
            ("channel_id = \"@my_power_channel\"", "channel_id = \"my\""),
        ] {
            assert!(Config::parse(&MINIMAL.replace(from, to)).is_err(), "{to}");
        }
        let short = format!("{MINIMAL}\n[polling]\ncheck_interval_secs = 10\n");
        assert!(Config::parse(&short).is_err());
        let zero = format!("{MINIMAL}\n[polling]\nfailure_alert_threshold = 0\n");
        assert!(Config::parse(&zero).is_err());
        let no_placeholder = format!("{MINIMAL}\n[source]\njson_url = \"https://x/y.json\"\n");
        assert!(Config::parse(&no_placeholder).is_err());
    }

    #[test]
    fn config_path_resolution() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            config_path_from_args(args(&["bot", "--config", "/c.toml"])).unwrap(),
            PathBuf::from("/c.toml")
        );
        assert_eq!(
            config_path_from_args(args(&["bot", "--config=/d.toml"])).unwrap(),
            PathBuf::from("/d.toml")
        );
        assert!(config_path_from_args(args(&["bot", "--config"])).is_err());
        assert!(config_path_from_args(args(&["bot", "--oops"])).is_err());
    }
}

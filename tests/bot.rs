//! End-to-end check cycles against a mocked mirror and a mocked Telegram API.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dtek_power_off_bot::bot::{Bot, CheckOutcome};
use dtek_power_off_bot::config::Config;
use dtek_power_off_bot::message::PostKind;
use dtek_power_off_bot::schedule;
use dtek_power_off_bot::source::Source;
use dtek_power_off_bot::telegram::Telegram;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const FIXTURE: &str = include_str!("fixtures/kyiv-region.json");
const TOKEN: &str = "123:secret";
const DAY_KEY: &str = "1792789200";

struct Env {
    mirror: MockServer,
    tg: MockServer,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        let env = Self {
            mirror: MockServer::start().await,
            tg: MockServer::start().await,
            dir: tempfile::tempdir().unwrap(),
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":{}}"#))
            .mount(&env.tg)
            .await;
        env
    }

    fn state_path(&self) -> std::path::PathBuf {
        self.dir.path().join("state.json")
    }

    fn bot(&self) -> Bot {
        let config = Config::parse(&format!(
            r#"
[telegram]
channel_id = "@channel"
alert_chat_id = 777

[schedule]
region = "kyiv-region"
queue = "3.2"

[source]
json_url = "{0}/data/{{region}}.json"
image_url = "{0}/images/{{region}}/gpv-{{queue_dashed}}.png"

[storage]
state_path = "{1}"
"#,
            self.mirror.uri(),
            self.state_path().display()
        ))
        .unwrap();
        let source = Source::new(config.json_url(), config.image_url(), config.http_timeout())
            .unwrap()
            .with_retry_delays(vec![]);
        let telegram = Telegram::with_api_base(self.tg.uri(), TOKEN.into(), Duration::from_secs(5))
            .unwrap()
            .with_backoff(Duration::ZERO);
        Bot::new(config, source, telegram)
    }

    async fn serve(&self, json: &str, image_etag: &str) {
        self.mirror.reset().await;
        Mock::given(path("/data/kyiv-region.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(json))
            .mount(&self.mirror)
            .await;
        Mock::given(path("/images/kyiv-region/gpv-3-2.png"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", image_etag)
                    .set_body_bytes(b"\x89PNG fake".to_vec()),
            )
            .mount(&self.mirror)
            .await;
    }

    async fn serve_error(&self, status: u16) {
        self.mirror.reset().await;
        Mock::given(path_regex(".*"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.mirror)
            .await;
    }

    async fn tg_requests(&self, method_name: &str) -> Vec<Request> {
        self.tg
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with(method_name))
            .collect()
    }
}

fn now() -> DateTime<Utc> {
    schedule::parse(FIXTURE).unwrap().last_updated + chrono::Duration::minutes(5)
}

/// Fixture with GPV3.2 hour 1 changed to "no".
fn changed_fixture() -> String {
    let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    v["fact"]["data"][DAY_KEY]["GPV3.2"]["1"] = "no".into();
    v.to_string()
}

fn text_of(req: &Request) -> String {
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    body["text"].as_str().unwrap().to_owned()
}

fn read_state(p: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[tokio::test]
async fn posts_once_and_survives_restart() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;

    let mut bot = env.bot();
    assert!(matches!(
        bot.check(now()).await,
        CheckOutcome::Posted(PostKind::Daily)
    ));
    let photos = env.tg_requests("sendPhoto").await;
    assert_eq!(photos.len(), 1);
    let body = String::from_utf8_lossy(&photos[0].body);
    assert!(body.contains("@channel"));
    assert!(body.contains("🔴 08:00–12:00 — світла немає"));
    assert!(body.contains("Без світла сьогодні: 4 год"));
    assert!(body.contains("🏠 Черга 3.2 · Київська обл."));

    let state = read_state(&env.state_path());
    assert_eq!(state["posted_date"], "2026-10-24");
    assert_eq!(state["posted_image_etag"], "\"img1\"");

    // Repeated checks without changes post nothing.
    assert!(matches!(bot.check(now()).await, CheckOutcome::Unchanged));
    // A restart does not re-post the same schedule.
    let mut bot = env.bot();
    assert!(matches!(bot.check(now()).await, CheckOutcome::Unchanged));
    assert_eq!(env.tg_requests("sendPhoto").await.len(), 1);
}

#[tokio::test]
async fn update_only_timestamp_does_not_post() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    let mut bot = env.bot();
    bot.check(now()).await;

    let touched = FIXTURE.replace("24.10.2026 14:42", "24.10.2026 15:10");
    env.serve(&touched, "\"img2\"").await;
    assert!(matches!(bot.check(now()).await, CheckOutcome::Unchanged));
    assert_eq!(env.tg_requests("sendPhoto").await.len(), 1);
}

#[tokio::test]
async fn change_waits_for_fresh_image() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    let mut bot = env.bot();
    let t0 = now();
    bot.check(t0).await;

    // Schedule changed but the CDN still serves the old image.
    env.serve(&changed_fixture(), "\"img1\"").await;
    assert!(matches!(bot.check(t0).await, CheckOutcome::WaitingForImage));
    let t1 = t0 + chrono::Duration::minutes(5);
    assert!(matches!(bot.check(t1).await, CheckOutcome::WaitingForImage));
    assert_eq!(env.tg_requests("sendPhoto").await.len(), 1);

    // The fresh image arrives: posted as an update.
    env.serve(&changed_fixture(), "\"img2\"").await;
    assert!(matches!(
        bot.check(t1).await,
        CheckOutcome::Posted(PostKind::Updated)
    ));
    let photos = env.tg_requests("sendPhoto").await;
    assert_eq!(photos.len(), 2);
    assert!(String::from_utf8_lossy(&photos[1].body).contains("🔄 Графік на сьогодні оновлено"));
    assert!(bot.state().pending_change_since.is_none());
}

#[tokio::test]
async fn change_posts_with_old_image_after_wait_limit() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    let mut bot = env.bot();
    let t0 = now();
    bot.check(t0).await;

    env.serve(&changed_fixture(), "\"img1\"").await;
    assert!(matches!(bot.check(t0).await, CheckOutcome::WaitingForImage));
    // Pending timer survives a restart.
    let mut bot = env.bot();
    assert!(matches!(
        bot.check(t0 + chrono::Duration::seconds(899)).await,
        CheckOutcome::WaitingForImage
    ));
    assert!(matches!(
        bot.check(t0 + chrono::Duration::seconds(900)).await,
        CheckOutcome::Posted(PostKind::Updated)
    ));
}

#[tokio::test]
async fn failure_alert_once_then_recovery_once() {
    let env = Env::new().await;
    env.serve_error(500).await;
    let mut bot = env.bot();

    for _ in 0..2 {
        assert!(matches!(bot.check(now()).await, CheckOutcome::Failed(_)));
    }
    assert!(env.tg_requests("sendMessage").await.is_empty());

    bot.check(now()).await;
    let messages = env.tg_requests("sendMessage").await;
    assert_eq!(messages.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&messages[0].body).unwrap();
    assert_eq!(body["chat_id"], 777);
    assert_eq!(
        text_of(&messages[0]),
        "⚠️ Бот не може отримати графік відключень (невдалих перевірок поспіль: 3).\nОстання помилка: HTTP 500"
    );

    // More failures (and a restart) do not repeat the alert.
    bot.check(now()).await;
    let mut bot = env.bot();
    bot.check(now()).await;
    assert_eq!(env.tg_requests("sendMessage").await.len(), 1);
    assert_eq!(bot.state().consecutive_failures, 5);

    // Recovery: one message, and the schedule is posted.
    env.serve(FIXTURE, "\"img1\"").await;
    assert!(matches!(
        bot.check(now()).await,
        CheckOutcome::Posted(PostKind::Daily)
    ));
    bot.check(now()).await;
    let messages = env.tg_requests("sendMessage").await;
    assert_eq!(messages.len(), 2);
    assert_eq!(
        text_of(&messages[1]),
        "✅ Бот знову отримує графік відключень."
    );
    assert_eq!(bot.state().consecutive_failures, 0);
}

#[tokio::test]
async fn image_failure_counts_as_failed_check() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    env.mirror.reset().await;
    Mock::given(path("/data/kyiv-region.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
        .mount(&env.mirror)
        .await;
    let mut bot = env.bot();
    let outcome = bot.check(now()).await;
    assert!(matches!(outcome, CheckOutcome::Failed(_)), "{outcome}");
    assert_eq!(bot.state().consecutive_failures, 1);
}

#[tokio::test]
async fn telegram_failure_is_not_a_data_failure() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    env.tg.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"ok":false,"error_code":403,"description":"Forbidden: bot is not a member"}"#,
        ))
        .mount(&env.tg)
        .await;
    let mut bot = env.bot();
    assert!(matches!(bot.check(now()).await, CheckOutcome::SendFailed));
    assert_eq!(bot.state().consecutive_failures, 0);
    assert!(bot.state().posted_date.is_none());
}

#[tokio::test]
async fn stale_data_is_reported() {
    let env = Env::new().await;
    env.serve(FIXTURE, "\"img1\"").await;
    let mut bot = env.bot();
    let late = now() + chrono::Duration::hours(5);
    for _ in 0..3 {
        bot.check(late).await;
    }
    let messages = env.tg_requests("sendMessage").await;
    assert_eq!(messages.len(), 1);
    assert!(text_of(&messages[0]).contains("дані застарілі (оновлено 5 год тому)"));
}

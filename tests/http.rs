//! HTTP-level tests against local mock servers (no real network).

use std::time::Duration;

use dtek_power_off_bot::config::ChatId;
use dtek_power_off_bot::error::FetchError;
use dtek_power_off_bot::source::Source;
use dtek_power_off_bot::telegram::{Telegram, TelegramError};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "123:secret";

fn source(server: &MockServer, timeout: Duration) -> Source {
    Source::new(
        format!("{}/data.json", server.uri()),
        format!("{}/image.png", server.uri()),
        timeout,
    )
    .unwrap()
    .with_retry_delays(vec![Duration::ZERO, Duration::ZERO])
}

fn telegram(server: &MockServer) -> Telegram {
    Telegram::with_api_base(server.uri(), TOKEN.into(), Duration::from_secs(5))
        .unwrap()
        .with_backoff(Duration::ZERO)
}

/// URL of a local port nobody listens on (mock servers are pooled, so dropping one is not enough).
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn json_200_then_304_uses_cached_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data.json"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_string("{\"a\":1}"),
        )
        .mount(&server)
        .await;

    let mut src = source(&server, Duration::from_secs(5));
    assert_eq!(&*src.fetch_json().await.unwrap(), "{\"a\":1}");
    assert_eq!(&*src.fetch_json().await.unwrap(), "{\"a\":1}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].headers.contains_key("if-none-match"));
    assert_eq!(requests[1].headers["if-none-match"], "\"v1\"");
    assert!(
        requests[0].headers["user-agent"]
            .to_str()
            .unwrap()
            .starts_with("dtek-power-off-bot/")
    );
}

#[tokio::test]
async fn json_5xx_is_retried_then_fails() {
    let server = MockServer::start().await;
    Mock::given(path("/data.json"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let mut src = source(&server, Duration::from_secs(5));
    assert_eq!(src.fetch_json().await.unwrap_err(), FetchError::Http(502));
    assert_eq!(hits(&server).await, 3);
}

#[tokio::test]
async fn json_5xx_then_success_within_one_check() {
    let server = MockServer::start().await;
    Mock::given(path("/data.json"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/data.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let mut src = source(&server, Duration::from_secs(5));
    assert_eq!(&*src.fetch_json().await.unwrap(), "ok");
}

#[tokio::test]
async fn json_404_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(path("/data.json"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let mut src = source(&server, Duration::from_secs(5));
    assert_eq!(src.fetch_json().await.unwrap_err(), FetchError::Http(404));
    assert_eq!(hits(&server).await, 1);
}

#[tokio::test]
async fn json_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/data.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("late")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    let mut src = source(&server, Duration::from_millis(200));
    assert_eq!(src.fetch_json().await.unwrap_err(), FetchError::Timeout);
}

#[tokio::test]
async fn unreachable_server_is_a_network_error() {
    let mut src = Source::new(
        format!("{}/data.json", closed_port_url()),
        String::new(),
        Duration::from_secs(2),
    )
    .unwrap()
    .with_retry_delays(vec![]);
    assert!(matches!(
        src.fetch_json().await.unwrap_err(),
        FetchError::Network(_)
    ));
}

#[tokio::test]
async fn image_with_etag() {
    let server = MockServer::start().await;
    Mock::given(path("/image.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"img\"")
                .set_body_bytes(vec![1u8, 2, 3]),
        )
        .mount(&server)
        .await;
    let img = source(&server, Duration::from_secs(5))
        .fetch_image()
        .await
        .unwrap();
    assert_eq!(img.bytes, vec![1, 2, 3]);
    assert_eq!(img.etag.as_deref(), Some("\"img\""));
}

#[tokio::test]
async fn telegram_send_message_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":{}}"#))
        .mount(&server)
        .await;
    telegram(&server)
        .send_message(&ChatId::Id(42), "<b>hi</b>", true)
        .await
        .unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["chat_id"], 42);
    assert_eq!(body["text"], "<b>hi</b>");
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["disable_notification"], true);
}

#[tokio::test]
async fn telegram_send_photo_multipart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{TOKEN}/sendPhoto")))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":{}}"#))
        .mount(&server)
        .await;
    telegram(&server)
        .send_photo(&ChatId::Name("@chan".into()), b"PNGDATA", "caption", false)
        .await
        .unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let body = String::from_utf8_lossy(&req.body);
    assert!(body.contains("name=\"chat_id\"\r\n\r\n@chan"));
    assert!(body.contains("name=\"photo\"; filename=\"schedule.png\""));
    assert!(body.contains("PNGDATA"));
}

#[tokio::test]
async fn telegram_429_waits_and_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":1}}"#,
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true,"result":{}}"#))
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    telegram(&server)
        .send_message(&ChatId::Id(1), "x", false)
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(hits(&server).await, 2);
}

#[tokio::test]
async fn telegram_5xx_gives_up_after_three_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let err = telegram(&server)
        .send_message(&ChatId::Id(1), "x", false)
        .await
        .unwrap_err();
    assert!(matches!(err, TelegramError::Api { code: 502, .. }));
    assert_eq!(hits(&server).await, 4);
}

#[tokio::test]
async fn telegram_4xx_is_not_retried_and_hides_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
        ))
        .mount(&server)
        .await;
    let tg = telegram(&server);
    let err = tg
        .send_message(&ChatId::Id(1), "x", false)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Telegram API error 400: Bad Request: chat not found"
    );
    assert_eq!(hits(&server).await, 1);
    assert!(!format!("{tg:?}").contains("secret"));
}

#[tokio::test]
async fn telegram_network_error_hides_token() {
    let tg = Telegram::with_api_base(closed_port_url(), TOKEN.into(), Duration::from_secs(2))
        .unwrap()
        .with_backoff(Duration::ZERO);
    let err = tg
        .send_message(&ChatId::Id(1), "x", false)
        .await
        .unwrap_err();
    assert!(matches!(err, TelegramError::Network(_)));
    assert!(!err.to_string().contains("secret"), "{err}");
}

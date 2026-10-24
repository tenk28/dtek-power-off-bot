use std::io::IsTerminal;
use std::pin::pin;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use dtek_power_off_bot::bot::Bot;
use dtek_power_off_bot::config::{self, Config};
use dtek_power_off_bot::source::Source;
use dtek_power_off_bot::telegram::Telegram;

/// Photo uploads may take longer than the mirror fetches.
const MIN_TELEGRAM_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let path = config::config_path_from_args(std::env::args())?;
    let config = Config::load(&path)?;
    let token = config::bot_token()?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %path.display(),
        region = %config.schedule.region,
        queue = %config.schedule.queue,
        channel = %config.telegram.channel_id,
        interval_secs = config.polling.check_interval_secs,
        state = %config.storage.state_path.display(),
        "starting"
    );

    let source = Source::new(config.json_url(), config.image_url(), config.http_timeout())
        .context("cannot create HTTP client")?;
    let telegram = Telegram::new(token, config.http_timeout().max(MIN_TELEGRAM_TIMEOUT))
        .context("cannot create Telegram client")?;
    let interval = config.check_interval();
    let mut bot = Bot::new(config, source, telegram);

    // State is saved synchronously inside a check, so cancelling at an await
    // point never leaves a half-written file (and the write is atomic anyway).
    let mut shutdown = pin!(shutdown_signal());
    loop {
        tokio::select! {
            outcome = bot.check(Utc::now()) => {
                info!(
                    result = %outcome,
                    failures = bot.state().consecutive_failures,
                    "check finished"
                );
            }
            _ = &mut shutdown => break,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = &mut shutdown => break,
        }
    }
    info!("shutdown signal received, exiting");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("cannot install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

//! Check errors: English for logs (`Display`), Ukrainian for user-facing alerts.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("network error: {0}")]
    Network(String),
    #[error("request timed out")]
    Timeout,
    #[error("unexpected HTTP status {0}")]
    Http(u16),
}

impl FetchError {
    pub fn from_reqwest(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            FetchError::Timeout
        } else if let Some(status) = err.status() {
            FetchError::Http(status.as_u16())
        } else {
            // without_url: URLs never reach the logs.
            FetchError::Network(err.without_url().to_string())
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            FetchError::Network(_) | FetchError::Timeout => true,
            FetchError::Http(code) => *code >= 500 || *code == 429,
        }
    }

    fn reason_uk(&self) -> String {
        match self {
            FetchError::Network(_) => "сервер недоступний".to_owned(),
            FetchError::Timeout => "сервер не відповідає (тайм-аут)".to_owned(),
            FetchError::Http(code) => format!("HTTP {code}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScheduleError {
    #[error("malformed data: {0}")]
    Format(String),
    #[error("source reports failed update: {0}")]
    SourceNotOk(String),
    #[error("data is stale: last updated {age_secs}s ago")]
    Stale { age_secs: i64 },
    #[error("no data for today ({0})")]
    NoToday(String),
    #[error("queue {0} is missing in the data")]
    QueueMissing(String),
    #[error("unknown status {status:?} for hour {hour}")]
    UnknownStatus { hour: u8, status: String },
}

impl ScheduleError {
    fn reason_uk(&self) -> String {
        match self {
            ScheduleError::Format(_) | ScheduleError::UnknownStatus { .. } => {
                "некоректний формат даних".to_owned()
            }
            ScheduleError::SourceNotOk(_) => "джерело даних повідомляє про помилку".to_owned(),
            ScheduleError::Stale { age_secs } => {
                format!("дані застарілі (оновлено {} тому)", format_age(*age_secs))
            }
            ScheduleError::NoToday(_) => "немає даних на сьогодні".to_owned(),
            ScheduleError::QueueMissing(queue) => {
                let queue = queue.strip_prefix("GPV").unwrap_or(queue);
                format!("черга {queue} відсутня в даних")
            }
        }
    }
}

/// Anything that makes a check fail.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CheckError {
    #[error("schedule fetch failed: {0}")]
    Json(FetchError),
    #[error("image fetch failed: {0}")]
    Image(FetchError),
    #[error("{0}")]
    Schedule(#[from] ScheduleError),
}

impl CheckError {
    /// Short Ukrainian reason for the failure alert.
    pub fn reason_uk(&self) -> String {
        match self {
            CheckError::Json(e) => e.reason_uk(),
            CheckError::Image(e) => format!("картинка графіка: {}", e.reason_uk()),
            CheckError::Schedule(e) => e.reason_uk(),
        }
    }
}

fn format_age(secs: i64) -> String {
    let minutes = secs.max(0) / 60;
    if minutes >= 60 {
        format!("{} год", minutes / 60)
    } else {
        format!("{} хв", minutes.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ukrainian_reasons() {
        let cases = [
            (
                CheckError::Json(FetchError::Network("dns".into())),
                "сервер недоступний",
            ),
            (CheckError::Json(FetchError::Http(502)), "HTTP 502"),
            (
                CheckError::Image(FetchError::Http(404)),
                "картинка графіка: HTTP 404",
            ),
            (
                ScheduleError::Stale {
                    age_secs: 5 * 3600 + 120,
                }
                .into(),
                "дані застарілі (оновлено 5 год тому)",
            ),
            (
                ScheduleError::Stale { age_secs: 600 }.into(),
                "дані застарілі (оновлено 10 хв тому)",
            ),
            (
                ScheduleError::NoToday("x".into()).into(),
                "немає даних на сьогодні",
            ),
            (
                ScheduleError::QueueMissing("GPV3.2".into()).into(),
                "черга 3.2 відсутня в даних",
            ),
            (
                ScheduleError::Format("x".into()).into(),
                "некоректний формат даних",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.reason_uk(), expected);
        }
    }
}

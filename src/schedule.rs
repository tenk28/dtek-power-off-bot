//! Parsing of the mirror JSON into 48 half-hour slots for today (Europe/Kyiv).

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Europe::Kyiv;
use serde::Deserialize;

use crate::error::ScheduleError;

pub const SLOTS_PER_DAY: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    On,
    Off,
    MaybeOff,
}

impl SlotState {
    pub fn code(self) -> char {
        match self {
            SlotState::On => '1',
            SlotState::Off => '0',
            SlotState::MaybeOff => 'm',
        }
    }
}

/// One day of half-hour slots; slot 0 is 00:00–00:30, slot 47 is 23:30–24:00.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaySchedule {
    pub slots: [SlotState; SLOTS_PER_DAY],
}

/// A run of equal slots: `[start, end)` in slot indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub start: usize,
    pub end: usize,
    pub state: SlotState,
}

impl DaySchedule {
    /// Builds slots from the hourly map `"1".."24"` → status.
    pub fn from_hours(hours: &HashMap<String, serde_json::Value>) -> Result<Self, ScheduleError> {
        let mut slots = [SlotState::On; SLOTS_PER_DAY];
        for hour in 1..=24u8 {
            let value = hours.get(&hour.to_string()).ok_or_else(|| {
                ScheduleError::Format(format!("hour {hour} is missing in the queue data"))
            })?;
            let status = value.as_str().ok_or_else(|| {
                ScheduleError::Format(format!("hour {hour} status is not a string: {value}"))
            })?;
            let (first, second) = halves(status).ok_or_else(|| ScheduleError::UnknownStatus {
                hour,
                status: status.to_owned(),
            })?;
            let i = usize::from(hour - 1) * 2;
            slots[i] = first;
            slots[i + 1] = second;
        }
        Ok(Self { slots })
    }

    /// Compact 48-char representation used in the state file.
    pub fn code(&self) -> String {
        self.slots.iter().map(|s| s.code()).collect()
    }

    pub fn intervals(&self) -> Vec<Interval> {
        let mut out: Vec<Interval> = Vec::new();
        for (i, &state) in self.slots.iter().enumerate() {
            match out.last_mut() {
                Some(last) if last.state == state => last.end = i + 1,
                _ => out.push(Interval {
                    start: i,
                    end: i + 1,
                    state,
                }),
            }
        }
        out
    }

    pub fn count(&self, state: SlotState) -> usize {
        self.slots.iter().filter(|&&s| s == state).count()
    }
}

fn halves(status: &str) -> Option<(SlotState, SlotState)> {
    use SlotState::*;
    Some(match status {
        "yes" => (On, On),
        "no" => (Off, Off),
        "maybe" => (MaybeOff, MaybeOff),
        "first" => (Off, On),
        "second" => (On, Off),
        "mfirst" => (MaybeOff, On),
        "msecond" => (On, MaybeOff),
        _ => return None,
    })
}

/// The subset of the mirror JSON the bot relies on.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionData {
    #[serde(default)]
    pub region_affiliation: Option<String>,
    pub last_updated: DateTime<Utc>,
    pub last_update_status: UpdateStatus,
    pub fact: Fact,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateStatus {
    pub ok: bool,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fact {
    /// Day key (unix seconds of local midnight) → queue key → hour → status.
    /// Hours are kept as raw values so one odd queue cannot break parsing of ours.
    pub data: HashMap<String, HashMap<String, HashMap<String, serde_json::Value>>>,
    /// DTEK's own "updated at" string, Kyiv time.
    #[serde(default)]
    pub update: Option<String>,
}

pub fn parse(json: &str) -> Result<RegionData, ScheduleError> {
    serde_json::from_str(json).map_err(|e| ScheduleError::Format(e.to_string()))
}

/// Today's schedule for one queue, ready to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Today {
    pub date: NaiveDate,
    pub schedule: DaySchedule,
    pub dtek_update: Option<String>,
    pub region_affiliation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extracted {
    Today(Today),
    /// Shortly after midnight, before DTEK publishes the new day.
    NotYetPublished {
        date: NaiveDate,
    },
}

pub struct ExtractParams<'a> {
    pub queue_key: &'a str,
    pub now: DateTime<Utc>,
    pub stale_after_secs: u64,
    pub missing_today_grace_secs: u64,
}

pub fn extract(data: &RegionData, p: &ExtractParams<'_>) -> Result<Extracted, ScheduleError> {
    if !data.last_update_status.ok {
        let detail = data
            .last_update_status
            .message
            .clone()
            .or_else(|| data.last_update_status.status.clone())
            .unwrap_or_default();
        return Err(ScheduleError::SourceNotOk(detail));
    }

    let age_secs = (p.now - data.last_updated).num_seconds();
    if age_secs > i64::try_from(p.stale_after_secs).unwrap_or(i64::MAX) {
        return Err(ScheduleError::Stale { age_secs });
    }

    let date = kyiv_date(p.now);
    let key = day_key(date);
    let Some(queues) = data.fact.data.get(&key.to_string()) else {
        let since_midnight = p.now.timestamp() - key;
        if since_midnight < i64::try_from(p.missing_today_grace_secs).unwrap_or(i64::MAX) {
            return Ok(Extracted::NotYetPublished { date });
        }
        return Err(ScheduleError::NoToday(format!("{date}, key {key}")));
    };

    let hours = queues
        .get(p.queue_key)
        .ok_or_else(|| ScheduleError::QueueMissing(p.queue_key.to_owned()))?;
    let schedule = DaySchedule::from_hours(hours)?;

    Ok(Extracted::Today(Today {
        date,
        schedule,
        dtek_update: data.fact.update.clone(),
        region_affiliation: data.region_affiliation.clone(),
    }))
}

/// Calendar date in Kyiv at the given instant.
pub fn kyiv_date(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&Kyiv).date_naive()
}

/// Day key used by the mirror: unix seconds of 00:00 Europe/Kyiv on `date`.
pub fn day_key(date: NaiveDate) -> i64 {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
    // Kyiv DST switches at 03:00/04:00, so local midnight is never ambiguous or skipped.
    Kyiv.from_local_datetime(&midnight)
        .earliest()
        .expect("local midnight exists in Europe/Kyiv")
        .timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use SlotState::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/kyiv-region.json");

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn params(now: DateTime<Utc>) -> ExtractParams<'static> {
        ExtractParams {
            queue_key: "GPV3.2",
            now,
            stale_after_secs: 3 * 3600,
            missing_today_grace_secs: 3600,
        }
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn hours_from(statuses: &[&str; 24]) -> HashMap<String, serde_json::Value> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, s)| ((i + 1).to_string(), serde_json::Value::from(*s)))
            .collect()
    }

    /// Minimal valid document with one day and one queue.
    fn synthetic(day: NaiveDate, last_updated: &str, statuses: &[&str; 24]) -> String {
        let hours = serde_json::to_string(&hours_from(statuses)).unwrap();
        format!(
            r#"{{"regionAffiliation":"Київська обл.","lastUpdated":"{last_updated}",
               "lastUpdateStatus":{{"status":"parsed","ok":true}},
               "fact":{{"update":"24.10.2026 14:42","data":{{"{}":{{"GPV3.2":{hours}}}}}}}}}"#,
            day_key(day)
        )
    }

    #[test]
    fn fixture_parses_for_its_day() {
        let data = parse(FIXTURE).unwrap();
        let now = data.last_updated + chrono::Duration::minutes(5);
        let Extracted::Today(today) = extract(&data, &params(now)).unwrap() else {
            panic!("expected today's schedule");
        };
        assert_eq!(today.date, date(2026, 10, 24));
        assert_eq!(today.dtek_update.as_deref(), Some("24.10.2026 14:42"));
        assert_eq!(today.region_affiliation.as_deref(), Some("Київська обл."));
        // Fixture: hours 9..=12 are "no", the rest "yes" → off 08:00–12:00.
        assert_eq!(
            today.schedule.code(),
            format!("{}{}{}", "1".repeat(16), "0".repeat(8), "1".repeat(24))
        );
    }

    #[test]
    fn day_key_matches_fixture_and_midnight_edges() {
        assert_eq!(day_key(date(2026, 10, 24)), 1792789200);
        // 23:59:59 Kyiv (UTC+3) is still the 24th, 00:00:00 is the 25th.
        assert_eq!(kyiv_date(utc("2026-10-24T20:59:59Z")), date(2026, 10, 24));
        assert_eq!(kyiv_date(utc("2026-10-24T21:00:00Z")), date(2026, 10, 25));
    }

    #[test]
    fn day_key_across_dst_transitions() {
        // Autumn: 25 October 2026 has 25 hours (UTC+3 → UTC+2).
        let oct25 = day_key(date(2026, 10, 25));
        let oct26 = day_key(date(2026, 10, 26));
        assert_eq!(oct25, utc("2026-10-24T21:00:00Z").timestamp());
        assert_eq!(oct26 - oct25, 25 * 3600);
        assert_eq!(kyiv_date(utc("2026-10-25T21:59:59Z")), date(2026, 10, 25));
        assert_eq!(kyiv_date(utc("2026-10-25T22:00:00Z")), date(2026, 10, 26));
        // Spring: 29 March 2026 has 23 hours (UTC+2 → UTC+3).
        let mar29 = day_key(date(2026, 3, 29));
        let mar30 = day_key(date(2026, 3, 30));
        assert_eq!(mar29, utc("2026-03-28T22:00:00Z").timestamp());
        assert_eq!(mar30 - mar29, 23 * 3600);
    }

    #[test]
    fn half_hour_statuses_map_to_slots() {
        let mut statuses = ["yes"; 24];
        statuses[0] = "first";
        statuses[1] = "second";
        statuses[2] = "mfirst";
        statuses[3] = "msecond";
        statuses[4] = "maybe";
        statuses[5] = "no";
        let s = DaySchedule::from_hours(&hours_from(&statuses)).unwrap();
        assert_eq!(
            &s.slots[..12],
            &[
                Off, On, On, Off, MaybeOff, On, On, MaybeOff, MaybeOff, MaybeOff, Off, Off
            ]
        );
        assert_eq!(&s.code()[..12], "0110m11mmm00");
    }

    #[test]
    fn intervals_merge_and_reach_midnight() {
        let mut statuses = ["yes"; 24];
        statuses[22] = "second"; // 22:30–23:00 off
        statuses[23] = "no"; // 23:00–24:00 off
        let s = DaySchedule::from_hours(&hours_from(&statuses)).unwrap();
        assert_eq!(
            s.intervals(),
            vec![
                Interval {
                    start: 0,
                    end: 45,
                    state: On
                },
                Interval {
                    start: 45,
                    end: 48,
                    state: Off
                },
            ]
        );
    }

    #[test]
    fn schedule_without_outages_is_one_interval() {
        let s = DaySchedule::from_hours(&hours_from(&["yes"; 24])).unwrap();
        assert_eq!(s.intervals().len(), 1);
        assert_eq!(s.count(Off), 0);
    }

    #[test]
    fn unknown_status_is_an_error() {
        let mut statuses = ["yes"; 24];
        statuses[6] = "sometimes";
        let err = DaySchedule::from_hours(&hours_from(&statuses)).unwrap_err();
        assert_eq!(
            err,
            ScheduleError::UnknownStatus {
                hour: 7,
                status: "sometimes".into()
            }
        );
    }

    #[test]
    fn missing_hour_is_an_error() {
        let mut hours = hours_from(&["yes"; 24]);
        hours.remove("24");
        assert!(matches!(
            DaySchedule::from_hours(&hours),
            Err(ScheduleError::Format(_))
        ));
    }

    #[test]
    fn missing_queue_is_an_error() {
        let data = parse(FIXTURE).unwrap();
        let now = data.last_updated;
        let p = ExtractParams {
            queue_key: "GPV9.9",
            ..params(now)
        };
        assert_eq!(
            extract(&data, &p).unwrap_err(),
            ScheduleError::QueueMissing("GPV9.9".into())
        );
    }

    #[test]
    fn stale_data_is_an_error() {
        let data = parse(FIXTURE).unwrap();
        let now = data.last_updated + chrono::Duration::hours(5);
        assert!(matches!(
            extract(&data, &params(now)),
            Err(ScheduleError::Stale { .. })
        ));
    }

    #[test]
    fn failed_source_status_is_an_error() {
        let json = synthetic(date(2026, 10, 24), "2026-10-24T12:00:00Z", &["yes"; 24])
            .replace("\"ok\":true", "\"ok\":false");
        let data = parse(&json).unwrap();
        assert!(matches!(
            extract(&data, &params(utc("2026-10-24T12:01:00Z"))),
            Err(ScheduleError::SourceNotOk(_))
        ));
    }

    #[test]
    fn missing_today_is_tolerated_only_shortly_after_midnight() {
        // Data only for the 24th; now is the 25th in Kyiv.
        let json = synthetic(date(2026, 10, 24), "2026-10-24T21:20:00Z", &["yes"; 24]);
        let data = parse(&json).unwrap();
        assert_eq!(
            extract(&data, &params(utc("2026-10-24T21:30:00Z"))).unwrap(),
            Extracted::NotYetPublished {
                date: date(2026, 10, 25)
            }
        );
        let late = utc("2026-10-24T22:30:00Z");
        let json = synthetic(date(2026, 10, 24), "2026-10-24T22:20:00Z", &["yes"; 24]);
        assert!(matches!(
            extract(&parse(&json).unwrap(), &params(late)),
            Err(ScheduleError::NoToday(_))
        ));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(matches!(parse("{}"), Err(ScheduleError::Format(_))));
        assert!(matches!(parse("<html>"), Err(ScheduleError::Format(_))));
    }
}

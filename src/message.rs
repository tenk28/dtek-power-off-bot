//! Ukrainian user-facing texts (Telegram HTML parse mode).

use chrono::{Datelike, NaiveDate, Weekday};

use crate::schedule::{SlotState, Today};

/// Telegram limit for photo captions, counted after entity parsing.
pub const CAPTION_LIMIT: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostKind {
    /// First post of the day (or first run).
    Daily,
    /// Today's schedule changed after it was posted.
    Updated,
}

const MONTHS_GENITIVE: [&str; 12] = [
    "січня",
    "лютого",
    "березня",
    "квітня",
    "травня",
    "червня",
    "липня",
    "серпня",
    "вересня",
    "жовтня",
    "листопада",
    "грудня",
];

fn weekday_uk(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Понеділок",
        Weekday::Tue => "Вівторок",
        Weekday::Wed => "Середа",
        Weekday::Thu => "Четвер",
        Weekday::Fri => "П’ятниця",
        Weekday::Sat => "Субота",
        Weekday::Sun => "Неділя",
    }
}

/// "Субота, 24 жовтня".
pub fn format_date(date: NaiveDate) -> String {
    format!(
        "{}, {} {}",
        weekday_uk(date.weekday()),
        date.day(),
        MONTHS_GENITIVE[date.month0() as usize]
    )
}

/// "4 год", "4 год 30 хв", "30 хв".
pub fn format_duration(minutes: usize) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} хв"),
        (h, 0) => format!("{h} год"),
        (h, m) => format!("{h} год {m} хв"),
    }
}

/// Slot boundary as "HH:MM"; slot 48 is "24:00".
fn slot_time(slot: usize) -> String {
    format!("{:02}:{:02}", slot / 2, (slot % 2) * 30)
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Length as Telegram counts it: tags removed, each entity counted as one character.
fn visible_len(html: &str) -> usize {
    let mut len = 0;
    let mut in_tag = false;
    let mut in_entity = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            '&' => {
                in_entity = true;
                len += 1;
            }
            ';' if in_entity => in_entity = false,
            _ if in_entity => {}
            _ => len += 1,
        }
    }
    len
}

pub struct CaptionInput<'a> {
    pub kind: PostKind,
    pub today: &'a Today,
    pub queue: &'a str,
    pub region_title: &'a str,
}

pub fn caption(input: &CaptionInput<'_>) -> String {
    let schedule = &input.today.schedule;
    let title = match input.kind {
        PostKind::Daily => "⚡ Графік відключень на сьогодні",
        PostKind::Updated => "🔄 Графік на сьогодні оновлено",
    };
    let header = [
        format!("<b>{title}</b>"),
        format!("📅 {}", format_date(input.today.date)),
        format!(
            "🏠 Черга {} · {}",
            escape_html(input.queue),
            escape_html(input.region_title)
        ),
        String::new(),
    ];

    let mut body: Vec<String> = schedule
        .intervals()
        .into_iter()
        .filter_map(|iv| {
            let (icon, label) = match iv.state {
                SlotState::On => return None,
                SlotState::Off => ("🔴", "світла немає"),
                SlotState::MaybeOff => ("🟡", "можливе відключення"),
            };
            Some(format!(
                "{icon} {}–{} — {label}",
                slot_time(iv.start),
                slot_time(iv.end)
            ))
        })
        .collect();

    let mut footer = Vec::new();
    if body.is_empty() {
        footer.push("✅ Відключень сьогодні не заплановано".to_owned());
    } else {
        footer.push(String::new());
        let off = schedule.count(SlotState::Off);
        if off > 0 {
            footer.push(format!(
                "Без світла сьогодні: {}",
                format_duration(off * 30)
            ));
        }
    }
    if let Some(update) = &input.today.dtek_update {
        footer.push(format!("🕒 Оновлено ДТЕК: {}", escape_html(update)));
    }

    let join = |body: &[String]| {
        header
            .iter()
            .chain(body)
            .chain(&footer)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut text = join(&body);
    // Practically unreachable (at most 24 interval lines), but never exceed the limit.
    let mut truncated = false;
    while visible_len(&text) > CAPTION_LIMIT && !body.is_empty() {
        if truncated {
            body.pop(); // the previous "…"
        }
        body.pop();
        body.push("…".to_owned());
        truncated = true;
        text = join(&body);
    }
    text
}

pub fn failure_alert(consecutive_failures: u32, reason: &str) -> String {
    format!(
        "⚠️ Бот не може отримати графік відключень (невдалих перевірок поспіль: {consecutive_failures}).\nОстання помилка: {}",
        escape_html(reason)
    )
}

pub const RECOVERY: &str = "✅ Бот знову отримує графік відключень.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::DaySchedule;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn today_from_code(code: &str) -> Today {
        assert_eq!(code.len(), 48);
        let mut slots = [SlotState::On; 48];
        for (slot, c) in slots.iter_mut().zip(code.chars()) {
            *slot = match c {
                '1' => SlotState::On,
                '0' => SlotState::Off,
                'm' => SlotState::MaybeOff,
                _ => unreachable!(),
            };
        }
        Today {
            date: date(2026, 10, 24),
            schedule: DaySchedule { slots },
            dtek_update: Some("24.10.2026 14:42".into()),
            region_affiliation: Some("Київська обл.".into()),
        }
    }

    fn render(kind: PostKind, today: &Today) -> String {
        caption(&CaptionInput {
            kind,
            today,
            queue: "3.2",
            region_title: "Київська обл.",
        })
    }

    #[test]
    fn caption_matches_spec_example() {
        // Off 08:00–12:00, maybe 18:30–20:00.
        let code = format!(
            "{}{}{}{}{}",
            "1".repeat(16),
            "0".repeat(8),
            "1".repeat(13),
            "m".repeat(3),
            "1".repeat(8)
        );
        let expected = "\
<b>⚡ Графік відключень на сьогодні</b>
📅 Субота, 24 жовтня
🏠 Черга 3.2 · Київська обл.

🔴 08:00–12:00 — світла немає
🟡 18:30–20:00 — можливе відключення

Без світла сьогодні: 4 год
🕒 Оновлено ДТЕК: 24.10.2026 14:42";
        assert_eq!(render(PostKind::Daily, &today_from_code(&code)), expected);
    }

    #[test]
    fn caption_for_update_with_half_hours_up_to_midnight() {
        let code = format!("0{}{}", "1".repeat(44), "000");
        let expected = "\
<b>🔄 Графік на сьогодні оновлено</b>
📅 Субота, 24 жовтня
🏠 Черга 3.2 · Київська обл.

🔴 00:00–00:30 — світла немає
🔴 22:30–24:00 — світла немає

Без світла сьогодні: 2 год
🕒 Оновлено ДТЕК: 24.10.2026 14:42";
        assert_eq!(render(PostKind::Updated, &today_from_code(&code)), expected);
    }

    #[test]
    fn caption_without_outages() {
        let expected = "\
<b>⚡ Графік відключень на сьогодні</b>
📅 Субота, 24 жовтня
🏠 Черга 3.2 · Київська обл.

✅ Відключень сьогодні не заплановано
🕒 Оновлено ДТЕК: 24.10.2026 14:42";
        assert_eq!(
            render(PostKind::Daily, &today_from_code(&"1".repeat(48))),
            expected
        );
    }

    #[test]
    fn caption_with_only_possible_outages_has_no_total() {
        let code = format!("{}{}", "m".repeat(4), "1".repeat(44));
        let text = render(PostKind::Daily, &today_from_code(&code));
        assert!(text.contains("🟡 00:00–02:00 — можливе відключення"));
        assert!(!text.contains("Без світла"));
    }

    #[test]
    fn caption_escapes_dynamic_values() {
        let mut today = today_from_code(&"1".repeat(48));
        today.dtek_update = Some("<b>&".into());
        let text = caption(&CaptionInput {
            kind: PostKind::Daily,
            today: &today,
            queue: "3.2",
            region_title: "A<B>",
        });
        assert!(text.contains("A&lt;B&gt;"));
        assert!(text.contains("&lt;b&gt;&amp;"));
    }

    #[test]
    fn caption_never_exceeds_limit() {
        let mut today = today_from_code(&"10".repeat(24));
        today.dtek_update = Some("x".repeat(700));
        let text = render(PostKind::Daily, &today);
        assert!(visible_len(&text) <= CAPTION_LIMIT);
        assert!(text.contains("\n…\n"));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(30), "30 хв");
        assert_eq!(format_duration(240), "4 год");
        assert_eq!(format_duration(270), "4 год 30 хв");
    }

    #[test]
    fn dates_in_ukrainian() {
        assert_eq!(format_date(date(2026, 10, 24)), "Субота, 24 жовтня");
        assert_eq!(format_date(date(2026, 1, 1)), "Четвер, 1 січня");
        assert_eq!(format_date(date(2026, 5, 1)), "П’ятниця, 1 травня");
        assert_eq!(format_date(date(2026, 12, 31)), "Четвер, 31 грудня");
    }

    #[test]
    fn visible_length_ignores_tags_and_entities() {
        assert_eq!(visible_len("<b>ab</b> &amp; c"), 6);
    }

    #[test]
    fn alerts() {
        assert_eq!(
            failure_alert(3, "HTTP 502"),
            "⚠️ Бот не може отримати графік відключень (невдалих перевірок поспіль: 3).\nОстання помилка: HTTP 502"
        );
    }
}

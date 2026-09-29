use crate::text::duration;
use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, TimeZone};
use skyhook::provider::protocol::Usage;
pub use skyhook::session::stats::agent_label;
pub fn number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.)
    } else if n >= 1000 {
        let value = format!("{:.1}", n as f64 / 1000.);
        format!("{}k", value.trim_end_matches(".0"))
    } else {
        n.to_string()
    }
}
pub fn footer(usage: Usage, context: Option<(u64, u64)>) -> String {
    footer_stats(usage, context).join(" · ")
}

pub fn footer_stats(usage: Usage, context: Option<(u64, u64)>) -> [String; 3] {
    let context =
        context
            .filter(|(_, capacity)| *capacity > 0)
            .map_or("—".into(), |(current, total)| {
                format!(
                    "{:.0}% ({}/{})",
                    current as f64 / total as f64 * 100.,
                    number(current),
                    number(total)
                )
            });
    [
        number(usage.output_tokens),
        format!(
            "{}({})",
            number(usage.input_tokens.saturating_add(usage.cached_input_tokens)),
            number(usage.input_tokens)
        ),
        context,
    ]
}

/// Remove terminal controls while preserving line boundaries and expanding tabs.
pub fn clean(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => output.push_str("    "),
            '\n' => output.push(ch),
            ch if !ch.is_control() => output.push(ch),
            _ => {}
        }
    }
    output
}

/// Text that is safe to paint: `clean` has been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clean(String);

impl From<String> for Clean {
    fn from(text: String) -> Self {
        if text.contains(|ch: char| ch.is_control() && ch != '\n') {
            Self(clean(&text))
        } else {
            Self(text)
        }
    }
}

impl From<&str> for Clean {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl std::ops::Deref for Clean {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// Wall time, in epoch milliseconds, that live counters are painted at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clock(i64);

impl Clock {
    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp_millis())
    }

    #[cfg(test)]
    pub fn at(millis: i64) -> Self {
        Self(millis)
    }

    /// The local day, which decides whether a time shows its date.
    pub fn day(self) -> NaiveDate {
        let at = DateTime::from_timestamp_millis(self.0).unwrap_or_default();
        at.with_timezone(&chrono::Local).date_naive()
    }
}

/// How long something has taken, as epoch milliseconds from the journal's clock.
/// A live timing counts on as the clock moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    Untimed,
    /// Under way since this time.
    Since(i64),
    /// Waiting until this time.
    Until(i64),
    Took {
        since: i64,
        until: i64,
    },
}

impl Timing {
    pub fn live(self) -> bool {
        matches!(self, Self::Since(_) | Self::Until(_))
    }

    /// When it began, if it counts from a start.
    pub fn since(self) -> Option<i64> {
        match self {
            Self::Since(since) | Self::Took { since, .. } => Some(since),
            Self::Untimed | Self::Until(_) => None,
        }
    }

    pub fn text(self, now: Clock) -> Option<String> {
        match self {
            Self::Untimed | Self::Took { .. } => self.took(),
            Self::Since(since) => Some(span(since, now.0)),
            Self::Until(until) if until <= now.0 => Some("now".into()),
            Self::Until(until) => Some(format!("in {}", span(now.0, until))),
        }
    }

    /// A finished duration, which reads the same at any time.
    pub fn took(self) -> Option<String> {
        match self {
            Self::Took { since, until } => Some(span(since, until)),
            Self::Untimed | Self::Since(_) | Self::Until(_) => None,
        }
    }
}

/// How long from `since` to `until` (epoch milliseconds).
pub fn span(since: i64, until: i64) -> String {
    duration(TimeDelta::milliseconds(until.saturating_sub(since)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precision {
    Minutes,
    Seconds,
}

/// `at` (epoch milliseconds) on `zone`'s wall clock: the time alone on `today`,
/// `zone`'s current day, with the date on another, and with the year in another year.
pub fn local_time<Tz: TimeZone>(
    at: i64,
    today: NaiveDate,
    zone: &Tz,
    precision: Precision,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = DateTime::from_timestamp_millis(at)
        .unwrap_or_default()
        .with_timezone(zone);
    let time = match precision {
        Precision::Minutes => "%H:%M",
        Precision::Seconds => "%H:%M:%S",
    };
    let date = if at.date_naive() == today {
        ""
    } else if at.year() == today.year() {
        "%b %-d "
    } else {
        "%b %-d %Y "
    };
    at.format(&format!("{date}{time}")).to_string()
}

pub fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cleaning_preserves_unicode_lines_and_expands_tabs() {
        for (text, expected) in [
            ("plain 界👩‍💻", "plain 界👩‍💻"),
            ("a\tb\n", "a    b\n"),
            ("\u{1b}[31mred\u{1b}[0m\r\0", "[31mred[0m"),
        ] {
            assert_eq!(clean(text), expected);
            assert_eq!(*Clean::from(text), *expected);
        }
    }

    #[test]
    fn timings_count_up_down_or_stay_fixed() {
        let now = Clock::at(60_950);
        assert_eq!(Timing::Untimed.text(now), None);
        assert_eq!(Timing::Since(48_400).text(now).unwrap(), "12.5s");
        assert_eq!(Timing::Until(65_000).text(now).unwrap(), "in 4.0s");
        assert_eq!(Timing::Until(60_900).text(now).unwrap(), "now");
        let took = Timing::Took {
            since: 1_000,
            until: 4_200,
        };
        assert_eq!(took.text(now).unwrap(), "3.2s");
        assert_eq!(took.took().unwrap(), "3.2s");
        assert_eq!(Timing::Since(0).took(), None);
        assert_eq!(took.since(), Some(1_000));
        assert!(Timing::Since(0).live() && Timing::Until(0).live() && !took.live());
    }

    #[test]
    fn local_times_name_the_date_only_off_today() {
        let zone = chrono::FixedOffset::east_opt(10 * 3600).unwrap();
        let millis = |text: &str| {
            let at = DateTime::parse_from_rfc3339(text).unwrap();
            at.timestamp_millis()
        };
        let today = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        for (at, precision, expected) in [
            ("2026-09-29T00:05:09+10:00", Precision::Minutes, "00:05"),
            ("2026-09-29T14:02:31+10:00", Precision::Seconds, "14:02:31"),
            // The zone's day, not UTC's: this is still the 28th in UTC.
            ("2026-09-29T05:00:00+10:00", Precision::Minutes, "05:00"),
            (
                "2026-09-28T23:59:00+10:00",
                Precision::Minutes,
                "Sep 28 23:59",
            ),
            (
                "2025-12-31T08:00:00+10:00",
                Precision::Minutes,
                "Dec 31 2025 08:00",
            ),
        ] {
            assert_eq!(local_time(millis(at), today, &zone, precision), expected);
        }
    }
}

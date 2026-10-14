use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{Result, anyhow, bail};
use chrono::{Datelike, Local, Offset as _, TimeZone as _, Timelike};
use chrono_tz::Tz;

use super::MINUTE_MS;

const MAX_SCAN_MINUTES: i64 = 527_040;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerTimeZone {
    System,
    Named(Tz),
}

impl SchedulerTimeZone {
    pub fn from_environment() -> Self {
        std::env::var("TZ")
            .ok()
            .and_then(|value| Tz::from_str(&value).ok())
            .map(Self::Named)
            .unwrap_or(Self::System)
    }

    pub fn named(name: &str) -> Result<Self> {
        Ok(Self::Named(
            Tz::from_str(name).map_err(|_| anyhow!("unknown timezone '{name}'"))?,
        ))
    }

    pub(super) fn local_parts(self, timestamp_ms: i64) -> Result<LocalParts> {
        match self {
            Self::System => {
                let value = Local
                    .timestamp_millis_opt(timestamp_ms)
                    .single()
                    .ok_or_else(|| anyhow!("timestamp is outside the local timezone range"))?;
                Ok(LocalParts::from_datetime(value))
            }
            Self::Named(timezone) => {
                let value = timezone
                    .timestamp_millis_opt(timestamp_ms)
                    .single()
                    .ok_or_else(|| anyhow!("timestamp is outside timezone {timezone}"))?;
                Ok(LocalParts::from_datetime(value))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LocalParts {
    pub(super) minute: u8,
    pub(super) hour: u8,
    day_of_month: u8,
    month: u8,
    day_of_week: u8,
    offset_seconds: i32,
}

impl LocalParts {
    fn from_datetime<T: chrono::TimeZone>(value: chrono::DateTime<T>) -> Self {
        Self {
            minute: value.minute() as u8,
            hour: value.hour() as u8,
            day_of_month: value.day() as u8,
            month: value.month() as u8,
            day_of_week: value.weekday().num_days_from_sunday() as u8,
            offset_seconds: value.offset().fix().local_minus_utc(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSpec {
    source: String,
    minute: Vec<u8>,
    hour: Vec<u8>,
    day_of_month: Vec<u8>,
    month: Vec<u8>,
    day_of_week: Vec<u8>,
}

impl CronSpec {
    pub fn parse(source: &str) -> Result<Self> {
        let fields: Vec<&str> = source.split_whitespace().collect();
        if fields.len() != 5 {
            bail!("Invalid cron expression '{source}'. Expected 5 fields: M H DoM Mon DoW.");
        }
        let spec = Self {
            source: source.trim().to_string(),
            minute: parse_field(fields[0], 0, 59, false)?,
            hour: parse_field(fields[1], 0, 23, false)?,
            day_of_month: parse_field(fields[2], 1, 31, false)?,
            month: parse_field(fields[3], 1, 12, false)?,
            day_of_week: parse_field(fields[4], 0, 6, true)?,
        };
        Ok(spec)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn next_after(&self, after_ms: i64, timezone: SchedulerTimeZone) -> Option<i64> {
        let start = after_ms
            .div_euclid(MINUTE_MS)
            .saturating_add(1)
            .saturating_mul(MINUTE_MS);
        let limit = start.saturating_add((MAX_SCAN_MINUTES - 1).saturating_mul(MINUTE_MS));
        let mut candidate = start;
        while candidate <= limit {
            let parts = timezone.local_parts(candidate).ok()?;
            if self.matches(parts) {
                return Some(candidate);
            }
            candidate = self.skip(timezone, candidate, parts)?;
        }
        None
    }

    /// Advance past a candidate that does not match, by whole calendar fields
    /// rather than a minute at a time, so a legal expression that never matches
    /// (`0 0 30 2 *`) exhausts the year in hundreds of steps instead of half a
    /// million timezone conversions.
    ///
    /// A skip is measured in local calendar fields, so it only holds while local
    /// time advances in lockstep with UTC. Across a DST transition the step is
    /// halved until it lands on the offset it started from; that costs a handful
    /// of lookups near the transition instead of degrading the whole scan.
    fn skip(&self, timezone: SchedulerTimeZone, candidate: i64, parts: LocalParts) -> Option<i64> {
        let mut minutes = self.skip_minutes(parts);
        while minutes > 1 {
            let target = candidate.saturating_add(minutes.saturating_mul(MINUTE_MS));
            if timezone.local_parts(target).ok()?.offset_seconds == parts.offset_seconds {
                return Some(target);
            }
            minutes /= 2;
        }
        Some(candidate.saturating_add(MINUTE_MS))
    }

    /// Minutes that provably hold no match, given the local fields of a
    /// candidate that already failed `matches`. Always at least one.
    fn skip_minutes(&self, parts: LocalParts) -> i64 {
        let into_day = i64::from(parts.hour) * 60 + i64::from(parts.minute);
        if !self.month.contains(&parts.month) || !self.day_matches(parts) {
            // dom and dow are an OR, which makes a whole day the atom here: a
            // skip by dom alone would step over days the dow half accepts.
            return 24 * 60 - into_day;
        }
        if !self.hour.contains(&parts.hour) {
            let next = self
                .hour
                .iter()
                .copied()
                .find(|hour| *hour > parts.hour)
                .map_or(24, i64::from);
            return next * 60 - into_day;
        }
        let next = self
            .minute
            .iter()
            .copied()
            .find(|minute| *minute > parts.minute)
            .map_or(60, i64::from);
        next - i64::from(parts.minute)
    }

    pub fn human_schedule(&self, timezone: SchedulerTimeZone) -> String {
        let fields: Vec<&str> = self.source.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields.as_slice() else {
            return self.source.clone();
        };
        if *hour == "*" && *dom == "*" && *month == "*" && *dow == "*" {
            if *minute == "*" {
                return "Every minute".into();
            }
            if let Some(step) = minute.strip_prefix("*/") {
                if step == "1" {
                    return "Every minute".into();
                }
                return format!("Every {step} minutes");
            }
            if minute.chars().all(|value| value.is_ascii_digit()) {
                if *minute == "0" {
                    return "Every hour".into();
                }
                return format!("Every hour at :{minute:0>2}");
            }
        }
        if minute.chars().all(|value| value.is_ascii_digit())
            && hour.chars().all(|value| value.is_ascii_digit())
            && *dom == "*"
            && *month == "*"
            && *dow == "*"
        {
            let minute = minute.parse::<u8>().unwrap_or_default();
            let hour = hour.parse::<u8>().unwrap_or_default();
            let suffix = match timezone {
                SchedulerTimeZone::Named(Tz::UTC) => " UTC",
                _ => "",
            };
            return format!("Every day at {hour}:{minute:02}{suffix}");
        }
        self.source.clone()
    }

    fn matches(&self, parts: LocalParts) -> bool {
        self.minute.contains(&parts.minute)
            && self.hour.contains(&parts.hour)
            && self.month.contains(&parts.month)
            && self.day_matches(parts)
    }

    /// Vixie cron's day rule: two unrestricted fields match every day, one
    /// unrestricted field defers to the other, two restricted fields are an OR.
    fn day_matches(&self, parts: LocalParts) -> bool {
        let dom_all = self.day_of_month.len() == 31;
        let dow_all = self.day_of_week.len() == 7;
        let dom = self.day_of_month.contains(&parts.day_of_month);
        let dow = self.day_of_week.contains(&parts.day_of_week);
        match (dom_all, dow_all) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
}

fn parse_field(source: &str, min: u8, max: u8, day_of_week: bool) -> Result<Vec<u8>> {
    let mut values = HashSet::new();
    for item in source.split(',') {
        if item.is_empty() {
            bail!("invalid cron field '{source}'");
        }
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => {
                if step.contains('/') {
                    bail!("invalid cron field '{source}'");
                }
                let step = step
                    .parse::<u8>()
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| anyhow!("invalid cron field '{source}'"))?;
                (base, step)
            }
            None => (item, 1),
        };
        let upper = if day_of_week { 7 } else { max };
        let (start, end) = if base == "*" {
            (min, upper)
        } else if let Some((start, end)) = base.split_once('-') {
            let start = start
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            let end = end
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            if start > end {
                bail!("invalid cron field '{source}'");
            }
            (start, end)
        } else {
            if item.contains('/') {
                bail!("invalid cron field '{source}'");
            }
            let value = base
                .parse::<u8>()
                .map_err(|_| anyhow!("invalid cron field '{source}'"))?;
            (value, value)
        };
        if start < min || end > upper {
            bail!("invalid cron field '{source}'");
        }
        let mut value = start;
        loop {
            values.insert(if day_of_week && value == 7 { 0 } else { value });
            let Some(next) = value.checked_add(step) else {
                break;
            };
            if next > end {
                break;
            }
            value = next;
        }
    }
    if values.is_empty() {
        bail!("invalid cron field '{source}'");
    }
    let mut values: Vec<u8> = values.into_iter().collect();
    values.sort_unstable();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::utc_ms;
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn cron_parser_matches_exact_field_contract() {
        let spec = CronSpec::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(spec.minute, vec![0, 15, 30, 45]);
        assert_eq!(spec.hour, (9..=17).collect::<Vec<_>>());
        assert_eq!(spec.day_of_week, vec![1, 2, 3, 4, 5]);
        assert!(CronSpec::parse("* * * *").is_err());
        assert!(CronSpec::parse("*/0 * * * *").is_err());
        assert!(CronSpec::parse("0 0 * JAN *").is_err());
        assert_eq!(CronSpec::parse("0 0 * * 7").unwrap().day_of_week, vec![0]);
    }

    #[test]
    fn dom_and_dow_use_cron_or_semantics() {
        let tz = SchedulerTimeZone::Named(Tz::UTC);
        let spec = CronSpec::parse("0 0 13 * 1").unwrap();
        let after = utc_ms(2026, 4, 12, 23, 59);
        assert_eq!(spec.next_after(after, tz), Some(utc_ms(2026, 4, 13, 0, 0)));
        let after = utc_ms(2026, 4, 13, 0, 0);
        assert_eq!(spec.next_after(after, tz), Some(utc_ms(2026, 4, 20, 0, 0)));
    }

    #[test]
    fn timezone_scan_handles_dst_gap_and_fold() {
        let tz = SchedulerTimeZone::named("America/New_York").unwrap();
        let gap = CronSpec::parse("30 2 * * *").unwrap();
        let before_gap = utc_ms(2026, 3, 8, 5, 0);
        assert_eq!(
            gap.next_after(before_gap, tz),
            Some(utc_ms(2026, 3, 9, 6, 30))
        );

        let fold = CronSpec::parse("30 1 * * *").unwrap();
        let before_fold = utc_ms(2026, 11, 1, 4, 0);
        let first = fold.next_after(before_fold, tz).unwrap();
        let second = fold.next_after(first, tz).unwrap();
        assert_eq!(first, utc_ms(2026, 11, 1, 5, 30));
        assert_eq!(second, utc_ms(2026, 11, 1, 6, 30));
    }

    /// The oracle the skipping scan has to agree with: the minute-by-minute walk
    /// `next_after` used to be. Test-only — the production path has no fallback.
    fn naive_next_after(
        spec: &CronSpec,
        after_ms: i64,
        timezone: SchedulerTimeZone,
    ) -> Option<i64> {
        let mut candidate = after_ms
            .div_euclid(MINUTE_MS)
            .saturating_add(1)
            .saturating_mul(MINUTE_MS);
        for _ in 0..MAX_SCAN_MINUTES {
            if spec.matches(timezone.local_parts(candidate).ok()?) {
                return Some(candidate);
            }
            candidate = candidate.saturating_add(MINUTE_MS);
        }
        None
    }

    #[test]
    fn field_skipping_agrees_with_a_minute_by_minute_scan() {
        // Each zone is paired with its own transitions: New York shifts a whole
        // hour, Lord Howe only thirty minutes, which is what a skip measured in
        // local fields gets wrong if it ignores the offset.
        let anchors = [
            (
                SchedulerTimeZone::Named(Tz::UTC),
                utc_ms(2026, 6, 17, 9, 12),
            ),
            (
                SchedulerTimeZone::named("America/New_York").unwrap(),
                utc_ms(2026, 3, 8, 4, 0),
            ),
            (
                SchedulerTimeZone::named("America/New_York").unwrap(),
                utc_ms(2026, 11, 1, 3, 0),
            ),
            (
                SchedulerTimeZone::named("Australia/Lord_Howe").unwrap(),
                utc_ms(2026, 4, 4, 14, 0),
            ),
            (
                SchedulerTimeZone::named("Australia/Lord_Howe").unwrap(),
                utc_ms(2026, 10, 3, 14, 0),
            ),
        ];
        let cases = [
            ("*/15 * * * *", 40),
            ("0 9 * * 1-5", 8),
            ("0 0 * * 0", 3),
            ("30 3 1 * *", 2),
            ("0 0 13 * 5", 6),
        ];
        for (timezone, anchor) in anchors {
            for (source, rounds) in cases {
                let spec = CronSpec::parse(source).unwrap();
                let mut after = anchor;
                for _ in 0..rounds {
                    let expected = naive_next_after(&spec, after, timezone);
                    assert_eq!(
                        spec.next_after(after, timezone),
                        expected,
                        "{source} after {after} in {timezone:?}"
                    );
                    let Some(next) = expected else { break };
                    after = next;
                }
            }
        }

        // Scans that span most of the window, including one that never matches.
        let timezone = SchedulerTimeZone::Named(Tz::UTC);
        for (source, after) in [
            ("0 0 1 3 *", utc_ms(2026, 1, 15, 0, 0)),
            ("0 0 29 2 *", utc_ms(2027, 3, 1, 0, 0)),
            ("0 0 30 2 *", utc_ms(2026, 1, 1, 0, 0)),
        ] {
            let spec = CronSpec::parse(source).unwrap();
            assert_eq!(
                spec.next_after(after, timezone),
                naive_next_after(&spec, after, timezone),
                "{source} after {after}"
            );
        }
    }

    #[test]
    fn month_end_and_leap_year_are_scanned() {
        let tz = SchedulerTimeZone::Named(Tz::UTC);
        let leap = CronSpec::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            leap.next_after(utc_ms(2027, 3, 1, 0, 0), tz),
            Some(utc_ms(2028, 2, 29, 0, 0))
        );
        let impossible = CronSpec::parse("0 0 31 2 *").unwrap();
        assert_eq!(impossible.next_after(utc_ms(2026, 1, 1, 0, 0), tz), None);
        assert!(NaiveDate::from_ymd_opt(2028, 2, 29).is_some());
    }
}

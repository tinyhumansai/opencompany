//! A dependency-light 5-field cron matcher.
//!
//! Parses a standard `minute hour day-of-month month day-of-week` expression
//! into a [`CronExpr`], then answers two questions against a wall-clock-free
//! [`CivilTime`]:
//!
//! - [`CronExpr::matches`] — does this minute satisfy the schedule?
//! - [`CronExpr::next_after`] — what is the next minute that does?
//!
//! No `chrono`/`cron` dependency: civil-time conversion is hand-rolled
//! (days-from-epoch, Howard Hinnant's algorithms) so the whole matcher is std
//! only and trivially testable with a fake clock. Every field supports `*`,
//! comma lists, `a-b` ranges, `*/step` (and `a-b/step`), and — for the month
//! and weekday fields — the usual three-letter names (`JAN`…`DEC`,
//! `SUN`…`SAT`). Day-of-week accepts both `0` and `7` for Sunday.
//!
//! Day-of-month and day-of-week combine with cron's historical "or" rule: when
//! *both* fields are restricted (neither is `*`), a day matches if it satisfies
//! *either* field; otherwise the two combine with the usual "and".

use crate::Result;
use crate::error::OpenCompanyError;

/// Milliseconds in one minute.
const MINUTE_MS: u64 = 60_000;
/// Milliseconds in one day.
const DAY_MS: u64 = 86_400_000;
/// Upper bound on the minute-by-minute search in [`CronExpr::next_after`].
///
/// Four years (with a leap day) is the widest gap any valid 5-field expression
/// can have between fire times (e.g. `0 0 29 2 *`), so a match is guaranteed
/// inside this window when one exists.
const NEXT_SEARCH_LIMIT_MINUTES: u64 = 4 * 366 * 24 * 60;

/// The set of permitted values for one cron field, as a 64-bit mask.
///
/// Bit `v` is set when value `v` is allowed. Every cron field value fits in
/// `0..=59`, well inside 64 bits. `restricted` records whether the source field
/// was anything other than `*`, which the day-of-month/day-of-week "or" rule
/// depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FieldSet {
    mask: u64,
    restricted: bool,
}

impl FieldSet {
    /// Whether `value` is a member of this set.
    fn contains(&self, value: u32) -> bool {
        value < 64 && self.mask & (1u64 << value) != 0
    }
}

/// A parsed 5-field cron expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpr {
    minute: FieldSet,
    hour: FieldSet,
    dom: FieldSet,
    month: FieldSet,
    dow: FieldSet,
}

impl CronExpr {
    /// Parses a standard 5-field cron expression.
    ///
    /// Returns [`OpenCompanyError::InvalidRequest`] with a prosumer-readable
    /// message when the expression does not have exactly five fields or a field
    /// is malformed or out of range.
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "cron `{expr}` needs 5 fields (minute hour day month weekday), found {}",
                fields.len()
            )));
        }
        Ok(Self {
            minute: parse_field(fields[0], 0, 59, &[])?,
            hour: parse_field(fields[1], 0, 23, &[])?,
            dom: parse_field(fields[2], 1, 31, &[])?,
            month: parse_field(fields[3], 1, 12, MONTHS)?,
            dow: parse_dow(fields[4])?,
        })
    }

    /// Whether `t` (truncated to the minute) satisfies this schedule.
    pub fn matches(&self, t: &CivilTime) -> bool {
        if !self.minute.contains(t.minute)
            || !self.hour.contains(t.hour)
            || !self.month.contains(t.month)
        {
            return false;
        }
        // Cron's day rule: OR the two day fields when both are restricted.
        let dom_hit = self.dom.contains(t.day);
        let dow_hit = self.dow.contains(t.weekday);
        if self.dom.restricted && self.dow.restricted {
            dom_hit || dow_hit
        } else {
            dom_hit && dow_hit
        }
    }

    /// The next minute strictly after `t` that satisfies this schedule.
    ///
    /// Searches minute-by-minute over a bounded window (four leap years), so it
    /// returns `None` only for an expression that can never fire (impossible for
    /// a value that parsed successfully). Purely arithmetic — no wall clock.
    pub fn next_after(&self, t: &CivilTime) -> Option<CivilTime> {
        let start = t.floor_to_minute_millis();
        let mut cursor = start + MINUTE_MS;
        for _ in 0..NEXT_SEARCH_LIMIT_MINUTES {
            let candidate = CivilTime::from_unix_millis(cursor);
            if self.matches(&candidate) {
                return Some(candidate);
            }
            cursor += MINUTE_MS;
        }
        None
    }
}

/// Three-letter month names, index 0 = `JAN` (value 1).
const MONTHS: &[&str] = &[
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
/// Three-letter weekday names, index 0 = `SUN` (value 0).
const WEEKDAYS: &[&str] = &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// Parses the day-of-week field, normalizing `7` (and `SUN`) to `0`.
fn parse_dow(spec: &str) -> Result<FieldSet> {
    // Accept 0..=7 on input, then fold bit 7 down onto bit 0 (both = Sunday) so
    // matching against a `0..=6` civil weekday works uniformly.
    let mut set = parse_field(spec, 0, 7, WEEKDAYS)?;
    if set.mask & (1u64 << 7) != 0 {
        set.mask &= !(1u64 << 7);
        set.mask |= 1u64;
    }
    Ok(set)
}

/// Parses one cron field into a [`FieldSet`] bounded by `[min, max]`.
///
/// `names` maps three-letter aliases to values (`names[i]` = `min + i`); it is
/// empty for the purely numeric fields.
fn parse_field(spec: &str, min: u32, max: u32, names: &[&str]) -> Result<FieldSet> {
    let mut mask = 0u64;
    // A field is "unrestricted" only when it is exactly `*`; any list, range, or
    // step restricts it. This flag drives the day-of-month/day-of-week or-rule.
    let restricted = spec.trim() != "*";
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(field_error(spec, "an empty item"));
        }
        // Split off an optional `/step`.
        let (range_spec, step) = match part.split_once('/') {
            Some((range, step_str)) => {
                let step: u32 = step_str
                    .parse()
                    .map_err(|_| field_error(spec, "a non-numeric step"))?;
                if step == 0 {
                    return Err(field_error(spec, "a zero step"));
                }
                (range, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range_spec == "*" {
            (min, max)
        } else if let Some((a, b)) = range_spec.split_once('-') {
            (
                resolve_value(a, min, max, names, spec)?,
                resolve_value(b, min, max, names, spec)?,
            )
        } else {
            let value = resolve_value(range_spec, min, max, names, spec)?;
            (value, value)
        };
        if lo > hi {
            return Err(field_error(spec, "a descending range"));
        }
        let mut value = lo;
        while value <= hi {
            mask |= 1u64 << value;
            value += step;
        }
    }
    if mask == 0 {
        return Err(field_error(spec, "no values"));
    }
    Ok(FieldSet { mask, restricted })
}

/// Resolves a single numeric-or-named token to a value inside `[min, max]`.
fn resolve_value(token: &str, min: u32, max: u32, names: &[&str], spec: &str) -> Result<u32> {
    let token = token.trim();
    let value = if let Some(idx) = names.iter().position(|n| n.eq_ignore_ascii_case(token)) {
        min + idx as u32
    } else {
        token
            .parse::<u32>()
            .map_err(|_| field_error(spec, "an unrecognized value"))?
    };
    if value < min || value > max {
        return Err(field_error(spec, "a value out of range"));
    }
    Ok(value)
}

/// Builds a uniform field-parse error.
fn field_error(spec: &str, why: &str) -> OpenCompanyError {
    OpenCompanyError::InvalidRequest(format!("cron field `{spec}` has {why}"))
}

/// A minute-granular civil (UTC) timestamp derived from unix milliseconds.
///
/// Carries the pre-computed weekday (`0` = Sunday) so the matcher never touches
/// a wall clock. Constructed with [`CivilTime::from_unix_millis`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilTime {
    /// Proleptic Gregorian year (e.g. `2026`).
    pub year: i64,
    /// Month of year, `1`–`12`.
    pub month: u32,
    /// Day of month, `1`–`31`.
    pub day: u32,
    /// Hour of day, `0`–`23`.
    pub hour: u32,
    /// Minute of hour, `0`–`59`.
    pub minute: u32,
    /// Day of week, `0` = Sunday … `6` = Saturday.
    pub weekday: u32,
}

impl CivilTime {
    /// Converts unix epoch milliseconds (UTC) into civil fields.
    pub fn from_unix_millis(ms: u64) -> Self {
        let days = (ms / DAY_MS) as i64;
        let rem = ms % DAY_MS;
        let hour = (rem / 3_600_000) as u32;
        let minute = ((rem % 3_600_000) / MINUTE_MS) as u32;
        // Epoch day 0 (1970-01-01) is a Thursday; `(days + 4) % 7` maps to the
        // Sunday-indexed weekday. `days` is non-negative for any real timestamp.
        let weekday = ((days % 7 + 4) % 7) as u32;
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour,
            minute,
            weekday,
        }
    }

    /// Unix milliseconds at the start of this civil minute.
    fn floor_to_minute_millis(&self) -> u64 {
        let days = days_from_civil(self.year, self.month, self.day);
        (days as u64) * DAY_MS + (self.hour as u64) * 3_600_000 + (self.minute as u64) * MINUTE_MS
    }
}

/// Civil date from a days-since-epoch count (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// Days-since-epoch from a civil date (inverse of [`civil_from_days`]).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod test {
    use super::*;

    /// Builds a `CivilTime` from a `YYYY-MM-DD HH:MM` UTC literal via its unix
    /// millis, exercising the same path the scheduler uses.
    fn at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> CivilTime {
        let ms = ((days_from_civil(year, month, day) as u64) * DAY_MS)
            + (hour as u64) * 3_600_000
            + (minute as u64) * MINUTE_MS;
        CivilTime::from_unix_millis(ms)
    }

    #[test]
    fn epoch_is_a_thursday() {
        let t = CivilTime::from_unix_millis(0);
        assert_eq!((t.year, t.month, t.day), (1970, 1, 1));
        assert_eq!(t.weekday, 4); // Thursday
        assert_eq!((t.hour, t.minute), (0, 0));
    }

    #[test]
    fn known_weekdays_round_trip() {
        // 2026-07-10 is a Friday (weekday 5).
        let friday = at(2026, 7, 10, 12, 0);
        assert_eq!(friday.weekday, 5);
        // 2000-01-01 was a Saturday (weekday 6).
        assert_eq!(at(2000, 1, 1, 0, 0).weekday, 6);
        // 2024-02-29 (leap day) was a Thursday (weekday 4).
        assert_eq!(at(2024, 2, 29, 0, 0).weekday, 4);
    }

    #[test]
    fn every_field_wildcard_matches_any_minute() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        assert!(expr.matches(&at(2026, 7, 10, 3, 27)));
        assert!(expr.matches(&at(1999, 12, 31, 23, 59)));
    }

    #[test]
    fn step_minute_matches_quarter_hours() {
        let expr = CronExpr::parse("*/15 * * * *").unwrap();
        for minute in [0, 15, 30, 45] {
            assert!(expr.matches(&at(2026, 7, 10, 9, minute)), "minute {minute}");
        }
        for minute in [1, 14, 16, 29, 44, 59] {
            assert!(
                !expr.matches(&at(2026, 7, 10, 9, minute)),
                "minute {minute}"
            );
        }
    }

    #[test]
    fn named_weekday_matches_only_that_day() {
        // 09:00 every Monday. 2026-07-13 is a Monday; 2026-07-14 a Tuesday.
        let expr = CronExpr::parse("0 9 * * MON").unwrap();
        assert!(expr.matches(&at(2026, 7, 13, 9, 0)));
        assert!(!expr.matches(&at(2026, 7, 14, 9, 0)));
        assert!(!expr.matches(&at(2026, 7, 13, 10, 0)));
    }

    #[test]
    fn sunday_accepts_zero_and_seven() {
        let zero = CronExpr::parse("0 0 * * 0").unwrap();
        let seven = CronExpr::parse("0 0 * * 7").unwrap();
        // 2026-07-12 is a Sunday.
        let sunday = at(2026, 7, 12, 0, 0);
        assert!(zero.matches(&sunday));
        assert!(seven.matches(&sunday));
        assert_eq!(zero, seven);
    }

    #[test]
    fn day_of_month_field() {
        let expr = CronExpr::parse("0 0 1 * *").unwrap();
        assert!(expr.matches(&at(2026, 3, 1, 0, 0)));
        assert!(!expr.matches(&at(2026, 3, 2, 0, 0)));
    }

    #[test]
    fn named_month_range_and_business_hours() {
        // 09:00 and 17:00 on weekdays (Mon–Fri).
        let expr = CronExpr::parse("0 9,17 * * 1-5").unwrap();
        assert!(expr.matches(&at(2026, 7, 10, 9, 0))); // Friday 09:00
        assert!(expr.matches(&at(2026, 7, 10, 17, 0))); // Friday 17:00
        assert!(!expr.matches(&at(2026, 7, 11, 9, 0))); // Saturday
        assert!(!expr.matches(&at(2026, 7, 10, 12, 0))); // midday, not on the hour list

        let named = CronExpr::parse("0 0 * JAN-MAR *").unwrap();
        assert!(named.matches(&at(2026, 2, 15, 0, 0)));
        assert!(!named.matches(&at(2026, 4, 1, 0, 0)));
    }

    #[test]
    fn day_or_rule_when_both_restricted() {
        // "Fire on the 1st OR on any Monday." 2026-07-13 is a Monday but not the
        // 1st; 2026-07-01 is the 1st (a Wednesday).
        let expr = CronExpr::parse("0 0 1 * MON").unwrap();
        assert!(expr.matches(&at(2026, 7, 13, 0, 0))); // Monday
        assert!(expr.matches(&at(2026, 7, 1, 0, 0))); // the 1st
        assert!(!expr.matches(&at(2026, 7, 8, 0, 0))); // neither
    }

    #[test]
    fn next_after_finds_the_following_tick() {
        let expr = CronExpr::parse("*/15 * * * *").unwrap();
        let next = expr.next_after(&at(2026, 7, 10, 9, 7)).unwrap();
        assert_eq!((next.hour, next.minute), (9, 15));

        let weekly = CronExpr::parse("0 9 * * MON").unwrap();
        // From Friday, the next Monday 09:00 is 2026-07-13.
        let next = weekly.next_after(&at(2026, 7, 10, 9, 0)).unwrap();
        assert_eq!((next.year, next.month, next.day), (2026, 7, 13));
        assert_eq!((next.hour, next.minute), (9, 0));
    }

    #[test]
    fn next_after_crosses_the_leap_day() {
        let expr = CronExpr::parse("0 0 29 2 *").unwrap();
        // From 2025 (not a leap year) the next 29 Feb is 2028.
        let next = expr.next_after(&at(2025, 3, 1, 0, 0)).unwrap();
        assert_eq!((next.year, next.month, next.day), (2028, 2, 29));
    }

    #[test]
    fn rejects_malformed_expressions() {
        assert!(CronExpr::parse("* * * *").is_err()); // 4 fields
        assert!(CronExpr::parse("* * * * * *").is_err()); // 6 fields
        assert!(CronExpr::parse("60 * * * *").is_err()); // minute out of range
        assert!(CronExpr::parse("* 24 * * *").is_err()); // hour out of range
        assert!(CronExpr::parse("*/0 * * * *").is_err()); // zero step
        assert!(CronExpr::parse("5-1 * * * *").is_err()); // descending range
        assert!(CronExpr::parse("* * * FOO *").is_err()); // bad month name
    }
}

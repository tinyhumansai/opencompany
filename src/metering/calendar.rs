//! Minimal proleptic-Gregorian date math over epoch millis — no `chrono`/`time`
//! dependency. Uses Howard Hinnant's public-domain `civil_from_days` /
//! `days_from_civil` algorithms so the metering projections can label daily
//! buckets and find the current-month boundary offline.
//!
//! All arithmetic is UTC. "Epoch day" is the count of whole days since
//! 1970-01-01.

/// Milliseconds in one UTC day.
pub const MILLIS_PER_DAY: u64 = 86_400_000;

/// The epoch day (days since 1970-01-01, UTC) an instant falls on.
pub fn epoch_day(at_millis: u64) -> i64 {
    (at_millis / MILLIS_PER_DAY) as i64
}

/// The `(year, month, day)` of an epoch day, using Hinnant's `civil_from_days`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// The epoch day for a civil `(year, month, day)`, using Hinnant's
/// `days_from_civil`.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The ISO `YYYY-MM-DD` label for an epoch day.
pub fn iso_day(epoch_day: i64) -> String {
    let (y, m, d) = civil_from_days(epoch_day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The epoch-millis start (00:00 UTC on the 1st) of the month an instant falls
/// in.
pub fn month_start_millis(at_millis: u64) -> u64 {
    let (y, m, _) = civil_from_days(epoch_day(at_millis));
    let first = days_from_civil(y, m, 1);
    (first as u64) * MILLIS_PER_DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_1970_01_01() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(iso_day(0), "1970-01-01");
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn known_dates_round_trip() {
        // 2026-07-16 — the project's "today".
        let d = days_from_civil(2026, 7, 16);
        assert_eq!(civil_from_days(d), (2026, 7, 16));
        assert_eq!(iso_day(d), "2026-07-16");
    }

    #[test]
    fn iso_day_from_millis_matches() {
        // 2021-01-01T00:00:00Z = 1_609_459_200_000 ms.
        assert_eq!(iso_day(epoch_day(1_609_459_200_000)), "2021-01-01");
        // A time later that same day stays on the same bucket.
        assert_eq!(
            iso_day(epoch_day(1_609_459_200_000 + 23 * 3_600_000)),
            "2021-01-01"
        );
    }

    #[test]
    fn month_start_snaps_to_the_first() {
        // 2026-07-16T12:00:00Z.
        let mid_month = (days_from_civil(2026, 7, 16) as u64) * MILLIS_PER_DAY + 12 * 3_600_000;
        let start = month_start_millis(mid_month);
        assert_eq!(iso_day(epoch_day(start)), "2026-07-01");
        // The start of the month maps to itself.
        assert_eq!(month_start_millis(start), start);
    }

    #[test]
    fn leap_day_is_valid() {
        let d = days_from_civil(2024, 2, 29);
        assert_eq!(civil_from_days(d), (2024, 2, 29));
    }
}

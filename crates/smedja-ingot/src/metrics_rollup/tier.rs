use chrono::{DateTime, Datelike, Duration, TimeZone as _, Timelike, Utc};
use serde::{Deserialize, Serialize};

/// Microseconds in one second.
pub(crate) const MICROS_PER_SEC: i64 = 1_000_000;

/// A fixed time tier the rollup aggregates over.
///
/// Each tier truncates a microsecond timestamp to the start of its grid cell in
/// UTC via [`RollupTier::bucket_start`]; `Raw` keeps per-source granularity (no
/// truncation). Callers pass a tier, never raw SQL or arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RollupTier {
    /// Per-source granularity — the timestamp is its own bucket.
    Raw,
    /// Start of the hour (UTC).
    Hourly,
    /// Start of the day (00:00 UTC).
    Daily,
    /// ISO-week start (Monday 00:00 UTC).
    Weekly,
    /// First of the month (00:00 UTC).
    Monthly,
}

impl RollupTier {
    /// Returns the tier's wire string (`"raw"`, `"hourly"`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    /// Parses a tier from its wire string, returning `None` for any other input.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "raw" => Some(Self::Raw),
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }

    /// Truncates `ts_micros` (microseconds since the Unix epoch) to the start of
    /// this tier's grid cell, in UTC, returning the bucket start in microseconds.
    ///
    /// `Raw` returns the input unchanged. `Hourly`/`Daily` floor to the start of
    /// the hour/day; `Weekly` floors to the ISO-week start (Monday 00:00 UTC);
    /// `Monthly` floors to the first of the month (00:00 UTC). An instant that
    /// already sits exactly on a boundary maps to itself.
    #[must_use]
    pub fn bucket_start(self, ts_micros: i64) -> i64 {
        if self == Self::Raw {
            return ts_micros;
        }
        // Split into whole seconds and the sub-second microsecond remainder so
        // calendar math runs on a chrono DateTime, then re-attach zero micros.
        let secs = ts_micros.div_euclid(MICROS_PER_SEC);
        let dt: DateTime<Utc> = Utc
            .timestamp_opt(secs, 0)
            .single()
            .unwrap_or_else(|| Utc.timestamp_nanos(0));
        let truncated = match self {
            // Raw handled above; listing it keeps the match exhaustive.
            Self::Raw => dt,
            Self::Hourly => dt
                .with_minute(0)
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .unwrap_or(dt),
            Self::Daily => start_of_day(dt),
            Self::Weekly => {
                let days_since_monday = i64::from(dt.weekday().num_days_from_monday());
                start_of_day(dt) - Duration::days(days_since_monday)
            }
            Self::Monthly => Utc
                .with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
                .single()
                .unwrap_or_else(|| start_of_day(dt)),
        };
        truncated.timestamp() * MICROS_PER_SEC
    }
}

/// Returns `dt` floored to 00:00:00.000000 of the same UTC day.
fn start_of_day(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.with_hour(0)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the micros for a UTC `Y-M-D h:m:s` instant.
    fn micros(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid instant")
            .timestamp()
            * MICROS_PER_SEC
    }

    #[test]
    fn raw_tier_is_identity() {
        let ts = micros(2026, 6, 24, 12, 34, 56);
        assert_eq!(RollupTier::Raw.bucket_start(ts), ts);
        // Including sub-second micros.
        assert_eq!(RollupTier::Raw.bucket_start(ts + 789), ts + 789);
    }

    #[test]
    fn hourly_floors_to_start_of_hour() {
        let ts = micros(2026, 6, 24, 12, 34, 56) + 789;
        assert_eq!(
            RollupTier::Hourly.bucket_start(ts),
            micros(2026, 6, 24, 12, 0, 0)
        );
        // Boundary instant maps to itself.
        let boundary = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(RollupTier::Hourly.bucket_start(boundary), boundary);
    }

    #[test]
    fn daily_floors_to_midnight() {
        let ts = micros(2026, 6, 24, 23, 59, 59);
        assert_eq!(
            RollupTier::Daily.bucket_start(ts),
            micros(2026, 6, 24, 0, 0, 0)
        );
        let boundary = micros(2026, 6, 24, 0, 0, 0);
        assert_eq!(RollupTier::Daily.bucket_start(boundary), boundary);
    }

    #[test]
    fn weekly_floors_to_iso_monday() {
        // 2026-06-24 is a Wednesday; its ISO week starts Monday 2026-06-22.
        let wednesday = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(
            RollupTier::Weekly.bucket_start(wednesday),
            micros(2026, 6, 22, 0, 0, 0)
        );
        // A Monday 00:00 boundary maps to itself (no jump to the previous week).
        let monday = micros(2026, 6, 22, 0, 0, 0);
        assert_eq!(RollupTier::Weekly.bucket_start(monday), monday);
        // Sunday is the last day of the same ISO week.
        let sunday = micros(2026, 6, 28, 23, 0, 0);
        assert_eq!(
            RollupTier::Weekly.bucket_start(sunday),
            micros(2026, 6, 22, 0, 0, 0)
        );
    }

    #[test]
    fn monthly_floors_to_first_of_month() {
        let ts = micros(2026, 6, 24, 12, 0, 0);
        assert_eq!(
            RollupTier::Monthly.bucket_start(ts),
            micros(2026, 6, 1, 0, 0, 0)
        );
        let boundary = micros(2026, 6, 1, 0, 0, 0);
        assert_eq!(RollupTier::Monthly.bucket_start(boundary), boundary);
        // Month length edge: 2026-02-28 floors to 2026-02-01.
        let feb = micros(2026, 2, 28, 23, 59, 59);
        assert_eq!(
            RollupTier::Monthly.bucket_start(feb),
            micros(2026, 2, 1, 0, 0, 0)
        );
    }

    #[test]
    fn tier_str_round_trips() {
        for tier in [
            RollupTier::Raw,
            RollupTier::Hourly,
            RollupTier::Daily,
            RollupTier::Weekly,
            RollupTier::Monthly,
        ] {
            assert_eq!(RollupTier::parse(tier.as_str()), Some(tier));
        }
        assert_eq!(RollupTier::parse("yearly"), None);
    }
}

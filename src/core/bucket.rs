//! Time bucketing for reports. A pure mapping from a UTC instant to a local
//! bucket label, with the timezone injected so tests stay deterministic and the
//! store can keep UTC (`docs/specs/cli.md`, `storage.md`).

use chrono::{FixedOffset, TimeZone};

/// JST is UTC+9. The default presentation zone (`docs/specs/cli.md`).
pub const JST_OFFSET_SECS: i32 = 9 * 3600;

/// The granularity of a time bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Year,
    Month,
    Week,
    Day,
    Hour,
}

impl Bucket {
    /// Parse a `--by` value. Returns `None` for an unrecognised granularity.
    pub fn parse(value: &str) -> Option<Bucket> {
        match value {
            "year" => Some(Bucket::Year),
            "month" => Some(Bucket::Month),
            "week" => Some(Bucket::Week),
            "day" => Some(Bucket::Day),
            "hour" => Some(Bucket::Hour),
            _ => None,
        }
    }
}

/// The bucket label for a UTC `epoch_secs`, evaluated in the given timezone
/// offset. Day/hour buckets reflect the *local* day/hour, so an event near
/// midnight UTC lands in the correct local day.
pub fn bucket_label(epoch_secs: i64, bucket: Bucket, tz_offset_secs: i32) -> String {
    let offset = FixedOffset::east_opt(tz_offset_secs).expect("valid tz offset");
    let local = offset.timestamp_opt(epoch_secs, 0).single();
    let Some(local) = local else {
        return String::new();
    };
    let pattern = match bucket {
        Bucket::Year => "%Y",
        Bucket::Month => "%Y-%m",
        Bucket::Week => "%G-W%V", // ISO year + ISO week number
        Bucket::Day => "%Y-%m-%d",
        Bucket::Hour => "%Y-%m-%d %H:00",
    };
    local.format(pattern).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Epoch 0 is 1970-01-01 00:00:00 UTC, which is 1970-01-01 09:00 JST.
    const EPOCH_ZERO: i64 = 0;

    #[test]
    fn buckets_epoch_zero_in_jst() {
        assert_eq!(
            bucket_label(EPOCH_ZERO, Bucket::Year, JST_OFFSET_SECS),
            "1970"
        );
        assert_eq!(
            bucket_label(EPOCH_ZERO, Bucket::Month, JST_OFFSET_SECS),
            "1970-01"
        );
        assert_eq!(
            bucket_label(EPOCH_ZERO, Bucket::Day, JST_OFFSET_SECS),
            "1970-01-01"
        );
        assert_eq!(
            bucket_label(EPOCH_ZERO, Bucket::Hour, JST_OFFSET_SECS),
            "1970-01-01 09:00"
        );
    }

    #[test]
    fn local_day_differs_from_utc_day_across_the_boundary() {
        // 1970-01-01 20:00 UTC is 1970-01-02 05:00 JST — the JST day is the 2nd.
        let twenty_hundred_utc = 20 * 3600;
        assert_eq!(
            bucket_label(twenty_hundred_utc, Bucket::Day, JST_OFFSET_SECS),
            "1970-01-02"
        );
        // In UTC (offset 0) the same instant is still the 1st.
        assert_eq!(
            bucket_label(twenty_hundred_utc, Bucket::Day, 0),
            "1970-01-01"
        );
    }

    #[test]
    fn parse_accepts_known_granularities_only() {
        assert_eq!(Bucket::parse("month"), Some(Bucket::Month));
        assert_eq!(Bucket::parse("hour"), Some(Bucket::Hour));
        assert_eq!(Bucket::parse("fortnight"), None);
    }
}

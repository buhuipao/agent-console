//! Both providers stamp transcript lines with an RFC 3339 UTC string
//! (`2026-08-21T15:03:20.435Z`), but the web contract carries `ts` as Unix seconds. This is
//! the whole of the conversion -- the crate has no date-time dependency and one field in one
//! known format does not justify adding one.

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff][Z]` into Unix seconds. Fractional seconds and any
/// trailing zone marker are ignored: provider transcripts are always UTC.
pub(crate) fn parse_rfc3339_seconds(value: &str) -> Option<u64> {
    let (date, rest) = value.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year = date.next()?.parse::<i64>().ok()?;
    let month = date.next()?.parse::<i64>().ok()?;
    let day = date.next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let time = rest
        .split(['Z', 'z', '+'])
        .next()
        .unwrap_or(rest)
        .split_once('.')
        .map_or(rest, |(seconds, _)| seconds);
    let mut time = time.splitn(3, ':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let second = time
        .next()
        .map(|second| second.trim_end_matches(['Z', 'z']))
        .and_then(|second| second.parse::<i64>().ok())
        .unwrap_or(0);
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    let total = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(total).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_position = (month + 9) % 12;
    let day_of_year = (153 * month_position + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_timestamps_convert_to_unix_seconds() {
        assert_eq!(parse_rfc3339_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_seconds("2026-08-21T15:03:20.435Z"),
            Some(1_787_324_600)
        );
        // Same instant, no fractional part and no zone marker.
        assert_eq!(
            parse_rfc3339_seconds("2026-08-21T15:03:20"),
            Some(1_787_324_600)
        );
    }

    #[test]
    fn a_leap_day_lands_on_the_right_second() {
        assert_eq!(
            parse_rfc3339_seconds("2024-02-29T00:00:00Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn malformed_or_pre_epoch_timestamps_are_rejected_rather_than_wrapping() {
        assert_eq!(parse_rfc3339_seconds(""), None);
        assert_eq!(parse_rfc3339_seconds("not-a-date"), None);
        assert_eq!(parse_rfc3339_seconds("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_seconds("1969-12-31T23:59:59Z"), None);
    }
}

//! Timestamps as GitHub and Docker write them (RFC 3339 in UTC, such as `2026-10-22T19:27:25Z` or
//! `2026-10-22T19:27:25.233122085Z`), and the clock as runnerd records it. Pure functions apart from `now_ms`.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, now.
pub(crate) fn now_ms() -> u64 {
    ms(SystemTime::now())
}

/// Milliseconds since the Unix epoch of a time.
pub(crate) fn ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Milliseconds since the Unix epoch of a UTC RFC 3339 timestamp (`Z` only, fractions of a second optional);
/// `None` for anything else.
pub(crate) fn parse_ms(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<u64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time = time.splitn(3, ':').map(str::parse::<u64>);
    let (hour, minute, second) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let millis = format!("{fraction:0<3}")[..3].parse::<u64>().ok()?;
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar, for years from 1970 (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::parse_ms;

    #[test]
    fn github_and_docker_timestamps_parse_to_epoch_milliseconds() {
        assert_eq!(parse_ms("1970-01-01T00:00:00Z"), Some(0));
        // `date -u -d 2026-10-22T19:27:25Z +%s`
        assert_eq!(parse_ms("2026-10-22T19:27:25Z"), Some(1_792_697_245_000));
        assert_eq!(
            parse_ms("2026-10-22T19:27:25.233122085Z"),
            Some(1_792_697_245_233)
        );
        assert_eq!(parse_ms("2024-02-29T12:00:00.5Z"), Some(1_709_208_000_500));
        for invalid in [
            "",
            "2026-10-22T19:27:25",
            "2026-13-01T00:00:00Z",
            "yesterday",
        ] {
            assert_eq!(parse_ms(invalid), None, "{invalid}");
        }
    }
}

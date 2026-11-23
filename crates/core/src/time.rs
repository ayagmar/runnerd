//! Timestamps as GitHub and Docker write them (RFC 3339 in UTC, such as `2026-10-22T19:27:25Z` or
//! `2026-10-22T19:27:25.233122085Z`), and the clock as runnerd records it. Pure functions apart from `now_ms` and
//! `suspended_ms`.

#[cfg(feature = "host")]
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, now.
#[cfg(feature = "host")]
pub(crate) fn now_ms() -> u64 {
    ms(SystemTime::now())
}

/// How long the host must have slept between two looks for runnerd to take it as a sleep, in ms: a pause of a few
/// seconds keeps the runners' sessions with GitHub, and its time in the waits is no more than a tick's.
#[cfg(feature = "host")]
pub(crate) const SLEPT_AFTER_MS: u64 = 30_000;

/// How long the host has been suspended since it booted, in ms: `CLOCK_BOOTTIME` less `CLOCK_MONOTONIC`. Linux stops
/// the monotonic clock, which tokio's timers go by, while the host sleeps, and the boot clock goes on; nothing else
/// moves them apart, so this grows by exactly the time the host slept. A step of the wall clock, as a time daemon
/// makes on a laptop that booted with a wrong one, leaves it as it was (ADR 0159).
#[cfg(feature = "host")]
pub(crate) fn suspended_ms() -> u64 {
    use rustix::time::{ClockId, clock_gettime};
    let ms = |clock: ClockId| {
        let time = clock_gettime(clock);
        i128::from(time.tv_sec) * 1000 + i128::from(time.tv_nsec) / 1_000_000
    };
    // Read the monotonic clock last: a read between the two can only make the difference smaller.
    let boot = ms(ClockId::Boottime);
    let monotonic = ms(ClockId::Monotonic);
    u64::try_from(boot.saturating_sub(monotonic)).unwrap_or(0)
}

/// Milliseconds since the Unix epoch of a time.
#[cfg(feature = "host")]
pub(crate) fn ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Milliseconds since the Unix epoch of a UTC RFC 3339 timestamp (`Z` only, fractions of a second optional);
/// `None` for anything else.
pub(crate) fn parse_ms(text: &str) -> Option<u64> {
    parse_ns(text).map(|ns| ns / 1_000_000)
}

/// Nanoseconds since the Unix epoch of a UTC RFC 3339 timestamp, as `parse_ms` takes it; Docker's log timestamps
/// have all nine digits.
pub(crate) fn parse_ns(text: &str) -> Option<u64> {
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
    let nanos = format!("{fraction:0<9}")[..9].parse::<u64>().ok()?;
    let days = crate::api::days_from_civil(year, month, day);
    let seconds = ((days * 24 + hour) * 60 + minute) * 60 + second;
    seconds.checked_mul(1_000_000_000)?.checked_add(nanos)
}

/// `ms` since the epoch as GitHub writes a time, to the second: a test's fixture dated by the clock, so no window
/// that reads back from now ages it out.
#[cfg(test)]
pub(crate) fn rfc3339(ms: u64) -> String {
    let (seconds, of_day) = (ms / 1000, ms / 1000 % 86_400);
    let (year, month, day) = crate::api::civil_from_days(seconds / 86_400);
    let (hour, minute, second) = (of_day / 3600, of_day / 60 % 60, of_day % 60);
    format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::{parse_ms, rfc3339};

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

    #[test]
    fn a_time_written_as_github_does_parses_back_to_its_second() {
        for at in [0, 1_709_208_000_500, 1_792_697_245_233, 4_102_444_799_000] {
            assert_eq!(parse_ms(&rfc3339(at)), Some(at / 1000 * 1000), "{at}");
        }
        assert_eq!(rfc3339(1_792_697_245_233), "2026-10-22T19:27:25Z");
    }
}

//! How the dashboard writes numbers and times: sizes as `runnerd.toml` writes them, durations like `1m 12s`, CPU in
//! cores (a cap in CPUs), dates in UTC. Pure functions of their arguments; the caller passes the current time. Sizes
//! and CPU read as in `runnerctl`; durations are a stopwatch (`2h 05m`) whose width holds as it counts, and ages are
//! rounded (`3 h ago`) for a glance, where the terminal writes both exactly (`2h 5m`, `1h 52m ago`).

use runnerd_core::config::ByteSize;

/// A size with a binary unit, as in `runnerd.toml`: `512M`, `9G`.
pub(crate) fn bytes(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

/// A duration in seconds: `45s`, `1m 12s`, `2h 05m`, `3d 4h`.
pub(crate) fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        3600..86_400 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

/// Millicores as cores: `0.25`, `1.5`, `12`.
#[allow(
    clippy::cast_precision_loss,
    reason = "millicores are far below 2^52; shown with two decimals"
)]
pub(crate) fn cores(millicores: u64) -> String {
    let text = format!("{:.2}", millicores as f64 / 1000.0);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// How long ago `at` was: `just now`, `4 min ago`, `3 h ago`, or the date after a week.
pub(crate) fn ago(at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(at);
    match seconds {
        0..10 => "just now".to_owned(),
        10..60 => format!("{seconds} s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        86_400..604_800 => format!("{} d ago", seconds / 86_400),
        _ => date(at),
    }
}

/// `2026-10-22`, in UTC.
pub(crate) fn date(at: u64) -> String {
    let (year, month, day) = civil(at / 86_400);
    format!("{year}-{month:02}-{day:02}")
}

/// `Oct 22`, in UTC: short labels on chart axes.
pub(crate) fn short_date(at: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, month, day) = civil(at / 86_400);
    let name = usize::try_from(month - 1)
        .ok()
        .and_then(|index| MONTHS.get(index))
        .unwrap_or(&"");
    format!("{name} {day}")
}

/// `14:03`, in UTC.
pub(crate) fn clock(at: u64) -> String {
    let seconds = at % 86_400;
    format!("{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
}

/// `2026-10-22 14:03 UTC`.
pub(crate) fn timestamp(at: u64) -> String {
    format!("{} {} UTC", date(at), clock(at))
}

/// RFC 3339 in UTC, for `<time datetime>`.
pub(crate) fn rfc3339(at: u64) -> String {
    let seconds = at % 86_400;
    format!("{}T{}:{:02}Z", date(at), clock(at), seconds % 60)
}

/// The civil date of a day count since 1970-01-01 (Howard Hinnant's `civil_from_days`, for days after the epoch).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_a_stopwatch() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(72), "1m 12s");
        assert_eq!(duration(2 * 3600 + 5 * 60 + 9), "2h 05m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn dates_are_utc_calendar_days() {
        // 2026-10-22T14:03:07Z.
        let at = 1_792_677_787;
        assert_eq!(timestamp(at), "2026-10-22 14:03 UTC");
        assert_eq!(rfc3339(at), "2026-10-22T14:03:07Z");
        assert_eq!(short_date(at), "Oct 22");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn cores_drop_trailing_zeros() {
        assert_eq!(cores(1500), "1.5");
        assert_eq!(cores(12_000), "12");
        assert_eq!(cores(250), "0.25");
    }
}

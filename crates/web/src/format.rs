//! How the dashboard writes numbers and times: sizes as `runnerd.toml` writes them, and dates in UTC. Durations,
//! ages, CPU time and cores in use are `runnerd_core::api`'s, as every client writes them. Pure functions of their
//! arguments; the caller passes the current time.

pub(crate) use runnerd_core::api::{ago, cores_used, cpu_time, duration};
use runnerd_core::config::ByteSize;

/// A size with a binary unit, as in `runnerd.toml`: `512M`, `9G`.
pub(crate) fn bytes(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

/// `line` as a sentence, its first letter a capital, for the core's lines that start lower case.
pub(crate) fn sentence(line: &str) -> String {
    let mut chars = line.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
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
    fn dates_are_utc_calendar_days() {
        // 2026-10-22T14:03:07Z.
        let at = 1_792_677_787;
        assert_eq!(timestamp(at), "2026-10-22 14:03 UTC");
        assert_eq!(rfc3339(at), "2026-10-22T14:03:07Z");
        assert_eq!(short_date(at), "Oct 22");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}

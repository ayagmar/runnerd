//! How every client writes times and CPU use: durations, ages, CPU time, and cores in use. One rounding policy each,
//! so the dashboard, the TUI, `runnerctl` and the daemon's own commands print the same figure for the same value.
//! A limit (a cap, a reservation) is in CPUs to a tenth rounded up, by [`cpus`](super::cpus); use is in cores to a
//! hundredth rounded up, here. Pure functions of their arguments; the caller passes the current time.

/// Seconds as a stopwatch writes them, its width holding as it counts: `45s`, `1m 12s`, `2h 05m`, `3d 4h`.
pub fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        3600..86_400 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3600),
    }
}

/// How long ago `at` was: `just now` under ten seconds, then as [`duration`] writes it, `1h 05m ago`.
pub fn ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..10 => "just now".to_owned(),
        seconds => format!("{} ago", duration(seconds)),
    }
}

/// CPU time given in milliseconds: `4.6s` to a tenth under a minute, then as [`duration`] writes it.
pub fn cpu_time(milliseconds: u64) -> String {
    if milliseconds < 60_000 {
        let tenths = (milliseconds + 50) / 100;
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        duration(milliseconds / 1000)
    }
}

/// Millicores in use as cores, to a hundredth rounded up so use never reads as none: `0.25`, `1.5`, `12`, `0.01`.
pub fn cores_used(millicores: u64) -> String {
    let hundredths = millicores.div_ceil(10);
    let (whole, part) = (hundredths / 100, hundredths % 100);
    if part == 0 {
        whole.to_string()
    } else if part.is_multiple_of(10) {
        format!("{whole}.{}", part / 10)
    } else {
        format!("{whole}.{part:02}")
    }
}

/// Millicores in use as [`cores_used`] writes them, with the unit: `1.5 cores`, `1 core`.
pub fn in_cores(millicores: u64) -> String {
    match cores_used(millicores).as_str() {
        "1" => "1 core".to_owned(),
        number => format!("{number} cores"),
    }
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar, for years from 1970 (Howard Hinnant's
/// `days_from_civil`).
pub(crate) fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day count since 1970-01-01, as `(year, month, day)` (Howard Hinnant's `civil_from_days`, for days
/// after the epoch).
pub(crate) fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + u64::from(month <= 2), month, day)
}

/// A UTC day as a short label: `Oct 3`.
pub(crate) fn short_date(at: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, month, day) = civil_from_days(at / 86_400);
    let name = usize::try_from(month.saturating_sub(1))
        .ok()
        .and_then(|index| MONTHS.get(index))
        .unwrap_or(&"");
    format!("{name} {day}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_reads_like_a_stopwatch_in_every_client() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(72), "1m 12s");
        assert_eq!(duration(3909), "1h 05m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn an_age_is_the_duration_since() {
        assert_eq!(ago(100, 105), "just now");
        assert_eq!(ago(0, 3909), "1h 05m ago");
        assert_eq!(
            ago(200, 100),
            "just now",
            "a time ahead of the clock is now"
        );
    }

    #[test]
    fn cpu_time_keeps_work_under_a_second() {
        assert_eq!(cpu_time(4600), "4.6s");
        assert_eq!(cpu_time(600), "0.6s");
        assert_eq!(cpu_time(125_000), "2m 05s");
    }

    #[test]
    fn cores_in_use_never_read_as_none() {
        assert_eq!(cores_used(1), "0.01");
        assert_eq!(cores_used(250), "0.25");
        assert_eq!(cores_used(1500), "1.5");
        assert_eq!(cores_used(12_000), "12");
        assert_eq!(cores_used(0), "0");
        assert_eq!(in_cores(1000), "1 core");
        assert_eq!(in_cores(1250), "1.25 cores");
    }
}

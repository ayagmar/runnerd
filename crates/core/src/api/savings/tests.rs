//! What the job minutes of the history would have cost on GitHub's runners, as every client shows it.

use std::collections::BTreeMap;

use super::*;

/// 2026-10-26 12:00 UTC, in milliseconds.
const NOW: u64 = 1_793_016_000_000;
/// 2026-10-01 00:00 UTC, in milliseconds.
const OCTOBER: u64 = 1_790_812_800_000;

/// A completed job of `repository` in `profile` that ran `seconds` and ended `ago` milliseconds before `NOW`.
fn ran(repository: &str, profile: &str, seconds: u64, ago: u64) -> PricedRun {
    let finished = NOW - ago;
    PricedRun {
        repository: repository.to_owned(),
        workflow: Some("CI".to_owned()),
        labels: vec!["self-hosted".to_owned()],
        state: JobState::Completed,
        profile: Some(profile.to_owned()),
        queued_at: finished - seconds * 1000 - 5000,
        started_at: Some(finished - seconds * 1000),
        finished_at: Some(finished),
    }
}

fn history(runs: Vec<PricedRun>) -> PricingHistory {
    PricingHistory {
        runs,
        visibility: BTreeMap::from([
            ("o/app".to_owned(), Visibility::Private),
            ("o/site".to_owned(), Visibility::Public),
        ]),
        oldest: Some(OCTOBER - 60 * 86_400_000),
    }
}

/// `default` priced at `actions_linux`, `big` at `linux_8_core`.
fn pricing() -> Pricing {
    Pricing {
        profiles: BTreeMap::from([("big".to_owned(), Sku::Linux8Core)]),
        default_sku: Sku::ActionsLinux,
    }
}

fn month(runs: Vec<PricedRun>) -> SavedWindow {
    Savings::of(&history(runs), &pricing(), NOW).month
}

#[test]
fn each_job_rounds_up_to_whole_minutes_and_a_re_run_bills_again() {
    let window = month(vec![
        ran("o/app", "default", 61, 1000),
        ran("o/app", "default", 0, 2000),
        ran("o/app", "default", 61, 3000),
    ]);
    assert_eq!(
        window.minutes,
        2 + 1 + 2,
        "61 s bills 2 minutes, 0 s bills 1"
    );
    assert_eq!(window.jobs, 3);
    assert_eq!(window.cost, Usd(5 * 6_000));
}

#[test]
fn a_job_another_machine_ran_or_no_runner_took_is_not_counted() {
    let mut elsewhere = ran("o/app", "default", 600, 1000);
    elsewhere.state = JobState::Elsewhere;
    let mut cancelled_in_queue = elsewhere.clone();
    cancelled_in_queue.started_at = None;
    let window = month(vec![elsewhere, cancelled_in_queue]);
    assert_eq!((window.jobs, window.not_priced()), (0, 0));
    assert_eq!(window.headline(), "This month: no job-minutes here yet");
}

#[test]
fn a_public_repository_is_free_on_a_standard_runner_and_billed_on_a_larger_one() {
    let window = month(vec![
        ran("o/site", "default", 60, 1000),
        ran("o/site", "big", 60, 2000),
    ]);
    assert_eq!(window.free, 1);
    assert_eq!(window.jobs, 1);
    assert_eq!(window.cost, Sku::Linux8Core.rate());
}

#[test]
fn dependabot_and_pages_are_free_on_a_standard_runner_in_a_private_repository() {
    let mut labelled = ran("o/app", "default", 60, 1000);
    labelled.labels = vec!["dependabot".to_owned()];
    let mut dynamic = ran("o/app", "default", 60, 2000);
    dynamic.workflow = Some("Dependabot Updates".to_owned());
    let mut pages = ran("o/app", "default", 60, 3000);
    pages.workflow = Some("pages build and deployment".to_owned());
    let mut larger = ran("o/app", "big", 60, 4000);
    larger.labels = vec!["dependabot".to_owned()];
    let window = month(vec![labelled, dynamic, pages, larger]);
    assert_eq!(window.free, 3);
    assert_eq!(
        window.jobs, 1,
        "Dependabot on a larger runner is billed, as GitHub bills it"
    );
}

#[test]
fn jobs_that_cannot_be_priced_are_counted_and_said() {
    let mut lost = ran("o/app", "default", 86_400, 1000);
    lost.state = JobState::Lost;
    let mut untimed = ran("o/app", "default", 60, 2000);
    untimed.finished_at = None;
    let window = month(vec![
        lost,
        untimed,
        ran("o/gone", "default", 60, 3000),
        ran("o/app", "default", 60, 4000),
    ]);
    assert_eq!(
        (window.lost, window.untimed, window.unknown, window.jobs),
        (1, 1, 1, 1),
        "a day-long lost job never reaches the figure"
    );
    assert_eq!(
        window.headline(),
        "This month: 1 job-minute here, $0.00 at GitHub's 2-core Linux rate, before your plan's free minutes \
         (3 jobs not priced)"
    );
    assert_eq!(
        window.details(),
        [
            "1 minute of 1 job at actions_linux (2-core Linux, $0.006 a minute): $0.00",
            "1 job not priced: GitHub has not said whether their repository is public",
            "1 job not priced: lost, so how long they ran is not known",
            "1 job not priced: no start or end recorded",
            "An estimate from this machine's run times, at each profile's priced_as and each repository's \
             visibility as they are now; on GitHub's runners the jobs would run longer or shorter",
        ]
    );
}

#[test]
fn the_month_starts_at_midnight_utc_on_the_first() {
    let just_before = ran("o/app", "default", 60, NOW - OCTOBER + 1000);
    let at_start = ran("o/app", "default", 60, NOW - OCTOBER);
    let savings = Savings::of(&history(vec![just_before, at_start]), &pricing(), NOW);
    assert_eq!(savings.month.since, OCTOBER / 1000);
    assert_eq!(
        savings.month.jobs, 1,
        "a job that ended in September is not October's"
    );
    assert_eq!(savings.last_30_days.jobs, 2);
    assert_eq!(reads_since(NOW), NOW - 30 * 86_400_000);
}

#[test]
fn a_history_younger_than_the_window_says_where_it_starts() {
    let mut young = history(vec![ran("o/app", "big", 600, 1000)]);
    // 2026-10-03 08:00 UTC.
    young.oldest = Some(OCTOBER + 2 * 86_400_000 + 8 * 3_600_000);
    let window = Savings::of(&young, &pricing(), NOW).month;
    assert!(window.cut, "the window starts at the history's oldest job");
    assert_eq!(
        window.headline(),
        "Since Oct 3: 10 job-minutes here, $0.22 at GitHub's 8-core Linux rate, before your plan's free minutes"
    );
}

#[test]
fn several_runners_are_one_figure_with_a_line_each() {
    let window = month(vec![
        ran("o/app", "default", 600, 1000),
        ran("o/app", "big", 600, 2000),
        ran("o/app", "gone", 60, 3000),
    ]);
    assert_eq!(
        window.headline(),
        "This month: 21 job-minutes here, $0.28 at GitHub's rates for each profile's runner, before your plan's \
         free minutes"
    );
    assert_eq!(
        window
            .runners
            .iter()
            .map(|runner| (runner.sku, runner.minutes))
            .collect::<Vec<_>>(),
        [(Sku::ActionsLinux, 11), (Sku::Linux8Core, 10)],
        "a profile gone from the config is priced at the default runner"
    );
}

#[test]
fn money_is_written_to_the_cent_rounded_down() {
    assert_eq!(Usd(7_409_999).to_string(), "$7.40");
    assert_eq!(Usd(1_204_180_000).to_string(), "$1,204.18");
    assert_eq!(Usd(0).to_string(), "$0.00");
}

#[test]
fn a_profile_naming_no_runner_is_priced_at_the_hosts_standard_one() {
    assert_eq!(Sku::standard_for("aarch64"), Sku::ActionsLinuxArm);
    assert_eq!(Sku::standard_for("x86_64"), Sku::ActionsLinux);
}

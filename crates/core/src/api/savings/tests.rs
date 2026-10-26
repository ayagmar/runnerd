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
        kept_since: OCTOBER - 60 * 86_400_000,
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
fn a_job_named_as_dependabot_or_pages_is_not_priced_on_a_standard_runner_and_a_label_proves_nothing()
 {
    let mut labelled = ran("o/app", "default", 60, 1000);
    labelled.labels = vec!["self-hosted".to_owned(), "dependabot".to_owned()];
    let mut dynamic = ran("o/app", "default", 60, 2000);
    dynamic.workflow = Some("Dependabot Updates".to_owned());
    let mut pages = ran("o/app", "default", 60, 3000);
    pages.workflow = Some("pages build and deployment".to_owned());
    let mut larger = ran("o/app", "big", 60, 4000);
    larger.workflow = Some("Dependabot Updates".to_owned());
    let window = month(vec![labelled, dynamic, pages, larger]);
    assert_eq!(
        (window.services, window.free),
        (2, 0),
        "a name is not proof the job was free"
    );
    assert_eq!(
        window.jobs, 2,
        "CI on a dependabot-labelled pool is priced, and so is Dependabot on a larger runner"
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
        "This month: 1 job-minute here, an estimated $0.00 at GitHub's 2-core Linux rate, before your plan's free minutes, \
         by today's profiles and repository visibility (3 jobs not priced)"
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
    young.kept_since = OCTOBER + 2 * 86_400_000 + 8 * 3_600_000;
    let window = Savings::of(&young, &pricing(), NOW).month;
    assert!(window.cut, "the window starts where the history does");
    assert_eq!(
        window.headline(),
        "Since Oct 3: 10 job-minutes here, an estimated $0.22 at GitHub's 8-core Linux rate, before your plan's free minutes, \
         by today's profiles and repository visibility"
    );
}

#[test]
fn a_history_younger_than_both_windows_shows_one() {
    let mut young = history(vec![ran("o/app", "default", 60, 1000)]);
    young.kept_since = OCTOBER + 22 * 86_400_000;
    let savings = Savings::of(&young, &pricing(), NOW);
    assert_eq!(savings.windows(), vec![&savings.month]);
    let old = Savings::of(&history(vec![]), &pricing(), NOW);
    assert_eq!(old.windows(), vec![&old.month, &old.last_30_days]);
}

#[test]
fn jobs_from_before_the_history_s_start_are_left_out_while_pruning_deletes_them() {
    // Pruning has raised the start to Oct 19 but not yet deleted the Oct 15 jobs.
    let mut pruning = history(vec![
        ran("o/app", "default", 60_000, NOW - OCTOBER - 14 * 86_400_000),
        {
            let mut lost = ran("o/app", "default", 60, NOW - OCTOBER - 14 * 86_400_000);
            lost.state = JobState::Lost;
            lost
        },
        ran("o/app", "default", 600, 86_400_000),
    ]);
    pruning.kept_since = OCTOBER + 18 * 86_400_000;
    let window = Savings::of(&pruning, &pricing(), NOW).month;
    assert_eq!(
        (window.jobs, window.minutes, window.cost, window.lost),
        (1, 10, Usd(10 * 6_000), 0),
        "only the jobs since Oct 19 are counted"
    );
    assert!(
        window
            .headline()
            .starts_with("Since Oct 19: 10 job-minutes here, an estimated $0.06"),
        "{}",
        window.headline()
    );
}

#[test]
fn a_repository_made_private_prices_its_whole_history_and_the_figure_says_so() {
    let runs = vec![ran("o/site", "default", 600, 1000)];
    let public = month(runs.clone());
    let mut private = history(runs);
    private
        .visibility
        .insert("o/site".to_owned(), Visibility::Private);
    let private = Savings::of(&private, &pricing(), NOW).month;
    assert_eq!((public.cost, private.cost), (Usd(0), Usd(10 * 6_000)));
    for window in [public, private] {
        assert!(
            window
                .headline()
                .contains("by today's profiles and repository visibility"),
            "{}",
            window.headline()
        );
    }
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
        "This month: 21 job-minutes here, an estimated $0.28 at GitHub's rates for each profile's runner, before your plan's \
         free minutes, by today's profiles and repository visibility"
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
fn a_history_pruned_of_every_job_this_month_says_where_it_starts_not_that_none_ran() {
    let mut pruned = history(Vec::new());
    pruned.kept_since = NOW - 86_400_000;
    assert_eq!(
        Savings::of(&pruned, &pricing(), NOW).month.headline(),
        "Since Oct 25: no job-minutes here yet"
    );
}

#[test]
fn a_minute_and_a_millisecond_bills_two_minutes() {
    assert_eq!(billed_minutes(0, 60_000), 1);
    assert_eq!(billed_minutes(0, 60_001), 2);
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

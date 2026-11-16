use super::*;

const NOW: u64 = 1_800_000_000;

fn check(name: &str, status: CheckStatus) -> Check {
    Check {
        name: name.to_owned(),
        status,
        detail: format!("{name} detail"),
        fix: None,
    }
}

fn names(checks: &[&Check]) -> Vec<String> {
    checks.iter().map(|check| check.name.clone()).collect()
}

fn doctor(state: DoctorState) -> Doctor {
    Doctor {
        checks: vec![
            check("Docker", CheckStatus::Pass),
            check("slice", CheckStatus::Warn),
            check("sysbox", CheckStatus::Pass),
            check("kvm", CheckStatus::Info),
            check("repository", CheckStatus::Fail),
            check("mirror", CheckStatus::Skip),
        ],
        state: Some(state),
    }
}

#[test]
fn a_current_run_puts_failures_and_warnings_first_and_counts_the_rest_by_label() {
    let doctor = doctor(DoctorState::Ready { at: NOW - 240 });
    let summary = doctor.summary(NOW);
    assert_eq!(summary.not_current, None);
    assert_eq!(names(&summary.problems), ["slice", "repository"]);
    assert_eq!(names(&summary.rest), ["Docker", "sysbox", "kvm", "mirror"]);
    assert_eq!(
        summary.counts,
        [
            CheckCount {
                label: "ok",
                count: 2
            },
            CheckCount {
                label: "info",
                count: 1
            },
            CheckCount {
                label: "skip",
                count: 1
            },
        ],
        "in the order the labels first come"
    );
    assert_eq!(
        doctor
            .state
            .as_ref()
            .and_then(|state| state.checked(NOW))
            .as_deref(),
        Some("Checked 4m 00s ago")
    );
}

#[test]
fn checks_that_are_not_current_ask_nothing_of_the_reader_and_say_no_age() {
    for state in [
        DoctorState::Stale { at: NOW - 7200 },
        DoctorState::NotRun,
        DoctorState::Unknown,
    ] {
        let doctor = doctor(state.clone());
        let summary = doctor.summary(NOW);
        assert!(
            summary.problems.is_empty(),
            "{state:?}: no check stands as a problem"
        );
        assert_eq!(summary.rest.len(), 6, "{state:?}: every check folds");
        assert_eq!(summary.not_current, state.line(NOW));
        assert_eq!(
            state.checked(NOW),
            None,
            "{state:?}: no age beside checks that do not stand"
        );
    }
}

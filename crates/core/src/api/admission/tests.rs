//! The wire format of an admission and the budget against readers already released: 0.0.7's `Admission`,
//! `Basis` and `Budget` read what this one writes, and 0.0.13's `HostMemory` too.

use serde::Deserialize;

use super::{Admission, AdmissionPolicy, Basis, CpuAdmission, JobPeak};
use crate::api::{Budget, CpuBudget};

/// `Basis` as 0.0.7 shipped it.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
enum ReleasedBasis {
    Job { runs: u64 },
    Profile { runs: u64 },
    ProfileMax { runs: u64 },
    Overran { until: u64 },
    Cap,
    Unpredictable { until: u64 },
    Unmeasured,
}

/// `Admission` as 0.0.7 shipped it.
#[derive(Debug, Deserialize)]
#[expect(dead_code, reason = "only the basis is compared")]
struct ReleasedAdmission {
    policy: AdmissionPolicy,
    reserved: u64,
    basis: ReleasedBasis,
    free: Option<u64>,
    slice: Option<u64>,
}

fn admission(basis: Basis) -> Admission {
    Admission {
        policy: AdmissionPolicy::Measured,
        reserved: 1536 << 20,
        basis,
        free: Some(20 << 30),
        slice: Some(8 << 30),
        cpu: None,
    }
}

#[test]
fn a_released_reader_reads_a_prediction_from_a_jobs_largest_peak_as_one_from_its_runs() {
    let payload = serde_json::to_string(&admission(Basis::Job {
        runs: 3,
        peak: JobPeak::Largest,
    }))
    .unwrap();
    let read: ReleasedAdmission = serde_json::from_str(&payload).unwrap();
    assert_eq!(read.basis, ReleasedBasis::Job { runs: 3 });
}

#[test]
fn a_p95_prediction_is_written_as_it_was_and_records_written_before_read_as_p95() {
    let written = r#"{"policy":"measured","reserved":1610612736,"basis":{"kind":"job","runs":10},"free":null,"slice":null}"#;
    let read: Admission = serde_json::from_str(written).unwrap();
    assert_eq!(
        read.basis,
        Basis::Job {
            runs: 10,
            peak: JobPeak::P95
        }
    );
    assert_eq!(serde_json::to_string(&read).unwrap(), written);
}

/// `Budget` as 0.0.7 shipped it.
#[derive(Debug, Deserialize)]
#[expect(dead_code, reason = "only the memory is compared")]
struct ReleasedBudget {
    slice: String,
    memory_max: Option<u64>,
    memory_in_use: u64,
    #[serde(default)]
    admission: AdmissionPolicy,
    #[serde(default)]
    held: bool,
}

fn with_cpu(basis: Basis) -> Admission {
    Admission {
        cpu: Some(CpuAdmission {
            reserved: 1500,
            basis,
            free: 20_000,
            waited: Some(90),
        }),
        ..admission(Basis::Cap)
    }
}

#[test]
fn a_released_reader_reads_an_admission_and_a_budget_with_cpu_as_before() {
    let payload = serde_json::to_string(&with_cpu(Basis::Job {
        runs: 3,
        peak: JobPeak::Largest,
    }))
    .unwrap();
    let read: ReleasedAdmission = serde_json::from_str(&payload).unwrap();
    assert_eq!(read.basis, ReleasedBasis::Cap);
    let budget = Budget {
        slice: "ci.slice".to_owned(),
        memory_max: Some(33 << 30),
        memory_in_use: 9 << 30,
        cpu: Some(CpuBudget {
            cpus: 32_000,
            budget: 32_000,
            in_use: 4500,
            held: false,
        }),
        ..Budget::default()
    };
    let read: ReleasedBudget =
        serde_json::from_str(&serde_json::to_string(&budget).unwrap()).unwrap();
    assert_eq!(read.memory_in_use, 9 << 30);
}

#[test]
fn an_admission_or_budget_without_cpu_is_written_as_before() {
    let written = r#"{"policy":"measured","reserved":1610612736,"basis":{"kind":"cap"},"free":null,"slice":null}"#;
    let read: Admission = serde_json::from_str(written).unwrap();
    assert_eq!(read.cpu, None);
    assert_eq!(serde_json::to_string(&read).unwrap(), written);
    let budget = serde_json::to_value(Budget::default()).unwrap();
    assert!(budget.get("cpu").is_none(), "{budget}");
}

#[test]
fn a_record_says_what_cpu_its_runner_reserved_and_how_long_it_waited_for_it() {
    let line = with_cpu(Basis::Job {
        runs: 12,
        peak: JobPeak::P95,
    })
    .line();
    assert!(
        line.ends_with(
            ". Reserved 1.5 CPUs, the p95 average of 12 runs of its job; 20 CPUs of the budget were free; it waited \
             1m30s for CPU"
        ),
        "{line}"
    );
}

/// `HostMemory` as 0.0.13 shipped it, before it named what holds the host's memory.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct ReleasedHostMemory {
    at: u64,
    total: u64,
    available: u64,
    slice: u64,
}

#[test]
fn a_host_reading_names_its_holders_to_new_readers_and_reads_as_before_without_them() {
    let old = r#"{"at":7,"total":64,"available":2,"slice":10,"some":null,"full":null}"#;
    let read: crate::api::HostMemory = serde_json::from_str(old).unwrap();
    assert!(read.holders.is_empty(), "a 0.0.13 daemon names none");
    let named = crate::api::HostMemory {
        holders: vec![crate::api::Holder {
            name: "java".to_owned(),
            memory: 9,
        }],
        ..read
    };
    let payload = serde_json::to_string(&named).unwrap();
    assert_eq!(
        serde_json::from_str::<crate::api::HostMemory>(&payload).unwrap(),
        named
    );
    let released: ReleasedHostMemory = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        released,
        ReleasedHostMemory {
            at: 7,
            total: 64,
            available: 2,
            slice: 10
        }
    );
}

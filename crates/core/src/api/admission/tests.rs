//! The wire format of an admission and the host's memory: what serializing leaves out reads back as it was.

use super::{Admission, AdmissionPolicy, Basis, CpuAdmission, JobPeak};
use crate::api::Budget;

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
fn a_p95_prediction_is_written_without_its_peak_and_reads_back_as_p95() {
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
fn an_admission_or_budget_without_cpu_leaves_it_out() {
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
             1m 30s for CPU"
        ),
        "{line}"
    );
}

#[test]
fn a_host_reading_names_its_holders_and_leaves_them_out_when_none_is_that_large() {
    let none = crate::api::HostMemory {
        at: 7,
        total: 64,
        available: 2,
        slice: 10,
        some: None,
        full: None,
        holders: Vec::new(),
    };
    assert_eq!(
        serde_json::to_string(&none).unwrap(),
        r#"{"at":7,"total":64,"available":2,"slice":10,"some":null,"full":null}"#
    );
    let named = crate::api::HostMemory {
        holders: vec![crate::api::Holder {
            name: "java".to_owned(),
            memory: 9,
        }],
        ..none
    };
    for reading in [none, named] {
        let payload = serde_json::to_string(&reading).unwrap();
        assert_eq!(
            serde_json::from_str::<crate::api::HostMemory>(&payload).unwrap(),
            reading
        );
    }
}

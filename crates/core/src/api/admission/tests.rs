//! The wire format of an admission against readers already released: 0.0.7's `Basis` reads what this one writes.

use serde::Deserialize;

use super::{Admission, AdmissionPolicy, Basis, JobPeak};

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

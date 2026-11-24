//! Three machines read together (ADR 0165): this one, `tern`, as [`daemon`] answers but on its battery, which holds
//! every start (ADR 0175); `skiff`, an older daemon whose
//! status names no machine, which ran [`ELSEWHERE`] and a newer job; and `dory`, which cannot be reached. With the
//! facts core computes from them.

use runnerd_core::api::{Fleet, JobPage, JobRunner, JobState, MachineReading, Tone};

use crate::{ELSEWHERE, NOW, daemon};

/// A job only `skiff` ran, newer than every job of `tern`.
pub const SKIFF_ONLY: u64 = 9500;

/// Why `dory` cannot be read.
pub const DORY_DOWN: &str =
    "ssh dory failed (ssh said: ssh: connect to host dory port 22: Connection timed out)";

/// What a client reads from the three machines, `tern` first.
pub fn fleet_readings() -> Vec<MachineReading> {
    let tern = daemon();
    let mut tern_status = tern.status.clone();
    tern_status.on_battery = true;
    let mut skiff_status = tern.status.clone();
    skiff_status.machine = None;
    "0.1.5".clone_into(&mut skiff_status.version);
    skiff_status.shutting_down = false;
    let ran = |id: u64, state: JobState| {
        let mut job = tern
            .job(ELSEWHERE)
            .map_or_else(|| tern.summaries()[0].clone(), |job| job.summary.clone());
        job.id = id;
        job.state = state;
        job.conclusion = state.is_final().then(|| "success".to_owned());
        job.timeline.queued_at = NOW - 600;
        job.runner = Some(JobRunner {
            name: "runnerd-1a2b3c4d-skiff-jvm-1-1700000000".to_owned(),
            profile: "jvm".to_owned(),
            slot: 1,
            memory_cap: None,
            cpu_cap: None,
        });
        job
    };
    vec![
        MachineReading {
            machine: "tern".to_owned(),
            status: Ok(tern_status),
            jobs: Ok(JobPage {
                jobs: tern.summaries(),
                next: None,
            }),
        },
        MachineReading {
            machine: "skiff".to_owned(),
            status: Ok(skiff_status),
            jobs: Ok(JobPage {
                jobs: vec![
                    ran(SKIFF_ONLY, JobState::Running),
                    ran(ELSEWHERE, JobState::Completed),
                ],
                next: None,
            }),
        },
        MachineReading {
            machine: "dory".to_owned(),
            status: Err(DORY_DOWN.to_owned()),
            jobs: Err(DORY_DOWN.to_owned()),
        },
    ]
}

/// A machine's line, as core words and tones it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineFact {
    /// Its name.
    pub name: String,
    /// [`runnerd_core::api::FleetMachine::summary`].
    pub summary: String,
    /// [`runnerd_core::api::FleetMachine::tone`].
    pub tone: Tone,
}

/// What every client of the fleet shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetFacts {
    /// Each machine, in order.
    pub machines: Vec<MachineFact>,
    /// Each job shown, newest first.
    pub jobs: Vec<FleetJobFact>,
}

/// A job of the fleet's page and the machine whose record of it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetJobFact {
    /// The job's ID.
    pub id: u64,
    /// The machine.
    pub machine: String,
}

/// Core's facts of a merged fleet.
pub fn fleet_facts(fleet: &Fleet) -> FleetFacts {
    FleetFacts {
        machines: fleet
            .machines
            .iter()
            .map(|machine| MachineFact {
                name: machine.name.clone(),
                summary: machine.summary(),
                tone: machine.tone(),
            })
            .collect(),
        jobs: fleet
            .jobs
            .iter()
            .map(|job| FleetJobFact {
                id: job.job.id,
                machine: job.machine.clone(),
            })
            .collect(),
    }
}

/// The fleet of [`fleet_readings`], one page of up to 50 jobs.
pub fn fleet() -> Fleet {
    Fleet::merge(fleet_readings(), 50)
}

#[cfg(test)]
mod tests {
    use runnerd_core::api::{FleetMachine, MachineReading, ON_BATTERY, Reach, THIS_MACHINE, Tone};

    use super::{DORY_DOWN, FleetJobFact, SKIFF_ONLY, fleet, fleet_facts};
    use crate::ELSEWHERE;

    /// Catches: the job skiff ran shown as tern's `elsewhere`, skiff's older status failing to read, and dory hiding
    /// the others.
    #[test]
    fn the_fleet_shows_each_job_once_with_the_machine_that_ran_it_and_names_the_one_down() {
        let fleet = fleet();
        let facts = fleet_facts(&fleet);
        let skiff = |id| FleetJobFact {
            id,
            machine: "skiff".to_owned(),
        };
        assert_eq!(facts.jobs[0], skiff(SKIFF_ONLY));
        assert!(facts.jobs.contains(&skiff(ELSEWHERE)), "{:?}", facts.jobs);
        assert_eq!(
            facts.jobs.iter().filter(|job| job.id == ELSEWHERE).count(),
            1,
            "once"
        );
        assert!(
            matches!(fleet.machines[1].reach, Reach::Read { id: None, .. }),
            "skiff's older status reads, without an id"
        );
        assert!(
            facts.machines[1].summary.ends_with("runnerd 0.1.5"),
            "{}",
            facts.machines[1].summary
        );
        assert_eq!(
            facts.machines[2].summary,
            format!("cannot be read: {DORY_DOWN}")
        );
        assert_eq!(facts.machines[2].tone, Tone::Bad);
        assert!(
            facts.machines[0].summary.starts_with(ON_BATTERY),
            "tern says first that its battery holds every start: {}",
            facts.machines[0].summary
        );
        assert!(
            !facts.machines[1].summary.contains("battery"),
            "{}",
            facts.machines[1].summary
        );
        assert_eq!(fleet.next, None);
        assert!(!fleet.all_read(), "dory was not read");
    }

    /// Catches: a list of machines that reads only each status (ADR 0175) calling every machine's jobs unread, so all
    /// of them show as bad, or a machine read so losing its battery hold or its name.
    #[test]
    fn a_machine_read_from_its_status_alone_says_what_its_status_says() {
        let readings = super::fleet_readings();
        let merged = fleet();
        for (reading, merged) in readings.into_iter().zip(&merged.machines) {
            let alone = FleetMachine::of_status(reading.machine, reading.status);
            assert_eq!(&alone, merged, "the same line as the fleet's, jobs or not");
        }
        // Not shutting down, so the battery alone makes the warning.
        let mut status = crate::daemon().status;
        status.shutting_down = false;
        let plugged = FleetMachine::this_machine(Ok(status.clone()));
        assert_eq!(plugged.tone(), Tone::Neutral, "{}", plugged.summary());
        assert!(
            !plugged.summary().contains("battery"),
            "{}",
            plugged.summary()
        );
        status.on_battery = true;
        let this = FleetMachine::this_machine(Ok(status));
        assert_eq!(this.name, "tern");
        assert_eq!(this.tone(), Tone::Warn, "a held machine warns");
        assert!(this.summary().starts_with(ON_BATTERY), "{}", this.summary());
        let unasked = FleetMachine::unasked();
        assert_eq!(unasked.name, THIS_MACHINE);
        assert!(
            unasked.summary().contains("XDG_RUNTIME_DIR"),
            "{}",
            unasked.summary()
        );
    }

    /// Catches: this machine's line named by each client in its own words, or a daemon's own name ignored.
    #[test]
    fn this_machine_is_named_as_its_daemon_says_or_as_this_machine() {
        let mut status = crate::daemon().status;
        let named = MachineReading::this_machine(Ok(status.clone()), Err("x".to_owned()));
        assert_eq!(named.machine, "tern");
        status.machine = None;
        let unnamed = MachineReading::this_machine(Ok(status), Err("x".to_owned()));
        assert_eq!(unnamed.machine, THIS_MACHINE);
        let down = MachineReading::this_machine(Err("down".to_owned()), Err("down".to_owned()));
        assert_eq!(down.machine, THIS_MACHINE);
        let unasked = MachineReading::unasked();
        assert!(
            unasked
                .status
                .as_ref()
                .is_err_and(|why| why.contains("XDG_RUNTIME_DIR")),
            "{unasked:?}"
        );
    }
}

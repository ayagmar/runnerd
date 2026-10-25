//! What an owner's action did, as every client says it after the daemon answers: a pause and the runners it
//! stopped, what forcing one would cancel, a reseed asked for and done, records removed and a resize written.
//! Clients may ask differently (a button, a key, a flag); the facts they report come from here.

use super::{Paused, RemovedRecords, Resize};
use crate::config::ByteSize;

/// What forcing a pause does to `jobs` running jobs: `cancels its run on GitHub and removes its runner`, or for
/// several, `cancels their runs on GitHub and removes their runners`.
pub fn force_pause_consequence(jobs: usize) -> &'static str {
    if jobs == 1 {
        "cancels its run on GitHub and removes its runner"
    } else {
        "cancels their runs on GitHub and removes their runners"
    }
}

impl Paused {
    /// The pause of `target`, a repository or `every repository`: `o/app paused: it gets no new runners`, or for a
    /// forced one, `o/app paused; stopped job 42 on runnerd-jvm-1`.
    pub fn line(&self, target: &str) -> String {
        if self.stopped.is_empty() {
            return format!("{target} paused: it gets no new runners");
        }
        let stopped: Vec<String> = self.stopped.iter().map(ToString::to_string).collect();
        format!("{target} paused; stopped {}", stopped.join(", "))
    }
}

/// A reseed of `profile` the daemon accepted: `jvm: reseed requested; it runs once the profile's slots are free`.
pub fn reseed_requested(profile: &str) -> String {
    format!("{profile}: reseed requested; it runs once the profile's slots are free")
}

/// A reseed of `profile` done, as `cache.reseeded` reports it: `jvm: reseeded` or `jvm: reseed failed: …`.
pub fn reseeded(profile: &str, error: Option<&str>) -> String {
    match error {
        Some(error) => format!("{profile}: reseed failed: {error}"),
        None => format!("{profile}: reseeded"),
    }
}

impl RemovedRecords {
    /// What it freed: `no record removed`, `removed 1 record, 3G freed` or `removed 4 records, 9G freed`.
    pub fn line(&self) -> String {
        match self.jobs.len() {
            0 => "no record removed".to_owned(),
            1 => format!("removed 1 record, {} freed", ByteSize(self.bytes)),
            count => format!("removed {count} records, {} freed", ByteSize(self.bytes)),
        }
    }
}

impl Resize {
    /// It written into `profile`: `jvm: memory 6G, 2 slots written; the daemon runs with them`, naming its CPUs
    /// when it changes them.
    pub fn written_line(&self, profile: &str) -> String {
        let cpus = self
            .cpus
            .map(|cpus| format!(", {cpus} cpus"))
            .unwrap_or_default();
        format!(
            "{profile}: memory {}, {} slots{cpus} written; the daemon runs with them",
            ByteSize(self.memory),
            self.slots
        )
    }
}

#[cfg(test)]
mod tests;

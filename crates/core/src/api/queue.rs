//! What holds the queue as a whole, for a script to ask in one call: whether a queued job waits for the host's
//! memory. It reads the status the daemon served; it decides nothing about scheduling.

use super::{QueuedJob, Status, Waiting};

impl QueuedJob {
    /// Whether it waits for the host's memory: it does not fit what the host has to spare beside its other programs
    /// (`Waiting::HostMemory`), or it waits behind an older job for memory (`Waiting::BehindMemory`) and that job waits
    /// for the host's memory. Memory the host's other programs give back would let it start. `false` for a job not
    /// planned yet, one behind a job no longer queued, and a reason this client does not know.
    pub fn waits_for_host_memory(&self, status: &Status) -> bool {
        let mut job = self;
        // Each step goes to an older job, so a chain ends within the queue; the bound only guards a malformed one.
        for _ in 0..=status.queued.len() {
            match &job.waiting {
                Some(Waiting::HostMemory { .. }) => return true,
                Some(Waiting::BehindMemory { job: older }) => {
                    match status.queued.iter().find(|queued| queued.id == *older) {
                        Some(older) => job = older,
                        None => return false,
                    }
                }
                _ => return false,
            }
        }
        false
    }
}

/// Whether queued jobs wait for the host's memory, as [`Status::host_wait`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWait {
    /// A queued job asked about waits for the host's memory.
    Waits,
    /// None does.
    Clear,
    /// The repository asked about is not one the daemon serves, so the answer would say nothing.
    UnknownRepository,
}

impl Status {
    /// Whether a queued job, of `repository` (`owner/name`, in any case, as GitHub reads it) when given, waits for
    /// the host's memory ([`QueuedJob::waits_for_host_memory`]): what a local build asks before it takes memory CI
    /// could start with.
    pub fn host_wait(&self, repository: Option<&str>) -> HostWait {
        let repository = match repository {
            None => None,
            Some(asked) => match self
                .repositories
                .iter()
                .find(|served| served.name.eq_ignore_ascii_case(asked))
            {
                Some(served) => Some(served.name.as_str()),
                None => return HostWait::UnknownRepository,
            },
        };
        let waits = self
            .queued
            .iter()
            .filter(|job| repository.is_none_or(|name| job.repository.eq_ignore_ascii_case(name)))
            .any(|job| job.waits_for_host_memory(self));
        if waits {
            HostWait::Waits
        } else {
            HostWait::Clear
        }
    }
}

#[cfg(test)]
mod tests;

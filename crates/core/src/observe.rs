//! What runnerd learns about running jobs without GitHub's help: CPU, memory and network of each runner container
//! from its cgroup v2 files and `/proc` (`metrics`), what it and the drive under Docker's root write (`disk`), the job a runner took and its log from inside the container
//! (`capture`), which runner pulled what through the Docker Hub mirror (`mirror`), and whether the host runs on its
//! battery (`power`), and what file systems hold the history and Docker's root (`storage`). Reads files and parses
//! output; it starts no container, writes nothing to the host but `storage`'s flag on a directory or empty file the
//! store just made, and decides nothing about scheduling.

pub mod capture;
pub mod disk;
pub mod metrics;
pub mod mirror;
pub mod power;
pub mod storage;

#[cfg(test)]
mod tests;

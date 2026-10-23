//! CPU, memory and network of runner containers and of their slice, read from cgroup v2 files and
//! `/proc/<pid>/net/dev` every `SAMPLE_INTERVAL`. Memory is what the kernel cannot take back from the cgroup
//! without swap: `memory.current` less the file LRU lists (`active_file`, `inactive_file`) and `slab_reclaimable`,
//! so anonymous memory, tmpfs and shared memory (`shmem`, which sits on the anon lists), and unreclaimable kernel
//! memory. That is what `memory.max` OOM-kills on: the kernel's cgroup-v2 docs (Documentation/admin-guide/
//! cgroup-v2.rst) invoke the OOM killer when usage reaches the limit "and can't be reduced". `docker stats`
//! subtracts only `inactive_file`, which still counts page cache read twice. Whether the cgroup met its limit
//! comes from `memory.events`, never from how close a sample came to it. A container whose files vanish ends its
//! series; that is not an error.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use crate::api;
use crate::store::{SampleRecord, Store};
use crate::sync::lock;
use crate::time;

/// How often running containers are sampled.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Where the cgroup and proc file systems are mounted: the host's, or a test's fixture tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Usually `/sys/fs/cgroup`.
    pub cgroup: PathBuf,
    /// Usually `/proc`.
    pub proc: PathBuf,
}

impl Default for Paths {
    fn default() -> Paths {
        Paths {
            cgroup: PathBuf::from("/sys/fs/cgroup"),
            proc: PathBuf::from("/proc"),
        }
    }
}

impl Paths {
    /// The cgroup of a systemd slice: `a-b.slice` is `a.slice/a-b.slice`.
    pub fn slice(&self, slice: &str) -> PathBuf {
        let name = slice.strip_suffix(".slice").unwrap_or(slice);
        let mut dir = self.cgroup.clone();
        let mut prefix = String::new();
        for part in name.split('-') {
            if !prefix.is_empty() {
                prefix.push('-');
            }
            prefix.push_str(part);
            dir.push(format!("{prefix}.slice"));
        }
        dir
    }

    /// The cgroup Docker's systemd driver gives a container in a slice.
    pub fn container(&self, slice: &str, container: &str) -> PathBuf {
        self.slice(slice).join(format!("docker-{container}.scope"))
    }
}

/// A cgroup's counters at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// CPU time so far, in microseconds (`cpu.stat` `usage_usec`).
    pub cpu_usage: u64,
    /// `memory.current` less `active_file`, `inactive_file` and `slab_reclaimable` from `memory.stat`.
    pub memory: u64,
}

/// What a cgroup's `memory.events` counted so far, its descendants' included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Events {
    /// Times its memory reached `memory.max`, reclaim could not bring it down and an allocation was about to fail
    /// (`oom`). Not `max`, which counts every reclaim at the limit, page cache's included.
    pub oom: u64,
    /// Processes the kernel killed in it for memory (`oom_kill`).
    pub oom_kill: u64,
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
}

fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        (name == key).then(|| value.trim().parse().ok()).flatten()
    })
}

fn invalid(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} has an unexpected format", path.display()),
    )
}

/// Reads a cgroup's CPU and memory counters.
pub fn read_usage(cgroup: &Path) -> io::Result<Usage> {
    let current_path = cgroup.join("memory.current");
    let current: u64 = read(&current_path)?
        .trim()
        .parse()
        .map_err(|_| invalid(&current_path))?;
    let stat_path = cgroup.join("memory.stat");
    let stat = read(&stat_path)?;
    let reclaimable = ["active_file", "inactive_file", "slab_reclaimable"]
        .iter()
        .map(|key| field(&stat, key))
        .sum::<Option<u64>>()
        .ok_or_else(|| invalid(&stat_path))?;
    let cpu_path = cgroup.join("cpu.stat");
    let cpu_usage = field(&read(&cpu_path)?, "usage_usec").ok_or_else(|| invalid(&cpu_path))?;
    Ok(Usage {
        cpu_usage,
        memory: current.saturating_sub(reclaimable),
    })
}

/// A cgroup's `memory.events`; `None` when it can't be read, a counter it lacks being 0.
pub fn read_events(cgroup: &Path) -> Option<Events> {
    let events = read(&cgroup.join("memory.events")).ok()?;
    Some(Events {
        oom: field(&events, "oom").unwrap_or(0),
        oom_kill: field(&events, "oom_kill").unwrap_or(0),
    })
}

/// Bytes received and sent by a process's network namespace on its `eth*` interfaces: the container's own
/// traffic, without its loopback or the bridge of the Docker Engine inside it, whose traffic also crosses `eth0`.
pub fn read_network(paths: &Paths, pid: u32) -> io::Result<(u64, u64)> {
    let path = paths.proc.join(pid.to_string()).join("net/dev");
    let text = read(&path)?;
    let mut totals = (0, 0);
    for line in text.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            return Err(invalid(&path));
        };
        if !name.trim().starts_with("eth") {
            continue;
        }
        let counters: Vec<u64> = counters
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid(&path))?;
        // Receive: bytes packets errs drop fifo frame compressed multicast; then transmit, bytes first.
        let (Some(received), Some(sent)) = (counters.first(), counters.get(8)) else {
            return Err(invalid(&path));
        };
        totals.0 += received;
        totals.1 += sent;
    }
    Ok(totals)
}

/// A runner container to sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    /// The runner's name, which keys its samples.
    pub runner: String,
    /// The full Docker container ID.
    pub container: String,
    /// The container's main process on the host, for its network counters.
    pub pid: u32,
}

/// Samples containers and their slice, turning CPU time into CPU use between samples.
#[derive(Debug)]
pub struct Sampler {
    paths: Paths,
    slice: String,
    previous: HashMap<String, (u64, u64)>,
    previous_host: Option<(u64, u64)>,
}

/// Millicores used between two readings of CPU time (µs) taken at two times (ms).
fn millicores(previous: Option<(u64, u64)>, at: u64, cpu_usage: u64) -> u64 {
    previous.map_or(0, |(then, usage)| {
        cpu_usage
            .saturating_sub(usage)
            .checked_div(at.saturating_sub(then))
            .unwrap_or(0)
    })
}

impl Sampler {
    /// A sampler for containers in `slice`.
    pub fn new(paths: Paths, slice: &str) -> Sampler {
        Sampler {
            paths,
            slice: slice.to_owned(),
            previous: HashMap::new(),
            previous_host: None,
        }
    }

    /// One sample of every tracked container that still exists, and of the slice, at `at` (ms). A container
    /// whose files are gone gets no sample, and is forgotten.
    pub fn sample(
        &mut self,
        tracked: &[Tracked],
        at: u64,
    ) -> (Vec<SampleRecord>, Option<api::HostSample>) {
        let mut samples = Vec::new();
        let mut seen = HashMap::new();
        for container in tracked {
            let cgroup = self.paths.container(&self.slice, &container.container);
            let (Ok(usage), Ok((received, sent))) = (
                read_usage(&cgroup),
                read_network(&self.paths, container.pid),
            ) else {
                continue;
            };
            let cpu = millicores(
                self.previous.get(&container.runner).copied(),
                at,
                usage.cpu_usage,
            );
            seen.insert(container.runner.clone(), (at, usage.cpu_usage));
            samples.push(SampleRecord {
                runner: container.runner.clone(),
                at,
                cpu,
                cpu_usage: usage.cpu_usage,
                memory: usage.memory,
                network_received: received,
                network_sent: sent,
                events: read_events(&cgroup),
            });
        }
        self.previous = seen;
        let host = read_usage(&self.paths.slice(&self.slice))
            .ok()
            .map(|usage| {
                let cpu = millicores(self.previous_host, at, usage.cpu_usage);
                self.previous_host = Some((at, usage.cpu_usage));
                api::HostSample {
                    at: at / 1000,
                    cpu,
                    memory: usage.memory,
                }
            });
        (samples, host)
    }
}

/// Samples the tracked containers every `SAMPLE_INTERVAL` into the store, until the engine drops its sender.
pub async fn run(state: Sampler, tracked: watch::Receiver<Vec<Tracked>>, store: Store) {
    let state = Arc::new(Mutex::new(state));
    let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if tracked.has_changed().is_err() {
            return;
        }
        let containers = tracked.borrow().clone();
        let state = state.clone();
        // Reading cgroup files is quick but blocking.
        let Ok((samples, host)) = tokio::task::spawn_blocking(move || {
            // A panic midway through a sample leaves the counters of the last or of this one, either of which the
            // next sample measures from.
            lock(&state).sample(&containers, time::now_ms())
        })
        .await
        else {
            continue;
        };
        if !samples.is_empty() {
            store.samples(samples);
        }
        if let Some(host) = host {
            store.host_sample(host);
        }
    }
}

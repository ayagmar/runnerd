//! CPU, memory and network of runner containers and of their slice, read from cgroup v2 files and
//! `/proc/<pid>/net/dev` every `SAMPLE_INTERVAL`. A container's memory is an estimate of what its job holds without
//! page cache: `memory.current` less the file LRU lists (`active_file`, `inactive_file`), so anonymous memory, tmpfs
//! and shared memory (`shmem`, on the anon lists) and kernel memory, reclaimable slab included, since slab in use
//! (an open file's dentry and inode) can't be freed. Page cache on the file lists is usually the kernel's to drop
//! before `memory.max` OOM-kills ("reaches this limit and can't be reduced", Documentation/admin-guide/
//! cgroup-v2.rst), though dirty, writeback or pinned pages may not be dropped at once. `docker stats` subtracts only
//! `inactive_file`, so it counts page cache read twice. The slice's memory for the high-water mark is that
//! conservative number, `memory.current` less `inactive_file`, as it always was: it stops starts before the slice
//! is short of memory, not after. Whether a cgroup met its limit comes from `memory.events`, never from how close
//! a sample came to it. A container whose files vanish ends its series; that is not an error. Beside the slice, it
//! reads the host's memory: `MemTotal` and `MemAvailable` from `/proc/meminfo` and the stall shares of
//! `/proc/pressure/memory`.

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
    /// `memory.current` less `active_file` and `inactive_file` from `memory.stat`: without page cache.
    pub memory: u64,
    /// `memory.current` less `inactive_file`: page cache read twice still counts.
    pub memory_with_active_file: u64,
}

/// What a cgroup's `memory.events` counted so far, its descendants' included; `None` for a counter the file lacks
/// or that doesn't parse, which says nothing either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Events {
    /// Times its memory, or a descendant's under its own limit, reached the limit, reclaim could not bring it down
    /// and an allocation was about to fail (`oom`). Not `max`, which counts every reclaim at the limit, page
    /// cache's included.
    pub oom: Option<u64>,
    /// Processes the kernel killed in it for memory (`oom_kill`).
    pub oom_kill: Option<u64>,
    /// Times a socket's receive window was held small because its memory, or a descendant's, was under pressure
    /// (`sock_throttled`): reclaim that went badly, of the host or of a limit, slows its downloads. `None` on a
    /// kernel without the counter.
    pub sock_throttled: Option<u64>,
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
    let (Some(active_file), Some(inactive_file)) =
        (field(&stat, "active_file"), field(&stat, "inactive_file"))
    else {
        return Err(invalid(&stat_path));
    };
    let cpu_path = cgroup.join("cpu.stat");
    let cpu_usage = field(&read(&cpu_path)?, "usage_usec").ok_or_else(|| invalid(&cpu_path))?;
    Ok(Usage {
        cpu_usage,
        memory: current.saturating_sub(active_file + inactive_file),
        memory_with_active_file: current.saturating_sub(inactive_file),
    })
}

/// What a cgroup holds that `MemAvailable` does not count as available: its memory without page cache less its
/// reclaimable slab (`slab_reclaimable`), which `MemAvailable` counts already. 0 when it can't be read.
pub fn read_unavailable(cgroup: &Path) -> u64 {
    let slab = read(&cgroup.join("memory.stat"))
        .ok()
        .and_then(|stat| field(&stat, "slab_reclaimable"))
        .unwrap_or(0);
    read_usage(cgroup).map_or(0, |usage| usage.memory.saturating_sub(slab))
}

/// A cgroup's `memory.events`; every counter `None` when the file can't be read.
pub fn read_events(cgroup: &Path) -> Events {
    let Ok(events) = read(&cgroup.join("memory.events")) else {
        return Events::default();
    };
    Events {
        oom: field(&events, "oom"),
        oom_kill: field(&events, "oom_kill"),
        sock_throttled: field(&events, "sock_throttled"),
    }
}

/// The host's memory from `/proc/meminfo` and `/proc/pressure/memory`, beside `slice` bytes the runners hold
/// without page cache, at `at` (seconds). `None` without `MemTotal` and `MemAvailable`; the pressure lines are `None`
/// on a kernel without pressure stall information.
pub fn read_host_memory(paths: &Paths, slice: u64, at: u64) -> Option<api::HostMemory> {
    let meminfo = read(&paths.proc.join("meminfo")).ok()?;
    let kib = |key: &str| {
        meminfo.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix(':')?;
            value.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
        })
    };
    let pressure = read(&paths.proc.join("pressure/memory")).unwrap_or_default();
    let line = |kind: &str| {
        let fields = pressure
            .lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))?;
        let avg = |key: &str| {
            fields.split_whitespace().find_map(|field| {
                let value = field.strip_prefix(key)?.strip_prefix('=')?;
                let (whole, fraction) = value.split_once('.').unwrap_or((value, "0"));
                let hundredths: u32 = format!("{fraction:0<2}").get(..2)?.parse().ok()?;
                Some(
                    whole
                        .parse::<u32>()
                        .ok()?
                        .saturating_mul(100)
                        .saturating_add(hundredths),
                )
            })
        };
        Some(api::Pressure {
            avg10: avg("avg10")?,
            avg60: avg("avg60")?,
        })
    };
    Some(api::HostMemory {
        at,
        total: kib("MemTotal")?.saturating_mul(1024),
        available: kib("MemAvailable")?.saturating_mul(1024),
        slice,
        some: line("some"),
        full: line("full"),
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

    /// The host's memory at `at` (ms), beside what the slice holds that `MemAvailable` leaves out
    /// (`read_unavailable`).
    pub fn host_memory(&self, at: u64) -> Option<api::HostMemory> {
        let slice = read_unavailable(&self.paths.slice(&self.slice));
        read_host_memory(&self.paths, slice, at / 1000)
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
                    memory: usage.memory_with_active_file,
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
        let Ok(((samples, host), memory)) = tokio::task::spawn_blocking(move || {
            // A panic midway through a sample leaves the counters of the last or of this one, either of which the
            // next sample measures from.
            let mut state = lock(&state);
            let at = time::now_ms();
            (state.sample(&containers, at), state.host_memory(at))
        })
        .await
        else {
            continue;
        };
        if let Some(memory) = memory {
            store.host_memory(memory);
        }
        if !samples.is_empty() {
            store.samples(samples);
        }
        if let Some(host) = host {
            store.host_sample(host);
        }
    }
}

//! What the disk under Docker's root writes: which block devices hold that root (`Disks::resolve`, from
//! `/proc/self/mountinfo` and sysfs), the bytes a cgroup wrote to them (`io.stat`), and over windows of
//! `WINDOW` the bytes the drive wrote (`/sys/class/block/<disk>/stat`), the bytes the runners' slice wrote and the
//! btrfs commits of the file system (`/sys/fs/btrfs/<uuid>/commit_stats`). A figure the host does not give, as on
//! cgroup v1, a kernel without the io controller, a file system other than btrfs or an unreadable sysfs, is absent,
//! never 0. Reads files only; it writes nothing to the host and decides nothing.
//!
//! Only the disks under Docker's root count: runners write their layers, slot volumes and shared caches there, and
//! a cgroup's `io.stat` also lists other devices, such as zram's swap, whose bytes never reach the drive. A
//! partition counts as its whole disk, which is how `io.stat` keys it; a device-mapper volume counts as itself, not
//! the disks under it, whose `io.stat` lines would count the same bytes again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::metrics::Paths;

/// How long a stored window of the drive's figures spans at most. A daemon's stop stores the window it is in.
pub const WINDOW: Duration = Duration::from_mins(10);

/// A block device, as a cgroup's `io.stat` keys it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Device {
    major: u32,
    minor: u32,
    name: String,
}

/// The disks Docker's root sits on, and its btrfs file system's UUID when it is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disks {
    devices: Vec<Device>,
    btrfs: Option<String>,
}

impl Disks {
    /// What `root`, Docker's root directory, is on, from the mount that holds it: its file system's block devices,
    /// each a partition's disk, and its btrfs UUID. `None` when the mount or its devices can't be read, as without
    /// `/proc/self/mountinfo` or sysfs, or on a file system without a block device.
    pub fn resolve(paths: &Paths, root: &Path) -> Option<Disks> {
        let mountinfo = std::fs::read_to_string(paths.proc.join("self/mountinfo")).ok()?;
        let mount = mountinfo
            .lines()
            .filter_map(Mount::parse)
            .filter(|mount| root.starts_with(&mount.point))
            .max_by_key(|mount| mount.point.components().count())?;
        let class = paths.sys.join("class/block");
        let btrfs = (mount.kind == "btrfs")
            .then(|| btrfs_uuid(paths, &mount.source))
            .flatten();
        let names: Vec<String> = if let Some(uuid) = &btrfs {
            std::fs::read_dir(paths.sys.join("fs/btrfs").join(uuid).join("devices"))
                .ok()?
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        } else {
            let link = paths.sys.join("dev/block").join(&mount.device);
            vec![file_name(&std::fs::canonicalize(link).ok()?)?]
        };
        let mut devices = BTreeSet::new();
        for name in names {
            let dir = class.join(&name);
            // A partition's directory sits in its disk's: the disk is what `io.stat` and the drive's `stat` count.
            let disk = if dir.join("partition").is_file() {
                std::fs::canonicalize(&dir).ok()?.parent()?.to_path_buf()
            } else {
                std::fs::canonicalize(&dir).ok()?
            };
            let number = std::fs::read_to_string(disk.join("dev")).ok()?;
            let (major, minor) = number.trim().split_once(':')?;
            devices.insert(Device {
                major: major.parse().ok()?,
                minor: minor.parse().ok()?,
                name: file_name(&disk)?,
            });
        }
        (!devices.is_empty()).then(|| Disks {
            devices: devices.into_iter().collect(),
            btrfs,
        })
    }

    /// The disks' names, joined by `+`: `nvme2n1`.
    pub fn name(&self) -> String {
        self.devices
            .iter()
            .map(|device| device.name.as_str())
            .collect::<Vec<_>>()
            .join("+")
    }

    /// The bytes and write requests a cgroup and its descendants have sent the disks so far (`io.stat` `wbytes`,
    /// `wios`). `None` when the cgroup has no `io.stat`, as without the io controller or once it is gone; a disk
    /// without a line in it has had none of the cgroup's I/O yet, which is 0.
    pub fn written(&self, cgroup: &Path) -> Option<Written> {
        let stat = std::fs::read_to_string(cgroup.join("io.stat")).ok()?;
        let mut written = Written::default();
        for line in stat.lines() {
            let mut fields = line.split_whitespace();
            let Some((major, minor)) = fields.next().and_then(|device| device.split_once(':'))
            else {
                continue;
            };
            if !self.devices.iter().any(|device| {
                major.parse() == Ok(device.major) && minor.parse() == Ok(device.minor)
            }) {
                continue;
            }
            for field in fields {
                let Some((key, value)) = field.split_once('=') else {
                    continue;
                };
                let Ok(value) = value.parse::<u64>() else {
                    return None;
                };
                match key {
                    "wbytes" => written.bytes = written.bytes.saturating_add(value),
                    "wios" => written.ios = written.ios.saturating_add(value),
                    _ => {}
                }
            }
        }
        Some(written)
    }

    /// The disks', the slice's and the file system's counters now, at `at` (ms).
    pub fn read(&self, paths: &Paths, slice: &Path, at: u64) -> Reading {
        let mut drive = Some(0u64);
        for device in &self.devices {
            // Sectors written, the seventh field, in 512-byte units whatever the disk's own sector size.
            let sectors = std::fs::read_to_string(
                paths
                    .sys
                    .join("class/block")
                    .join(&device.name)
                    .join("stat"),
            )
            .ok()
            .and_then(|stat| stat.split_whitespace().nth(6)?.parse::<u64>().ok());
            drive = drive
                .zip(sectors)
                .map(|(sum, sectors)| sum.saturating_add(sectors.saturating_mul(512)));
        }
        let commits = self.btrfs.as_ref().and_then(|uuid| {
            let stats =
                std::fs::read_to_string(paths.sys.join("fs/btrfs").join(uuid).join("commit_stats"))
                    .ok()?;
            stats
                .lines()
                .find_map(|line| line.strip_prefix("commits ")?.trim().parse().ok())
        });
        Reading {
            at,
            drive,
            slice: self.written(slice).map(|written| written.bytes),
            commits,
            device: self.name(),
        }
    }
}

/// What a cgroup has written to the disks so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    /// Bytes (`wbytes`).
    pub bytes: u64,
    /// Write requests (`wios`).
    pub ios: u64,
}

/// One line of `/proc/self/mountinfo`, as far as `Disks::resolve` reads it.
struct Mount {
    /// `major:minor` of the mounted file system; an anonymous one on btrfs.
    device: String,
    point: PathBuf,
    kind: String,
    source: String,
}

impl Mount {
    fn parse(line: &str) -> Option<Mount> {
        let (front, back) = line.split_once(" - ")?;
        let mut front = front.split(' ');
        let device = front.nth(2)?.to_owned();
        let point = PathBuf::from(unescape(front.nth(1)?));
        let mut back = back.split(' ');
        Some(Mount {
            device,
            point,
            kind: back.next()?.to_owned(),
            source: unescape(back.next()?),
        })
    }
}

/// A mountinfo field with its octal escapes (`\040` for a space) undone.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let code = bytes
            .get(at + 1..at + 4)
            .filter(|_| bytes[at] == b'\\')
            .and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok());
        if let Some(code) = code {
            out.push(code);
            at += 4;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

/// The UUID of the btrfs file system `source` (`/dev/nvme0n1p2`, `/dev/mapper/root`) belongs to: the one whose
/// `devices` lists the device it names.
fn btrfs_uuid(paths: &Paths, source: &str) -> Option<String> {
    let source = Path::new(source);
    // `/dev/mapper/root` is a link to `/dev/dm-0`, the name sysfs lists.
    let name = std::fs::canonicalize(source)
        .ok()
        .and_then(|path| file_name(&path))
        .or_else(|| file_name(source))?;
    std::fs::read_dir(paths.sys.join("fs/btrfs"))
        .ok()?
        .flatten()
        .find(|entry| entry.path().join("devices").join(&name).exists())
        .and_then(|entry| entry.file_name().into_string().ok())
}

/// The drive's, the slice's and the file system's counters at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    at: u64,
    drive: Option<u64>,
    slice: Option<u64>,
    commits: Option<u64>,
    device: String,
}

/// What a window's readings add up to, as the history stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Its first reading, in ms.
    pub start: u64,
    /// Its last reading, in ms.
    pub end: u64,
    /// The disks it read: `nvme2n1`.
    pub device: String,
    /// Bytes the disks wrote; `None` when a reading lacked them.
    pub drive: Option<u64>,
    /// Bytes the runners' slice wrote to them, its containers' and those it had that are gone; `None` when a
    /// reading lacked them.
    pub slice: Option<u64>,
    /// btrfs transaction commits; `None` on another file system or when a reading lacked them.
    pub commits: Option<u64>,
}

/// The window being added up: from its first reading to its last, and what the counters rose by between them.
#[derive(Debug)]
struct Open {
    first: u64,
    last: Reading,
    drive: Option<u64>,
    slice: Option<u64>,
    commits: Option<u64>,
}

impl Open {
    fn new(reading: Reading) -> Open {
        Open {
            first: reading.at,
            drive: Some(0),
            slice: Some(0),
            commits: reading.commits.map(|_| 0),
            last: reading,
        }
    }

    /// Adds what each counter rose by since the last reading. A counter either reading lacks makes the window's
    /// absent; one that fell, as a disk's after it was taken out and put back, adds nothing.
    fn add(&mut self, reading: Reading) {
        let rose = |sum: Option<u64>, before: Option<u64>, now: Option<u64>| {
            let (sum, before, now) = (sum?, before?, now?);
            Some(sum.saturating_add(now.saturating_sub(before)))
        };
        self.drive = rose(self.drive, self.last.drive, reading.drive);
        self.slice = rose(self.slice, self.last.slice, reading.slice);
        self.commits = rose(self.commits, self.last.commits, reading.commits);
        self.last = reading;
    }

    fn close(&self) -> Window {
        Window {
            start: self.first,
            end: self.last.at,
            device: self.last.device.clone(),
            drive: self.drive,
            slice: self.slice,
            commits: self.commits,
        }
    }
}

/// Adds up the readings of the drive into windows of `WINDOW`, each starting where the last ended. Survives neither
/// a restart nor a reload: a restart starts a new window at its first reading, so the time no daemon ran is in
/// none.
#[derive(Debug, Default)]
pub struct Windows {
    open: Option<Open>,
}

impl Windows {
    /// Adds a reading; returns the window it closes, once the window spans `WINDOW`.
    pub fn add(&mut self, reading: Reading) -> Option<Window> {
        let Some(open) = self.open.as_mut() else {
            self.open = Some(Open::new(reading));
            return None;
        };
        if reading.device != open.last.device {
            let closed = open.close();
            self.open = Some(Open::new(reading));
            return Some(closed).filter(|window| window.end > window.start);
        }
        open.add(reading);
        let span = u64::try_from(WINDOW.as_millis()).unwrap_or(u64::MAX);
        if open.last.at.saturating_sub(open.first) < span {
            return None;
        }
        let closed = open.close();
        let last = open.last.clone();
        self.open = Some(Open::new(last));
        Some(closed)
    }

    /// The window being added up, if it spans any time: what a stop stores.
    pub fn close(&mut self) -> Option<Window> {
        self.open
            .take()
            .map(|open| open.close())
            .filter(|window| window.end > window.start)
    }
}

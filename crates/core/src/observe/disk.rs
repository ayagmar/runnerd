//! What the disk under Docker's root writes: which block devices hold that root (`Disks::resolve`, from
//! `/proc/self/mountinfo` and sysfs), the bytes a cgroup wrote to them (`io.stat`), and over windows of
//! `WINDOW` the bytes the drive wrote (`/sys/class/block/<disk>/stat`), the bytes the runners' slice wrote and the
//! btrfs commits of the file system (`/sys/fs/btrfs/<uuid>/commit_stats`), and who else wrote to them by cgroup
//! (`Owners`, ADR 0157); and the free disk Docker has
//! (`free_space`), down through a loop device's backing file. A figure the host does not give, as on
//! cgroup v1, a kernel without the io controller, a file system other than btrfs or an unreadable sysfs, is absent,
//! never 0. Reads files only; it writes nothing to the host and decides nothing.
//!
//! Only the disks under Docker's root count: runners write their layers, slot volumes and shared caches there, and
//! a cgroup's `io.stat` also lists other devices, such as zram's swap, whose bytes never reach the drive. A
//! partition counts as its whole disk, which is how `io.stat` keys it; a device-mapper volume counts as itself, not
//! the disks under it, whose `io.stat` lines would count the same bytes again.
//!
//! When Docker's root is a file system in a file, on a loop device, a cgroup's figure is the loop device's line:
//! what it wrote to that file system. The loop's own writes to its backing file reach the drive charged to the
//! cgroup only some of the time, sometimes to none, so that line is never counted. The drive's bytes and commits
//! follow the backing file (`/sys/block/loopN/loop/backing_file`) to the disk and file system under it.
//!
//! Who wrote: the root cgroup's `io.stat` counts every write to a disk, each child's recursively what its processes
//! and its dead children sent, so the root's less its children's is what no cgroup sent: the kernel's own threads, and
//! btrfs's tree blocks, which it writes through an inode of the root cgroup whoever forced the commit. Writers runnerd
//! names, the runner slice, the host's Docker daemon, containerd and runnerd's own cgroup, come off the top-level
//! cgroup they sit in, `user.slice` or another; what is left of those is other user programs and other system
//! services.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::metrics::Paths;
use crate::api::DriveOwners;

/// How long a stored window of the drive's figures spans at most. A daemon's stop stores the window it is in.
pub const WINDOW: Duration = Duration::from_mins(10);

/// A block device, as a cgroup's `io.stat` keys it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Device {
    major: u32,
    minor: u32,
    name: String,
}

/// How deep a loop device's backing file is followed: a loop on a loop on a disk.
const LOOP_DEPTH: u8 = 4;

/// The devices Docker's root sits on: those a cgroup's writes to it are counted on, the physical disks under them,
/// and the btrfs file system's UUID when the disks hold one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disks {
    /// What `io.stat` counts: the disks, or the loop devices when the root is in a file.
    charged: Vec<Device>,
    /// The physical disks: the drive.
    drive: Vec<Device>,
    btrfs: Option<String>,
}

impl Disks {
    /// What `root`, Docker's root directory, is on, from the mount that holds it: its file system's block devices,
    /// each a partition's disk, and its btrfs UUID; through a loop device, the disks and file system its backing
    /// file is on. `None` when the mount or its devices can't be read, as without `/proc/self/mountinfo` or sysfs,
    /// or on a file system without a block device.
    pub fn resolve(paths: &Paths, root: &Path) -> Option<Disks> {
        let mountinfo = std::fs::read_to_string(paths.proc.join("self/mountinfo")).ok()?;
        Disks::under(paths, &mountinfo, root, LOOP_DEPTH)
    }

    fn under(paths: &Paths, mountinfo: &str, path: &Path, depth: u8) -> Option<Disks> {
        let held = Held::find(paths, mountinfo, path)?;
        let mut btrfs = held.btrfs;
        let mut charged = BTreeSet::new();
        let mut drive = BTreeSet::new();
        for disk in held.disks {
            let device = Device::read(&disk)?;
            if let Some(backing) = backing_file(&disk) {
                let under = Disks::under(paths, mountinfo, &backing, depth.checked_sub(1)?)?;
                drive.extend(under.drive);
                btrfs = under.btrfs;
            } else {
                drive.insert(device.clone());
            }
            charged.insert(device);
        }
        (!charged.is_empty()).then(|| Disks {
            charged: charged.into_iter().collect(),
            drive: drive.into_iter().collect(),
            btrfs,
        })
    }

    /// The loop devices Docker's root is on, joined by `+`, when it is a file system in a file: `loop0`.
    pub fn store(&self) -> Option<String> {
        (self.charged != self.drive).then(|| join(&self.charged))
    }

    /// The physical disks' names, joined by `+`: `nvme2n1`.
    pub fn name(&self) -> String {
        join(&self.drive)
    }

    /// The bytes and write requests a cgroup and its descendants have sent the disks so far (`io.stat` `wbytes`,
    /// `wios`), or the loop devices when the root is in a file: what they wrote to its file system. `None` when the
    /// cgroup has no `io.stat`, as without the io controller or once it is gone; a disk without a line in it has had
    /// none of the cgroup's I/O yet, which is 0.
    pub fn written(&self, cgroup: &Path) -> Option<Written> {
        let stat = std::fs::read_to_string(cgroup.join("io.stat")).ok()?;
        let mut written = Written::default();
        for line in stat.lines() {
            let mut fields = line.split_whitespace();
            let Some((major, minor)) = fields.next().and_then(|device| device.split_once(':'))
            else {
                continue;
            };
            if !self.charged.iter().any(|device| {
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
        for device in &self.drive {
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
        // The daemon's own: what it sent to any block device, its history among it.
        let daemon = std::fs::read_to_string(paths.proc.join("self/io"))
            .ok()
            .and_then(|io| {
                io.lines()
                    .find_map(|line| line.strip_prefix("write_bytes:")?.trim().parse().ok())
            });
        let slice_written = self.written(slice);
        Reading {
            at,
            drive,
            owners: self.owners(paths, slice, slice_written),
            slice: slice_written.map(|written| written.bytes),
            daemon,
            commits,
            device: self.name(),
            store: self.store(),
        }
    }

    /// What each writer had sent the disks so far, by cgroup (`DriveOwners`), the runner slice's `written` taken off
    /// its top-level cgroup's; `None` without the root's `io.stat` or a top-level cgroup's, as without the io
    /// controller. A writer's own figures are read before the top-level ones and those before the root's, so a
    /// counter that rose meanwhile leaves the root at least its children: no difference goes below 0.
    fn owners(&self, paths: &Paths, slice: &Path, written: Option<Written>) -> Option<DriveOwners> {
        let root = &paths.cgroup;
        let own = std::fs::read_to_string(paths.proc.join("self/cgroup"))
            .ok()
            .and_then(|cgroup| own_cgroup(&cgroup));
        let mut placed: Vec<PathBuf> = Vec::new();
        let mut place = |relative: Option<PathBuf>| {
            let relative = relative.filter(|relative| {
                relative.components().count() > 0
                    && !placed
                        .iter()
                        .any(|other| other.starts_with(relative) || relative.starts_with(other))
            })?;
            placed.push(relative.clone());
            Some(relative)
        };
        let slice = place(slice.strip_prefix(root).ok().map(Path::to_path_buf));
        let docker = place(Some(PathBuf::from("system.slice/docker.service")));
        let containerd = place(Some(PathBuf::from("system.slice/containerd.service")));
        let runnerd = place(own);
        let read = |relative: &Option<PathBuf>| {
            relative
                .as_ref()
                .and_then(|relative| Some((relative.clone(), self.written(&root.join(relative))?)))
        };
        let slice = slice.zip(written);
        let (docker, containerd, runnerd) = (read(&docker), read(&containerd), read(&runnerd));
        let mut user = 0u64;
        let mut system = 0u64;
        let mut children = Written::default();
        for entry in std::fs::read_dir(root).ok()?.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            let written = match self.written(&path) {
                Some(written) => written,
                // Removed since it was listed: its bytes are the root's now, as what no cgroup sent.
                None if !path.exists() => continue,
                None => return None,
            };
            children.bytes = children.bytes.saturating_add(written.bytes);
            children.ios = children.ios.saturating_add(written.ios);
            if entry.file_name() == USER_SLICE {
                user = user.saturating_add(written.bytes);
            } else {
                system = system.saturating_add(written.bytes);
            }
        }
        let root = self.written(root)?;
        for (relative, written) in [&slice, &docker, &containerd, &runnerd]
            .into_iter()
            .flatten()
        {
            let bucket = if relative.starts_with(USER_SLICE) {
                &mut user
            } else {
                &mut system
            };
            *bucket = bucket.saturating_sub(written.bytes);
        }
        let bytes =
            |owner: &Option<(PathBuf, Written)>| owner.as_ref().map(|(_, written)| written.bytes);
        Some(DriveOwners {
            runnerd: bytes(&runnerd),
            docker: bytes(&docker),
            containerd: bytes(&containerd),
            system,
            user,
            kernel: root.bytes.saturating_sub(children.bytes),
            kernel_ios: root.ios.saturating_sub(children.ios),
        })
    }
}

/// The top-level cgroup of users' sessions and services.
const USER_SLICE: &str = "user.slice";

/// The cgroup v2 path `/proc/self/cgroup` gives, `0::/user.slice/.../runnerd.service`, below the cgroup root:
/// `user.slice/.../runnerd.service`. `None` on cgroup v1, which has no `0::` line.
fn own_cgroup(cgroup: &str) -> Option<PathBuf> {
    cgroup.lines().find_map(|line| {
        let path = line.strip_prefix("0::")?.trim_end();
        Some(PathBuf::from(path.trim_start_matches('/')))
    })
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
    /// The mount in `mountinfo` that holds `path`: the deepest whose point it is under.
    fn holding(mountinfo: &str, path: &Path) -> Option<Mount> {
        mountinfo
            .lines()
            .filter_map(Mount::parse)
            .filter(|mount| path.starts_with(&mount.point))
            .max_by_key(|mount| mount.point.components().count())
    }

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

/// The file system that holds a path: its btrfs UUID when it is btrfs, and the sysfs directories of the whole disks
/// under it.
struct Held {
    btrfs: Option<String>,
    disks: Vec<PathBuf>,
}

impl Held {
    /// What holds `path`, from `mountinfo` and sysfs; `None` when the mount or a device can't be read.
    fn find(paths: &Paths, mountinfo: &str, path: &Path) -> Option<Held> {
        let mount = Mount::holding(mountinfo, path)?;
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
        let mut disks = Vec::new();
        for name in names {
            let dir = class.join(&name);
            // A partition's directory sits in its disk's: the disk is what `io.stat` and the drive's `stat` count.
            disks.push(if dir.join("partition").is_file() {
                std::fs::canonicalize(&dir).ok()?.parent()?.to_path_buf()
            } else {
                std::fs::canonicalize(&dir).ok()?
            });
        }
        Some(Held { btrfs, disks })
    }
}

/// The file a loop device whose sysfs directory is `disk` reads and writes; `None` for another device. A deleted
/// backing file keeps its path, marked; the mount that held it still holds its blocks.
fn backing_file(disk: &Path) -> Option<PathBuf> {
    let backing = std::fs::read_to_string(disk.join("loop/backing_file")).ok()?;
    let backing = backing.trim_end();
    Some(PathBuf::from(
        backing.strip_suffix(" (deleted)").unwrap_or(backing),
    ))
}

/// The bytes Docker can still write under `root`, its root directory (ADR 0054): `statfs`'s free space for an
/// unprivileged user, `free`, on the file system that holds it. When that file system is in a file, on a loop device,
/// the file can grow only while the file system under it has room too, so it is the least of the free space of each
/// file system down to the disk, nested loops included. A file system under it that can't be found or measured
/// leaves the figure of those above it, never 0. `free` is the one boundary to the host, so tests give their own.
///
/// # Errors
///
/// `free`'s error for `root` itself.
pub fn free_space(
    paths: &Paths,
    root: &Path,
    free: impl Fn(&Path) -> std::io::Result<u64>,
) -> std::io::Result<u64> {
    let own = free(root)?;
    let under = std::fs::read_to_string(paths.proc.join("self/mountinfo"))
        .ok()
        .and_then(|mountinfo| free_under(paths, &mountinfo, root, &free, LOOP_DEPTH));
    Ok(under.map_or(own, |under| own.min(under)))
}

/// The least free space of the file systems that hold the backing files of the loop devices `path`'s file system is
/// on, and of those under them; `None` when it is on no loop device or none of them could be measured.
fn free_under(
    paths: &Paths,
    mountinfo: &str,
    path: &Path,
    free: &impl Fn(&Path) -> std::io::Result<u64>,
    depth: u8,
) -> Option<u64> {
    let depth = depth.checked_sub(1)?;
    let held = Held::find(paths, mountinfo, path)?;
    held.disks
        .iter()
        .filter_map(|disk| backing_file(disk))
        .filter_map(|backing| {
            // The mount point, not the file: runnerd may not be allowed to search the directories above the file.
            let point = Mount::holding(mountinfo, &backing)?.point;
            let own = free(&point).ok();
            let under = free_under(paths, mountinfo, &backing, free, depth);
            own.into_iter().chain(under).min()
        })
        .min()
}

impl Device {
    /// The block device whose sysfs directory is `dir`.
    fn read(dir: &Path) -> Option<Device> {
        let number = std::fs::read_to_string(dir.join("dev")).ok()?;
        let (major, minor) = number.trim().split_once(':')?;
        Some(Device {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
            name: file_name(dir)?,
        })
    }
}

/// Devices' names joined by `+`.
fn join(devices: &[Device]) -> String {
    devices
        .iter()
        .map(|device| device.name.as_str())
        .collect::<Vec<_>>()
        .join("+")
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
    owners: Option<DriveOwners>,
    slice: Option<u64>,
    daemon: Option<u64>,
    commits: Option<u64>,
    device: String,
    store: Option<String>,
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
    /// Bytes the daemon itself wrote (`/proc/self/io`'s `write_bytes`), to any disk; `None` when a reading lacked
    /// them.
    pub daemon: Option<u64>,
    /// btrfs transaction commits; `None` on another file system or when a reading lacked them.
    pub commits: Option<u64>,
    /// Who else wrote to the disks, by cgroup (ADR 0157); `None` when a reading lacked it, and in a window stored
    /// before runnerd read it: not split.
    pub owners: Option<DriveOwners>,
    /// The loop devices Docker's root is on when it is a file system in a file on `device`: `loop0`. The runner
    /// slice's figure is then what it wrote to that file system.
    pub store: Option<String>,
}

/// The window being added up: from its first reading to its last, and what the counters rose by between them.
#[derive(Debug)]
struct Open {
    first: u64,
    last: Reading,
    drive: Option<u64>,
    slice: Option<u64>,
    daemon: Option<u64>,
    commits: Option<u64>,
    owners: Option<DriveOwners>,
}

impl Open {
    fn new(reading: Reading) -> Open {
        Open {
            first: reading.at,
            drive: Some(0),
            slice: Some(0),
            daemon: Some(0),
            commits: reading.commits.map(|_| 0),
            owners: reading.owners.map(|_| DriveOwners {
                runnerd: Some(0),
                docker: Some(0),
                containerd: Some(0),
                ..DriveOwners::default()
            }),
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
        self.daemon = rose(self.daemon, self.last.daemon, reading.daemon);
        self.commits = rose(self.commits, self.last.commits, reading.commits);
        self.owners =
            self.owners
                .zip(self.last.owners.zip(reading.owners))
                .map(|(sum, (before, now))| {
                    let rose = |before: u64, now: u64| now.saturating_sub(before);
                    let rose_some =
                        |before: Option<u64>, now: Option<u64>| Some(now?.saturating_sub(before?));
                    sum.plus(DriveOwners {
                        runnerd: rose_some(before.runnerd, now.runnerd),
                        docker: rose_some(before.docker, now.docker),
                        containerd: rose_some(before.containerd, now.containerd),
                        system: rose(before.system, now.system),
                        user: rose(before.user, now.user),
                        kernel: rose(before.kernel, now.kernel),
                        kernel_ios: rose(before.kernel_ios, now.kernel_ios),
                    })
                });
        self.last = reading;
    }

    fn close(&self) -> Window {
        Window {
            start: self.first,
            end: self.last.at,
            device: self.last.device.clone(),
            store: self.last.store.clone(),
            drive: self.drive,
            slice: self.slice,
            daemon: self.daemon,
            commits: self.commits,
            owners: self.owners,
        }
    }
}

/// Adds up the readings of the drive into windows of `WINDOW`, each starting where the last ended. A reload leaves
/// it be; a restart starts a new window at its first reading, so the time no daemon ran is in none.
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
        if reading.device != open.last.device || reading.store != open.last.store {
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

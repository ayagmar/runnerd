//! What runner jobs and the drive under Docker's root wrote (ADR 0143), as clients read it, and every word they show
//! of it: a job's figure in a table and on its page, and the disk-writes section of `GET /v1/insights`. Sizes are
//! bytes, times seconds since the Unix epoch.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{Tone, duration};
use crate::config::ByteSize;

/// What a job's runner container wrote to the disks under Docker's root, from its cgroup's `io.stat`: what its
/// processes, the Docker Engine inside it and that engine's containers sent the disks from the job's start to the
/// container's last read. Bytes still in the page cache then go to the disk after, on the runner slice's account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskWritten {
    /// Bytes written.
    pub bytes: u64,
    /// Write requests.
    pub ios: u64,
    /// What its container had written to the page cache and the kernel not yet to the disk at its last read: written
    /// after, unless deleted first. `None` where the kernel does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<u64>,
    /// Whether that is all it wrote.
    pub measure: Measure,
}

/// Whether a job's [`DiskWritten`] is all it wrote. A job no reading measured has no figure at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Measure {
    /// Read from before the job's start to within a sample of its container's end.
    Complete,
    /// Part of it was not read: its container's end, as when it ended while no daemon ran, or the counter's start,
    /// as when the container was started again, or its start, as when the counter was not read before the job.
    /// What was read is a floor.
    Partial,
    /// A measure a later release names; read as partial.
    #[serde(other)]
    Unknown,
}

impl DiskWritten {
    /// It in a table cell: `1.2G`, or `≥1.2G` for a partial figure.
    pub fn short(&self) -> String {
        match self.measure {
            Measure::Complete => ByteSize(self.bytes).to_string(),
            Measure::Partial | Measure::Unknown => format!("≥{}", ByteSize(self.bytes)),
        }
    }

    /// It on a job's page: `1.2G in 3402 writes`, then `; 80M more waited to be written back as its container
    /// ended` when some did, or `at least 1.2G in 3402 writes: …` with why a partial figure is one.
    pub fn line(&self) -> String {
        let writes = if self.ios == 1 { "write" } else { "writes" };
        let mut line = match self.measure {
            Measure::Complete => format!("{} in {} {writes}", ByteSize(self.bytes), self.ios),
            Measure::Partial | Measure::Unknown => format!(
                "at least {} in {} {writes}: part of its run was not read",
                ByteSize(self.bytes),
                self.ios
            ),
        };
        if let Some(dirty) = self.dirty.filter(|dirty| *dirty > 0) {
            let _ = write!(
                line,
                "; {} more waited to be written back as its container ended",
                ByteSize(dirty)
            );
        }
        line
    }

    /// What it means: a partial figure is a floor, so it reads in the background.
    pub fn tone(&self) -> Tone {
        match self.measure {
            Measure::Complete => Tone::Neutral,
            Measure::Partial | Measure::Unknown => Tone::Quiet,
        }
    }
}

/// The disk-writes section of [`Insights`](super::Insights): what the jobs that ended in the window wrote, the
/// workflows and jobs that wrote most, and what the drive wrote while the daemon read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskWrites {
    /// The jobs' figures together.
    pub jobs: JobWrites,
    /// The workflows whose jobs wrote most, most first.
    pub workflows: Vec<WriteTop>,
    /// The jobs, by repository, workflow and name, that wrote most, most first.
    pub top_jobs: Vec<WriteTop>,
    /// The drive's figures; `None` when the daemon read none in the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<DriveWrites>,
}

/// What the jobs that ended in a window wrote, each its runner container's [`DiskWritten`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobWrites {
    /// Jobs with a figure.
    pub measured: u32,
    /// Of those, the partial ones.
    pub partial: u32,
    /// Bytes they wrote together.
    pub total: u64,
    /// The median of one job's bytes.
    pub median: u64,
    /// The 90th percentile, by nearest rank.
    pub p90: u64,
    /// What waited to be written back as their containers ended, together.
    pub dirty: u64,
}

/// A workflow, or a job, and what its jobs in the window wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteTop {
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The job's name, for a job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Its jobs with a figure.
    pub jobs: u32,
    /// Bytes they wrote together.
    pub total: u64,
}

/// What the disks under Docker's root wrote while the daemon read them in a window: from `/sys/class/block`, the
/// runner slice's `io.stat`, which keeps what its containers wrote after they are gone, the daemon's own
/// `/proc/self/io`, and btrfs's commit count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveWrites {
    /// The disks: `nvme2n1`.
    pub device: String,
    /// Seconds the daemon read them.
    pub watched: u64,
    /// Bytes they wrote then.
    pub written: u64,
    /// Whose those bytes were.
    pub shares: DriveShares,
    /// btrfs transaction commits; `None` on another file system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<Commits>,
    /// The loop devices Docker's root is on when it is a file system in a file on the disks: `loop0`. Jobs' and the
    /// runner slice's figures are then bytes written to that file system, which the disks write with its journal
    /// and the overhead of the file system under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

/// Whose the drive's bytes were, over the stretches the daemon read the runner slice, or over all it read the drive
/// when it never read the slice: a stretch that missed one figure leaves the others' shares be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveShares {
    /// Seconds these were read over.
    pub watched: u64,
    /// Bytes the drive wrote then.
    pub written: u64,
    /// Of those, the figures of the jobs that ended then.
    pub jobs: u64,
    /// Of those, the runner slice's; `None` without its `io.stat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<u64>,
    /// What the daemon itself wrote then, to any disk; `None` where it could not read its own counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<u64>,
}

/// btrfs's transaction commits over the stretches the daemon read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commits {
    /// Commits.
    pub count: u64,
    /// Seconds they were read over.
    pub watched: u64,
}

/// `part` of `whole` in whole percent, rounded, at most 100.
fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    let share = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    u64::try_from(share.min(100)).unwrap_or(100)
}

impl JobWrites {
    /// Them in a line: `529 jobs wrote 144.7G: median 49M, p90 612M a job`, then `; 12 partial, counted as far as
    /// read` and `; 3.1G more waited to be written back as their containers ended` when so.
    pub fn line(&self) -> String {
        let jobs = if self.measured == 1 { "job" } else { "jobs" };
        let mut line = format!(
            "{} {jobs} wrote {}: median {}, p90 {} a job",
            self.measured,
            ByteSize(self.total),
            ByteSize(self.median),
            ByteSize(self.p90)
        );
        if self.partial > 0 {
            let _ = write!(line, "; {} partial, counted as far as read", self.partial);
        }
        if self.dirty > 0 {
            let _ = write!(
                line,
                "; {} more waited to be written back as their containers ended",
                ByteSize(self.dirty)
            );
        }
        line
    }
}

impl WriteTop {
    /// It in a line: `halyard/api CI / build: 12.4G over 31 jobs, 410M a job`; a workflow's without the job.
    pub fn line(&self) -> String {
        let mut name = self.repository.clone();
        if let Some(workflow) = &self.workflow {
            name.push(' ');
            name.push_str(workflow);
        }
        if let Some(job) = &self.job {
            name.push_str(if self.workflow.is_some() { " / " } else { " " });
            name.push_str(job);
        }
        let jobs = if self.jobs == 1 { "job" } else { "jobs" };
        format!(
            "{name}: {} over {} {jobs}, {} a job",
            ByteSize(self.total),
            self.jobs,
            ByteSize(self.total / u64::from(self.jobs.max(1)))
        )
    }
}

impl DriveWrites {
    /// What a day at the window's rate writes, in bytes.
    pub fn per_day(&self) -> u64 {
        let day = u128::from(self.written) * 86_400 / u128::from(self.watched.max(1));
        u64::try_from(day).unwrap_or(u64::MAX)
    }

    /// Them in lines: what the drive wrote and its daily rate; what the figures count when Docker's root is in a
    /// file on it; whose it was, runner jobs', the rest of the runner
    /// slice's, runnerd's own and the share no one in runnerd owns; and btrfs's commits a second.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} wrote {} over the {} runnerd read it: {} a day at this rate",
            self.device,
            ByteSize(self.written),
            duration(self.watched),
            ByteSize(self.per_day())
        )];
        if let Some(store) = &self.store {
            lines.push(format!(
                "Docker's root is a file system in a file on it ({store}): the jobs' and the runner slice's figures \
                 are bytes written to that file system; {} also wrote its journal and the overhead of the file \
                 system under it",
                self.device
            ));
        }
        lines.push(self.shares.line(self.watched));
        if let Some(commits) = self.commits {
            let tenths = u128::from(commits.count) * 10 / u128::from(commits.watched.max(1));
            lines.push(format!(
                "btrfs committed {}.{} times a second",
                tenths / 10,
                tenths % 10
            ));
        }
        lines
    }
}

impl DriveShares {
    /// Them in a line, saying the stretch they cover when it is shorter than the `watched` seconds the drive was
    /// read: `Runner jobs 25%; the rest of the runner slice 8% (...); runnerd itself 1%; no owner runnerd knows 66%`.
    /// No share passes the whole.
    pub fn line(&self, watched: u64) -> String {
        let whole = self.written;
        // Jobs that ended in the stretch may count what they wrote before it: never past the whole.
        let counted = self.jobs.min(whole);
        let mut line = if self.watched < watched {
            format!(
                "Over the {} runnerd read the runner slice: runner jobs",
                duration(self.watched)
            )
        } else {
            "Runner jobs".to_owned()
        };
        let _ = write!(line, " {}%", percent(counted, whole));
        let owned = match self.slice {
            Some(slice) => {
                let slice = slice.clamp(counted, whole);
                let _ = write!(
                    line,
                    "; the rest of the runner slice {}% (seeds, the mirror, runners before and after their jobs, \
                     write-back after a container ended)",
                    percent(slice - counted, whole)
                );
                slice
            }
            None => counted,
        };
        let daemon = self.daemon.map(|daemon| daemon.min(whole - owned));
        if let Some(daemon) = daemon {
            let _ = write!(line, "; runnerd itself {}%", percent(daemon, whole));
        }
        let _ = write!(
            line,
            "; no owner runnerd knows {}%",
            percent(whole - owned - daemon.unwrap_or(0), whole)
        );
        if self.slice.is_none() {
            line.push_str(", the runner slice's share unread");
        }
        line
    }
}

/// A part of the disk-writes section, under its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteSection {
    /// `Jobs`, `Drive`, `Workflows that wrote most`, `Jobs that wrote most`.
    pub title: &'static str,
    /// Its lines, in order.
    pub lines: Vec<String>,
}

impl DiskWrites {
    /// The section's title: `Disk writes`.
    pub const TITLE: &'static str = "Disk writes";

    /// Its parts in order, each with what it says, none empty: the jobs' figures, the drive's, and the workflows and
    /// the jobs that wrote most.
    pub fn sections(&self) -> Vec<WriteSection> {
        let mut sections = Vec::new();
        if self.jobs.measured > 0 {
            sections.push(WriteSection {
                title: "Jobs",
                lines: vec![self.jobs.line()],
            });
        }
        if let Some(drive) = &self.drive {
            sections.push(WriteSection {
                title: "Drive",
                lines: drive.lines(),
            });
        }
        for (title, top) in [
            ("Workflows that wrote most", &self.workflows),
            ("Jobs that wrote most", &self.top_jobs),
        ] {
            if !top.is_empty() {
                sections.push(WriteSection {
                    title,
                    lines: top.iter().map(WriteTop::line).collect(),
                });
            }
        }
        sections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = 1 << 20;
    const G: u64 = 1 << 30;

    #[test]
    fn a_partial_figure_reads_as_a_floor_and_says_why() {
        let mut written = DiskWritten {
            bytes: 640 * M,
            ios: 1,
            dirty: Some(0),
            measure: Measure::Complete,
        };
        assert_eq!(written.short(), "640M");
        assert_eq!(written.line(), "640M in 1 write");
        written.measure = Measure::Partial;
        written.dirty = Some(96 * M);
        assert_eq!(written.short(), "≥640M");
        assert_eq!(written.tone(), Tone::Quiet, "a floor is in the background");
        assert_eq!(
            written.line(),
            "at least 640M in 1 write: part of its run was not read; 96M more waited to be written back as its \
             container ended"
        );
    }

    #[test]
    fn the_drive_s_lines_share_its_bytes_out_without_passing_the_whole() {
        let shares = DriveShares {
            watched: 6 * 3600,
            written: 600 * G,
            jobs: 150 * G,
            slice: Some(200 * G),
            daemon: Some(6 * G),
        };
        let drive = DriveWrites {
            device: "nvme0n1".to_owned(),
            watched: 6 * 3600,
            written: 600 * G,
            shares,
            commits: Some(Commits {
                count: 43_200,
                watched: 6 * 3600,
            }),
            store: None,
        };
        assert_eq!(
            drive.lines(),
            [
                "nvme0n1 wrote 600G over the 6h 00m runnerd read it: 2.34T a day at this rate",
                "Runner jobs 25%; the rest of the runner slice 8% (seeds, the mirror, runners before and after \
                 their jobs, write-back after a container ended); runnerd itself 1%; no owner runnerd knows 66%",
                "btrfs committed 2.0 times a second",
            ]
        );
        // Jobs that ended in the window may count bytes written before it: never past the whole.
        let over = DriveWrites {
            shares: DriveShares {
                slice: Some(100 * G),
                jobs: 700 * G,
                ..shares
            },
            commits: None,
            ..drive.clone()
        };
        assert_eq!(
            over.lines()[1],
            "Runner jobs 100%; the rest of the runner slice 0% (seeds, the mirror, runners before and after their \
             jobs, write-back after a container ended); runnerd itself 0%; no owner runnerd knows 0%"
        );
        assert_eq!(over.lines().len(), 2, "no commits line off btrfs");
        let in_a_file = DriveWrites {
            store: Some("loop0".to_owned()),
            ..over.clone()
        };
        assert_eq!(
            in_a_file.lines()[1],
            "Docker's root is a file system in a file on it (loop0): the jobs' and the runner slice's figures are \
             bytes written to that file system; nvme0n1 also wrote its journal and the overhead of the file system \
             under it"
        );
        // The slice read over only part of the time: its shares say over which.
        let part = DriveWrites {
            shares: DriveShares {
                watched: 3600,
                written: 100 * G,
                jobs: 50 * G,
                slice: Some(60 * G),
                daemon: None,
            },
            ..drive
        };
        assert_eq!(
            part.lines()[1],
            "Over the 1h 00m runnerd read the runner slice: runner jobs 50%; the rest of the runner slice 10% (seeds, \
             the mirror, runners before and after their jobs, write-back after a container ended); no owner runnerd \
             knows 40%"
        );
    }
}

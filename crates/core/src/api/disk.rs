//! What runner jobs and the drive under Docker's root wrote (ADR 0143), as clients read it, and every word they show
//! of it: a job's figure in a table and on its page, and the disk-writes section of `GET /v1/insights`. Sizes are
//! bytes, times seconds since the Unix epoch.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{JOBS_HEADING, LabelledFigure, Tone, duration};
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

/// Sets each part's `percent` of `whole`, whose bytes they add up to, so that the percents add up to 100: each its
/// share rounded down, and the points left over to the parts with the largest remainders, the first of equal ones.
/// Rounding each alone could show 1%, 1%, 1% and 99%.
fn largest_remainder(parts: &mut [DriveShare], whole: u64) {
    if whole == 0 {
        return;
    }
    let whole = u128::from(whole);
    let mut left = 100u128;
    let mut remainders = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter_mut().enumerate() {
        let scaled = u128::from(part.bytes) * 100;
        let floor = scaled / whole;
        part.percent = u64::try_from(floor).unwrap_or(100);
        left = left.saturating_sub(floor);
        remainders.push((scaled % whole, index));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, index) in remainders
        .into_iter()
        .take(usize::try_from(left).unwrap_or(0))
    {
        if let Some(part) = parts.get_mut(index) {
            part.percent += 1;
        }
    }
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
        for note in [self.partial_note(), self.dirty_note()]
            .into_iter()
            .flatten()
        {
            let _ = write!(line, "; {note}");
        }
        line
    }

    /// `12 partial, counted as far as read`, when a job was read only in part.
    fn partial_note(&self) -> Option<String> {
        (self.partial > 0).then(|| format!("{} partial, counted as far as read", self.partial))
    }

    /// `3.1G more waited to be written back as their containers ended`, when any did.
    fn dirty_note(&self) -> Option<String> {
        (self.dirty > 0).then(|| {
            format!(
                "{} more waited to be written back as their containers ended",
                ByteSize(self.dirty)
            )
        })
    }
}

impl WriteTop {
    /// Its name: `halyard/api CI / build`; a workflow's without the job.
    pub fn name(&self) -> String {
        let mut name = self.repository.clone();
        if let Some(workflow) = &self.workflow {
            name.push(' ');
            name.push_str(workflow);
        }
        if let Some(job) = &self.job {
            name.push_str(if self.workflow.is_some() { " / " } else { " " });
            name.push_str(job);
        }
        name
    }

    /// What one of its jobs wrote on average, in bytes.
    pub fn per_job(&self) -> u64 {
        self.total / u64::from(self.jobs.max(1))
    }

    /// What its total is over, beside it: `31 jobs, 410M a job`.
    pub fn note(&self) -> String {
        let jobs = if self.jobs == 1 { "job" } else { "jobs" };
        format!("{} {jobs}, {} a job", self.jobs, ByteSize(self.per_job()))
    }

    /// It in a line: `halyard/api CI / build: 12.4G over 31 jobs, 410M a job`; a workflow's without the job.
    pub fn line(&self) -> String {
        format!(
            "{}: {} over {}",
            self.name(),
            ByteSize(self.total),
            self.note()
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
    /// file on it; whose it was, runner jobs', the rest of the runner slice's, runnerd's own and the share no one in
    /// runnerd owns, unless it wrote nothing over the shares; and btrfs's commits a second.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} wrote {} over the {} runnerd read it: {} a day at this rate",
            self.device,
            ByteSize(self.written),
            duration(self.watched),
            ByteSize(self.per_day())
        )];
        lines.extend(self.store_note());
        if !self.shares.is_empty() {
            lines.push(self.shares.line(self.watched));
        }
        if let Some(commits) = self.commits {
            lines.push(format!(
                "btrfs committed {} times a second",
                commits.per_second()
            ));
        }
        lines
    }
}

impl DriveWrites {
    /// What its bytes are called among the figures: `nvme0n1 wrote`.
    fn label(&self) -> String {
        format!("{} wrote", self.device)
    }

    /// What the figures count when Docker's root is a file system in a file on the drive.
    fn store_note(&self) -> Option<String> {
        self.store.as_ref().map(|store| {
            format!(
                "Docker's root is a file system in a file on it ({store}): the jobs' and the runner slice's figures \
                 are bytes written to that file system; {} also wrote its journal and the overhead of the file \
                 system under it",
                self.device
            )
        })
    }
}

impl Commits {
    /// Commits a second, to a tenth: `2.6`.
    pub fn per_second(&self) -> String {
        let tenths = u128::from(self.count) * 10 / u128::from(self.watched.max(1));
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

/// Whose a share of what the drive wrote was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareOwner {
    /// The runner jobs that ended then: what their containers wrote.
    Jobs,
    /// The rest of the runner slice: seeds, the mirror, runners before and after their jobs, write-back after a
    /// container ended.
    Slice,
    /// runnerd itself.
    Daemon,
    /// No owner runnerd knows.
    Unknown,
}

impl ShareOwner {
    /// Its name, lower case: `runner jobs`, `the rest of the runner slice`, `runnerd itself`, `no owner runnerd
    /// knows`.
    pub fn label(self) -> &'static str {
        match self {
            ShareOwner::Jobs => "runner jobs",
            ShareOwner::Slice => "the rest of the runner slice",
            ShareOwner::Daemon => "runnerd itself",
            ShareOwner::Unknown => "no owner runnerd knows",
        }
    }
}

/// One owner's share of what the drive wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveShare {
    /// Whose.
    pub owner: ShareOwner,
    /// Its bytes.
    pub bytes: u64,
    /// Its part of the whole, in whole percent; the parts' add up to 100.
    pub percent: u64,
}

impl DriveShares {
    /// Whether they have nothing to show: the drive wrote nothing over them, as on the first read after a start, so
    /// every part is 0 bytes and its percents would not add up to 100. A client draws no shares then.
    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    /// Whose the bytes were, in the order the line says them: runner jobs', the rest of the runner slice's when its
    /// `io.stat` was read, runnerd's own when read, and the rest, no owner runnerd knows. Their bytes add up to the
    /// whole; jobs that ended in the stretch may count what they wrote before it, so no share passes the whole.
    pub fn parts(&self) -> Vec<DriveShare> {
        let whole = self.written;
        let share = |owner, bytes| DriveShare {
            owner,
            bytes,
            percent: 0,
        };
        let counted = self.jobs.min(whole);
        let mut parts = vec![share(ShareOwner::Jobs, counted)];
        let owned = match self.slice {
            Some(slice) => {
                let slice = slice.clamp(counted, whole);
                parts.push(share(ShareOwner::Slice, slice - counted));
                slice
            }
            None => counted,
        };
        let daemon = self.daemon.map(|daemon| daemon.min(whole - owned));
        if let Some(daemon) = daemon {
            parts.push(share(ShareOwner::Daemon, daemon));
        }
        parts.push(share(
            ShareOwner::Unknown,
            whole - owned - daemon.unwrap_or(0),
        ));
        largest_remainder(&mut parts, whole);
        parts
    }

    /// Them in a line, saying the stretch they cover when it is shorter than the `watched` seconds the drive was
    /// read: `Runner jobs 25%; the rest of the runner slice 8% (...); runnerd itself 1%; no owner runnerd knows 66%`.
    /// No share passes the whole.
    pub fn line(&self, watched: u64) -> String {
        let mut line = self
            .stretch_note(watched)
            .map(|stretch| format!("{stretch}: "))
            .unwrap_or_default();
        let said: Vec<String> = self
            .parts()
            .iter()
            .map(|part| {
                let note = if part.owner == ShareOwner::Slice {
                    " (seeds, the mirror, runners before and after their jobs, write-back after a container ended)"
                } else {
                    ""
                };
                format!("{} {}%{note}", part.owner.label(), part.percent)
            })
            .collect();
        line.push_str(&said.join("; "));
        if self.slice.is_none() {
            line.push_str(", ");
            line.push_str(Self::SLICE_UNREAD);
        }
        sentence_case(&line)
    }

    /// What the shares say when the runner slice was not read.
    const SLICE_UNREAD: &'static str = "the runner slice's share unread";

    /// `over the 1h 00m runnerd read the runner slice`, when that is shorter than the `watched` seconds the drive was
    /// read.
    fn stretch_note(&self, watched: u64) -> Option<String> {
        (self.watched < watched).then(|| {
            format!(
                "over the {} runnerd read the runner slice",
                duration(self.watched)
            )
        })
    }
}

/// `line` starting as a sentence does, its first letter a capital.
fn sentence_case(line: &str) -> String {
    let mut chars = line.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// A part of the disk-writes section, under its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteSection {
    /// `Jobs`, `Drive`, [`DiskWrites::WORKFLOWS_TITLE`], [`DiskWrites::TOP_JOBS_TITLE`].
    pub title: &'static str,
    /// Its lines, in order.
    pub lines: Vec<String>,
}

impl DiskWrites {
    /// The section's title: `Disk writes`.
    pub const TITLE: &'static str = "Disk writes";
    /// What the drive's shares are called over them.
    pub const SHARES_TITLE: &'static str = "Who wrote to the drive";
    /// What the workflows that wrote most are called.
    pub const WORKFLOWS_TITLE: &'static str = "Workflows that wrote most";
    /// What the jobs that wrote most are called.
    pub const TOP_JOBS_TITLE: &'static str = "Jobs that wrote most";
    /// What the jobs' bytes are called among the figures.
    const JOBS_WROTE: &'static str = "Jobs wrote";

    /// What the figures leave out that changes how they read, a line each, in the order the figures show, each
    /// starting with the label of the figure or the title it is about: jobs read only in part, bytes that waited to be
    /// written back, Docker's root as a file system in a file on the drive, and the drive's shares read over a
    /// shorter stretch than the drive or without the runner slice's, when there are shares to show. The same words as
    /// [`DiskWrites::sections`].
    pub fn caveats(&self) -> Vec<String> {
        let mut caveats = Vec::new();
        if self.jobs.measured > 0 {
            caveats.extend(
                [self.jobs.partial_note(), self.jobs.dirty_note()]
                    .into_iter()
                    .flatten()
                    .map(|note| format!("{}: {note}", Self::JOBS_WROTE)),
            );
        }
        if let Some(drive) = &self.drive {
            caveats.extend(
                drive
                    .store_note()
                    .map(|note| format!("{}: {note}", drive.label())),
            );
            caveats.extend(
                (!drive.shares.is_empty())
                    .then(|| drive.shares.stretch_note(drive.watched))
                    .flatten()
                    .into_iter()
                    .chain(
                        (!drive.shares.is_empty() && drive.shares.slice.is_none())
                            .then(|| DriveShares::SLICE_UNREAD.to_owned()),
                    )
                    .map(|note| format!("{}: {note}", Self::SHARES_TITLE)),
            );
        }
        caveats
    }

    /// Its figures, in the order they show: the jobs' bytes, count, median and p90 when a job was measured; the
    /// drive's bytes, daily rate and btrfs commits when it was read.
    pub fn figures(&self) -> Vec<LabelledFigure> {
        let mut figures = Vec::new();
        let jobs = &self.jobs;
        if jobs.measured > 0 {
            figures.extend([
                LabelledFigure::new(Self::JOBS_WROTE, ByteSize(jobs.total).to_string()),
                LabelledFigure::new(JOBS_HEADING, jobs.measured.to_string()),
                LabelledFigure::new("Median a job", ByteSize(jobs.median).to_string()),
                LabelledFigure::new("p90 a job", ByteSize(jobs.p90).to_string()),
            ]);
        }
        if let Some(drive) = &self.drive {
            figures.push(LabelledFigure::new(
                drive.label(),
                ByteSize(drive.written).to_string(),
            ));
            figures.push(LabelledFigure::new(
                "A day at this rate",
                ByteSize(drive.per_day()).to_string(),
            ));
            if let Some(commits) = drive.commits {
                figures.push(LabelledFigure::new(
                    "btrfs commits a second",
                    commits.per_second(),
                ));
            }
        }
        figures
    }

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
            (Self::WORKFLOWS_TITLE, &self.workflows),
            (Self::TOP_JOBS_TITLE, &self.top_jobs),
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

    /// A client draws the shares as one bar: a share whose bytes ran past the whole, or that left a gap, would draw
    /// a bar longer or shorter than the drive's.
    #[test]
    fn the_drive_s_shares_as_parts_fill_the_whole_and_match_its_line() {
        let shares = DriveShares {
            watched: 3600,
            written: 100 * G,
            // Jobs that ended in the stretch count what they wrote before it.
            jobs: 150 * G,
            slice: Some(90 * G),
            daemon: Some(G),
        };
        let parts = shares.parts();
        assert_eq!(
            parts.iter().map(|part| part.bytes).sum::<u64>(),
            100 * G,
            "{parts:?}"
        );
        assert_eq!(parts[0].owner, ShareOwner::Jobs);
        assert_eq!(parts[0].percent, 100);
        assert_eq!(
            shares.line(3600),
            "Runner jobs 100%; the rest of the runner slice 0% (seeds, the mirror, runners before and after their \
             jobs, write-back after a container ended); runnerd itself 0%; no owner runnerd knows 0%"
        );
    }

    /// A key under the share bar reads as a whole: rounding each share alone showed 1%, 1%, 1% and 99%.
    #[test]
    fn the_drive_s_shares_add_up_to_100_percent_by_their_largest_remainders() {
        let percents = |shares: DriveShares| -> Vec<u64> {
            shares.parts().iter().map(|part| part.percent).collect()
        };
        let halves = DriveShares {
            watched: 3600,
            written: 1000,
            jobs: 5,
            slice: Some(10),
            daemon: Some(5),
        };
        assert_eq!(
            percents(halves),
            [1, 1, 0, 98],
            "the first of equal remainders take the points left"
        );
        let thirds = DriveShares {
            watched: 3600,
            written: 3,
            jobs: 1,
            slice: Some(2),
            daemon: Some(1),
        };
        assert_eq!(percents(thirds), [34, 33, 33, 0]);
        let none = DriveShares {
            watched: 3600,
            written: 0,
            jobs: 0,
            slice: None,
            daemon: None,
        };
        assert_eq!(percents(none), [0, 0]);
    }

    /// The figures every client shows of the section, worded once: a client that worded its own would drift.
    #[test]
    fn disk_writes_read_as_labelled_figures_and_each_top_as_its_note() {
        let top = WriteTop {
            repository: "halyard/ledger".to_owned(),
            workflow: Some("CI".to_owned()),
            job: None,
            jobs: 1,
            total: 410 * M,
        };
        assert_eq!(top.note(), "1 job, 410M a job");
        assert_eq!(top.line(), "halyard/ledger CI: 410M over 1 job, 410M a job");
        let disk = DiskWrites {
            jobs: JobWrites {
                measured: 31,
                partial: 0,
                total: 12 * G,
                median: 120 * M,
                p90: 780 * M,
                dirty: 0,
            },
            workflows: vec![top],
            top_jobs: Vec::new(),
            drive: Some(DriveWrites {
                device: "nvme0n1".to_owned(),
                watched: 43_200,
                written: 40 * G,
                shares: DriveShares {
                    watched: 43_200,
                    written: 40 * G,
                    jobs: 12 * G,
                    slice: None,
                    daemon: None,
                },
                commits: Some(Commits {
                    count: 112_320,
                    watched: 43_200,
                }),
                store: None,
            }),
        };
        let shown: Vec<(String, String)> = disk
            .figures()
            .into_iter()
            .map(|figure| (figure.label, figure.value))
            .collect();
        let expected = [
            ("Jobs wrote", "12G"),
            ("Jobs", "31"),
            ("Median a job", "120M"),
            ("p90 a job", "780M"),
            ("nvme0n1 wrote", "40G"),
            ("A day at this rate", "80G"),
            ("btrfs commits a second", "2.6"),
        ]
        .map(|(label, value)| (label.to_owned(), value.to_owned()));
        assert_eq!(shown, expected);
        let unmeasured = DiskWrites {
            jobs: JobWrites::default(),
            drive: None,
            ..disk
        };
        assert!(
            unmeasured.figures().is_empty(),
            "no figures without a measured job or a read drive"
        );
    }

    /// The figures leave out what the sentences say about how to read them: a client that shows only the figures
    /// would show bytes from a partly read job, or shares of a drive behind a file system in a file, as whole.
    #[test]
    fn the_caveats_say_what_the_figures_leave_out_in_the_sentences_words() {
        let shares = DriveShares {
            watched: 3600,
            written: 100 * G,
            jobs: 50 * G,
            slice: None,
            daemon: None,
        };
        let disk = DiskWrites {
            jobs: JobWrites {
                measured: 529,
                partial: 12,
                total: 145 * G,
                median: 49 * M,
                p90: 612 * M,
                dirty: 3 * G,
            },
            workflows: Vec::new(),
            top_jobs: Vec::new(),
            drive: Some(DriveWrites {
                device: "nvme0n1".to_owned(),
                watched: 2 * 3600,
                written: 200 * G,
                shares,
                commits: None,
                store: Some("loop0".to_owned()),
            }),
        };
        assert_eq!(
            disk.caveats(),
            [
                "Jobs wrote: 12 partial, counted as far as read",
                "Jobs wrote: 3G more waited to be written back as their containers ended",
                "nvme0n1 wrote: Docker's root is a file system in a file on it (loop0): the jobs' and the runner slice's \
                 figures are bytes written to that file system; nvme0n1 also wrote its journal and the overhead of the \
                 file system under it",
                "Who wrote to the drive: over the 1h 00m runnerd read the runner slice",
                "Who wrote to the drive: the runner slice's share unread",
            ]
        );
        // The sentences say the same, in the same words.
        let sentences: Vec<String> = disk
            .sections()
            .into_iter()
            .flat_map(|section| section.lines)
            .collect();
        let said = sentences.join(" ").to_lowercase();
        for caveat in disk.caveats() {
            let note = [
                DiskWrites::JOBS_WROTE,
                DiskWrites::SHARES_TITLE,
                "nvme0n1 wrote",
            ]
            .iter()
            .find_map(|title| caveat.strip_prefix(&format!("{title}: ")))
            .unwrap_or_else(|| panic!("{caveat} names its figure"));
            assert!(said.contains(&note.to_lowercase()), "{note}: {sentences:?}");
        }
        // Read whole, the figures need no caveat.
        let whole = DiskWrites {
            jobs: JobWrites {
                partial: 0,
                dirty: 0,
                ..disk.jobs
            },
            drive: disk.drive.map(|drive| DriveWrites {
                watched: 3600,
                store: None,
                shares: DriveShares {
                    slice: Some(60 * G),
                    ..shares
                },
                ..drive
            }),
            ..disk
        };
        assert_eq!(whole.caveats(), Vec::<String>::new(), "nothing to say");
    }

    /// A drive that wrote nothing, as on the first read after a start, has shares of 0 bytes whose percents do not
    /// add up: no client draws them, and core says nothing about them.
    #[test]
    fn a_drive_that_wrote_nothing_has_no_shares_to_show_or_speak_of() {
        let quiet = DriveShares {
            watched: 3600,
            written: 0,
            jobs: 0,
            slice: None,
            daemon: None,
        };
        assert!(quiet.is_empty(), "nothing to draw");
        assert!(
            !DriveShares {
                written: G,
                ..quiet
            }
            .is_empty(),
            "the drive wrote"
        );
        let drive = DriveWrites {
            device: "nvme0n1".to_owned(),
            watched: 2 * 3600,
            written: 0,
            shares: quiet,
            commits: None,
            store: None,
        };
        assert_eq!(
            drive.lines(),
            ["nvme0n1 wrote 0B over the 2h 00m runnerd read it: 0B a day at this rate"],
            "no shares line of zeros"
        );
        let disk = DiskWrites {
            jobs: JobWrites::default(),
            workflows: Vec::new(),
            top_jobs: Vec::new(),
            drive: Some(drive),
        };
        assert_eq!(
            disk.caveats(),
            Vec::<String>::new(),
            "neither a shorter stretch nor an unread slice is said of shares not shown"
        );
    }
}

//! The names every client gives the same thing: its views and the columns of a table of jobs. A client lays a name
//! out, upper-cases it or fits it; it does not spell its own.

/// A group of pages of the web dashboard, a group of views of `runnerctl tui`, in the order every client lists them:
/// what runs now, the jobs, what the history says, the machine's room and the daemon's settings (ADR 0132).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum View {
    /// Budget, slots, the queue, what needs attention and the latest jobs.
    Overview,
    /// Finished and running jobs, and the records of failed ones.
    Jobs,
    /// What the history says: jobs that got slower, are flaky or near their cap.
    Insights,
    /// What each profile's jobs use and what to set, and the caches.
    Capacity,
    /// The daemon's config and the host's checks.
    Settings,
}

impl View {
    /// Every view, in order.
    pub const ALL: [View; 5] = [
        View::Overview,
        View::Jobs,
        View::Insights,
        View::Capacity,
        View::Settings,
    ];

    /// What clients call it.
    pub fn title(self) -> &'static str {
        match self {
            View::Overview => "Overview",
            View::Jobs => "Jobs",
            View::Insights => "Insights",
            View::Capacity => "Capacity",
            View::Settings => "Settings",
        }
    }

    /// Its sections, in order: the first is what opening the view shows. A view of one page has none.
    pub fn sections(self) -> &'static [Section] {
        match self {
            View::Overview | View::Insights => &[],
            View::Jobs => &[Section::History, Section::Records],
            View::Capacity => &[Section::Sizing, Section::Caches],
            View::Settings => &[Section::Config],
        }
    }
}

/// A page inside a [`View`] of several, as every client names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Finished and running jobs, filtered.
    History,
    /// The records of failed jobs.
    Records,
    /// What each profile's jobs use and what to set.
    Sizing,
    /// The cache volumes and the build cache.
    Caches,
    /// The daemon's config and the host's checks.
    Config,
}

impl Section {
    /// What clients call it.
    pub fn title(self) -> &'static str {
        match self {
            Section::History => "History",
            Section::Records => "Records",
            Section::Sizing => "Sizing",
            Section::Caches => "Caches",
            Section::Config => "Config",
        }
    }

    /// The view it is a section of.
    pub fn view(self) -> View {
        match self {
            Section::History | Section::Records => View::Jobs,
            Section::Sizing | Section::Caches => View::Capacity,
            Section::Config => View::Settings,
        }
    }
}

/// A column of a table of jobs or of records, as the history, `runnerctl jobs` and the records name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// GitHub's number for the job.
    Id,
    /// How the job ended, or that it is running.
    Outcome,
    /// Its workflow and name.
    Job,
    /// Its repository.
    Repository,
    /// Its branch.
    Branch,
    /// When GitHub queued it.
    Queued,
    /// How long it waited for a runner.
    Waited,
    /// How long it ran.
    Ran,
    /// The highest memory its sampled use reached.
    MemoryPeak,
    /// Its average CPU, in cores.
    Cpu,
    /// When a record of the failed job was made.
    Made,
    /// How much disk a record takes.
    Size,
}

impl Column {
    /// What clients call it.
    pub fn title(self) -> &'static str {
        match self {
            Column::Id => "ID",
            Column::Outcome => "Outcome",
            Column::Job => "Job",
            Column::Repository => "Repository",
            Column::Branch => "Branch",
            Column::Queued => "Queued",
            Column::Waited => "Waited",
            Column::Ran => "Ran",
            Column::MemoryPeak => "Memory peak",
            Column::Cpu => "CPU",
            Column::Made => "Made",
            Column::Size => "Size",
        }
    }
}

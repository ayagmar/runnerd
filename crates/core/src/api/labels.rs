//! The names every client gives the same thing: its views and the columns of a table of jobs. A client lays a name
//! out, upper-cases it or fits it; it does not spell its own.

/// A page of the web dashboard, a view of `runnerctl tui`, in the order every client lists them: the views that
/// look at the present, then the history and what it advises, then the host and its config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Budget, slots, the queue and the latest jobs.
    Overview,
    /// Finished and running jobs, filtered.
    History,
    /// The records of failed jobs.
    Records,
    /// What each profile's jobs use and what to set.
    Sizing,
    /// What the history says: jobs that got slower, are flaky or near their cap.
    Insights,
    /// The cache volumes and the build cache.
    Caches,
    /// The daemon's config.
    Config,
}

impl View {
    /// Every view, in order.
    pub const ALL: [View; 7] = [
        View::Overview,
        View::History,
        View::Records,
        View::Sizing,
        View::Insights,
        View::Caches,
        View::Config,
    ];

    /// What clients call it.
    pub fn title(self) -> &'static str {
        match self {
            View::Overview => "Overview",
            View::History => "History",
            View::Records => "Records",
            View::Sizing => "Sizing",
            View::Insights => "Insights",
            View::Caches => "Caches",
            View::Config => "Config",
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

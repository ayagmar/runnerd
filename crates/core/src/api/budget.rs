//! What the memory budget means as every client shows it: its line, what is reserved and free of it, what of it no
//! runner may start in, and the parts a bar draws. Plain derivations of [`Status`]; clients only lay them out.

use super::{AdmissionPolicy, Budget, HostMemory, Status, percent};
use crate::config::ByteSize;

/// A figure and the words that follow it, such as `8G` and `of 24G reserved in runnerd.slice`, for a client that
/// sets the figure apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Such as `8G`.
    pub figure: String,
    /// What follows it, such as `of 24G reserved in runnerd.slice`.
    pub text: String,
}

impl Reading {
    /// The figure and its words as one line.
    pub fn line(&self) -> String {
        format!("{} {}", self.figure, self.text)
    }
}

impl Budget {
    /// What is reserved of what, as every client shows it: `8G` `of 24G reserved in runnerd.slice`, or without
    /// `MemoryMax` `8G` `reserved in runnerd.slice, which has no MemoryMax`; while pooling takes something off, `36.8G`
    /// `of 46G counted in ci.slice by predicted peaks: 42.8G reserved, 6G pooled`; how runners are admitted
    /// (`Budget::admission_note`) follows the slice.
    pub fn memory_reading(&self) -> Reading {
        let of = self
            .memory_max
            .map_or_else(String::new, |max| format!("of {} ", ByteSize(max)));
        let verb = if self.pooled.is_some() {
            "counted"
        } else {
            "reserved"
        };
        let mut text = format!("{of}{verb} in {}", self.slice);
        if let Some(note) = self.admission_note() {
            text = format!("{text} {note}");
        }
        if let Some(pooled) = &self.pooled {
            let reserved = ByteSize(self.memory_in_use);
            text = format!(
                "{text}: {reserved} reserved, {} pooled",
                ByteSize(pooled.credit)
            );
        }
        if self.memory_max.is_none() {
            text.push_str(", which has no MemoryMax");
        }
        Reading {
            figure: ByteSize(self.counted()).to_string(),
            text,
        }
    }

    /// What `MemoryMax` goes by, in bytes: `memory_in_use`, less what pooling takes off it (ADR 0099).
    pub fn counted(&self) -> u64 {
        self.pooled
            .map_or(self.memory_in_use, |pooled| pooled.counted)
    }

    /// The budget's clauses in the order clients show them: memory (`Budget::memory_reading`), then CPU
    /// (`CpuBudget::line`) and the host (`Budget::host_line`) when the daemon reports them.
    pub fn clauses(&self) -> Vec<String> {
        let mut clauses = vec![self.memory_reading().line()];
        clauses.extend(self.cpu.map(|cpu| cpu.line()));
        clauses.extend(self.host_line());
        clauses
    }

    /// The host's line as clients show it, such as `host: 31G of 64G available, 1.5% stalled for memory; leaves the
    /// slice 40G`; on a host with swap, `host: 31G of 64G available, swap 12.1G of 15.5G used, …` (ADR 0101); when that
    /// is less room than `MemoryMax` leaves (`Budget::host_withheld`), `…; leaves the slice 25G
    /// of its 46G, so 6.5G is free`, or past `MemoryMax` (`Budget::host_share`) `…; leaves the slice 47.5G for the 44.3G
    /// reserved there, so 3.2G is free`, and without `MemoryMax`, where the host alone bounds it, `…; leaves the slice
    /// 25G, so 6.5G is free`; while it stalls, `…; stalling, so no runner starts`. While the slice is held
    /// (`Budget::held`), nothing is free, so it says no figure of it. While the host's limit takes a credit (ADR 0108),
    /// what it counts follows: `…; what runs counts 23G: it uses 14G and keeps 9G for growth`
    /// (`HostBudget::growth_clause`). With programs outside the slice large enough to
    /// name, `…; programs outside CI hold 30G, most java 9.3G` (`HostMemory::outside_line`). `None` without a recent
    /// reading of the host.
    pub fn host_line(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        let mut line = format!("host: {}", host.memory.available_line());
        if let Some(swap) = host.memory.swap {
            line = format!(
                "{line}, swap {} of {} used",
                ByteSize(swap.used()),
                ByteSize(swap.total)
            );
        }
        if let Some(full) = host.memory.full.filter(|full| full.avg10 > 0) {
            line = format!("{line}, {} stalled for memory", percent(full.avg10));
        }
        let free = match host.spare {
            0 => "nothing".to_owned(),
            spare => ByteSize(spare).to_string(),
        };
        let limit = ByteSize(host.limit);
        line = if host.stalled {
            format!("{line}; stalling, so no runner starts")
        } else if self.closed() {
            format!("{line}; leaves the slice {limit}")
        } else {
            match self.memory_max {
                Some(max) if self.host_withheld().is_some() => format!(
                    "{line}; leaves the slice {}, so {free} is free",
                    self.host_share(host.limit, max)
                ),
                Some(_) => format!("{line}; leaves the slice {limit}"),
                None => format!("{line}; leaves the slice {limit}, so {free} is free"),
            }
        };
        if let Some(growth) = host.growth_clause(self.memory_in_use) {
            line = format!(
                "{line}; what runs counts {}: {growth}",
                ByteSize(host.counts(self.memory_in_use))
            );
        }
        if !host.memory.holders.is_empty() {
            line = format!("{line}; {}", host.memory.outside_line());
        }
        Some(line)
    }

    /// What the host's `limit` leaves the slice of `max`, its `MemoryMax`: `25G of its 46G`. A limit past `max` holds
    /// back room only where `max` reads a larger credit than the limit (ADR 0099, 0108). Nothing leaves a slice more
    /// than it has, so it says what the limit goes by: `47.5G for the 44.3G reserved there`, or while the host's limit
    /// takes a credit, `47.5G for the 40G counted there`.
    fn host_share(&self, limit: u64, max: u64) -> String {
        if limit > max {
            let host = self.host.as_ref();
            let (counts, verb) = match host.filter(|host| host.credit.is_some()) {
                Some(host) => (host.counts(self.memory_in_use), "counted"),
                None => (self.memory_in_use, "reserved"),
            };
            format!(
                "{} for the {} {verb} there",
                ByteSize(limit),
                ByteSize(counts)
            )
        } else {
            format!("{} of its {}", ByteSize(limit), ByteSize(max))
        }
    }

    /// Whether measured admission holds the slice at its high-water mark (`Budget::held`), so no runner starts and
    /// nothing is free.
    fn closed(&self) -> bool {
        self.held && self.admission == AdmissionPolicy::Measured
    }

    /// What the host's memory holds back of the room `MemoryMax` leaves beside what runs, in bytes: that room less
    /// what the host has to spare (`HostBudget::spare`), when the host leaves less. `None` while it leaves as much,
    /// while it stalls (its line says no runner starts), without a recent reading of it, and without `MemoryMax`.
    pub fn host_withheld(&self) -> Option<u64> {
        let host = self.host.as_ref().filter(|host| !host.stalled)?;
        let remaining = self.remaining()?;
        (host.spare < remaining).then(|| remaining - host.spare)
    }

    /// The budget as one line, its clauses joined by `; `.
    pub fn line(&self) -> String {
        self.clauses().join("; ")
    }

    /// What `MemoryMax` leaves beside what the running containers count (`Budget::counted`), in bytes: what a new
    /// runner must fit in. `None` without `MemoryMax`.
    pub fn remaining(&self) -> Option<u64> {
        self.memory_max
            .map(|max| max.saturating_sub(self.counted()))
    }
}

impl HostMemory {
    /// What the host has available of its memory, as the budget's host clause and doctor say it: `31G of 64G
    /// available`, and while it can still grow to its ceiling (`HostMemory::growable`), `1.5G of 2.1G available,
    /// growing to 24G`.
    pub fn available_line(&self) -> String {
        let line = format!(
            "{} of {} available",
            ByteSize(self.available),
            ByteSize(self.total)
        );
        match self.growable() {
            0 => line,
            _ => format!("{line}, growing to {}", ByteSize(self.grown())),
        }
    }
}

impl Status {
    /// The budget as a bar draws it: a block per slot as large as its cap (under measured admission, a block per
    /// runner as large as what it reserves), the mirror, seed containers and shells, then what is free and what of
    /// the room no runner may start in; and the whole the blocks are scaled against.
    pub fn budget_parts(&self) -> BudgetParts {
        let budget = &self.budget;
        let slots: Vec<SlotPart> = self
            .profiles
            .iter()
            .enumerate()
            .flat_map(|(profile, status)| {
                status.slots.iter().filter_map(move |slot| {
                    let memory = match budget.admission {
                        AdmissionPolicy::Caps => match &slot.runner {
                            Some(runner) if runner.reserved > 0 => runner.reserved,
                            _ => status.memory,
                        },
                        AdmissionPolicy::Measured => slot.runner.as_ref()?.reserved,
                    };
                    Some(SlotPart {
                        profile,
                        slot: slot.slot,
                        memory,
                        reserved: slot.runner.is_some(),
                    })
                })
            })
            .collect();
        let mirror = (budget.mirror_memory > 0).then_some(HeldPart {
            kind: HeldKind::Mirror,
            memory: budget.mirror_memory,
        });
        let seeds = budget.seed_memory.iter().map(|seed| HeldPart {
            kind: HeldKind::Seed {
                profile: seed.profile.clone(),
            },
            memory: seed.memory,
        });
        let shells = budget.shell_memory.iter().map(|shell| HeldPart {
            kind: HeldKind::Shell { job: shell.job },
            memory: shell.memory,
        });
        let held: Vec<HeldPart> = mirror.into_iter().chain(seeds).chain(shells).collect();
        let drawn: u64 = slots
            .iter()
            .map(|part| part.memory)
            .chain(held.iter().map(|part| part.memory))
            .sum();
        let whole = budget.memory_max.unwrap_or(drawn).max(drawn);
        let unclaimed: u64 = slots
            .iter()
            .filter(|part| !part.reserved)
            .map(|part| part.memory)
            .sum();
        let (free, closed) = budget.room_parts(drawn, unclaimed);
        BudgetParts {
            slots,
            held,
            free,
            closed,
            whole,
        }
    }
}

impl Budget {
    /// The bar's room past `drawn` bytes of blocks, of which `unclaimed` are idle slots' caps: what is free, and what
    /// of it no runner may start in.
    fn room_parts(&self, drawn: u64, unclaimed: u64) -> (Option<FreePart>, Option<ClosedPart>) {
        let Some(max) = self.memory_max else {
            return (None, None);
        };
        let room = max.saturating_sub(drawn);
        // While no runner starts, all the room is closed.
        let shut = if self.closed() {
            Some((
                ClosedKind::Held,
                "closed: the slice is past 90% of its MemoryMax, so no runner starts",
            ))
        } else if self.host.as_ref().is_some_and(|host| host.stalled) {
            Some((
                ClosedKind::Stalled,
                "closed: the host's programs stall waiting for memory, so no runner starts",
            ))
        } else {
            None
        };
        if let Some((kind, text)) = shut {
            let closed = (room > 0).then(|| ClosedPart {
                memory: room,
                kind,
                reading: Reading {
                    figure: ByteSize(room).to_string(),
                    text: text.to_owned(),
                },
            });
            return (None, closed);
        }
        let withheld = self.host_withheld();
        // The host's part ends the bar, within the free room: under caps, idle slots' blocks stay drawn though the
        // host may hold their room too.
        let host = withheld
            .map(|withheld| withheld.min(room))
            .filter(|&memory| memory > 0);
        let (host_limit, spare) = self
            .host
            .as_ref()
            .map_or((0, 0), |host| (host.limit, host.spare));
        let memory = room - host.unwrap_or(0);
        let mut text = match (unclaimed, withheld) {
            (0, None) => format!("of {} free", ByteSize(max)),
            (_, None) => format!(
                "of {} free past every slot's cap; {} is free beside what runs",
                ByteSize(max),
                ByteSize(self.remaining().unwrap_or(0))
            ),
            (0, Some(_)) => format!(
                "free; the host leaves the slice {}",
                self.host_share(host_limit, max)
            ),
            (_, Some(_)) => format!(
                "free past every slot's cap; {} is free beside what runs, as the host leaves the slice {}",
                ByteSize(spare),
                self.host_share(host_limit, max)
            ),
        };
        // The blocks stay what runners reserve, so what pooling adds is said and not drawn.
        if let Some(pooled) = &self.pooled {
            text = format!("{text}, and pooling adds {}", ByteSize(pooled.credit));
        }
        let free = (memory > 0).then(|| FreePart {
            memory,
            reading: Reading {
                figure: ByteSize(memory).to_string(),
                text,
            },
        });
        let host = host.map(|memory| ClosedPart {
            memory,
            kind: ClosedKind::Host,
            reading: Reading {
                figure: ByteSize(memory).to_string(),
                // Past `MemoryMax` the plain count holds this back, not the host's programs (`Budget::host_share`).
                text: if host_limit > max {
                    format!(
                        "held back by what runs reserves: the host leaves the slice {}",
                        self.host_share(host_limit, max)
                    )
                } else {
                    format!(
                        "{} for its other programs and page cache, which leave the slice {}",
                        ClosedKind::Host.name(),
                        ByteSize(host_limit)
                    )
                },
            },
        });
        (free, host)
    }
}

/// The budget as a bar draws it, from `Status::budget_parts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetParts {
    /// A block per slot, in config order: every slot under caps, only those with a runner under measured admission.
    pub slots: Vec<SlotPart>,
    /// The mirror, each seed container, then each shell open in a record.
    pub held: Vec<HeldPart>,
    /// What `MemoryMax` leaves past every block, less what is closed; `None` without `MemoryMax`, without room past
    /// the blocks and the closed part, and while no runner starts: the slice is held (`Budget::held`) or the host
    /// stalls.
    pub free: Option<FreePart>,
    /// What of the room past every block no runner may start in, drawn after what is free: all of it while the slice
    /// is held or the host stalls, else what the host's memory holds back (`Budget::host_withheld`); `None` while neither closes any.
    pub closed: Option<ClosedPart>,
    /// What the blocks are scaled against: `MemoryMax`, or the blocks' sum when larger or without it.
    pub whole: u64,
}

/// A slot's block of the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotPart {
    /// Its profile's index in `Status::profiles`.
    pub profile: usize,
    /// The slot, 1-based.
    pub slot: usize,
    /// Its size in bytes: what its runner reserves, or a free slot's cap.
    pub memory: u64,
    /// Whether a runner reserves it; a free slot's block under caps is room its next runner takes, still free.
    pub reserved: bool,
}

/// Memory the budget holds for something other than a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldPart {
    /// What holds it.
    pub kind: HeldKind,
    /// Its cap in bytes.
    pub memory: u64,
}

impl HeldPart {
    /// Its name under its block: `mirror`, `node seed` or `job 42 shell`.
    pub fn name(&self) -> String {
        match &self.kind {
            HeldKind::Mirror => "mirror".to_owned(),
            HeldKind::Seed { profile } => format!("{profile} seed"),
            HeldKind::Shell { job } => format!("job {job} shell"),
        }
    }

    /// What holds it, after its size: `held by the mirror`, `held by node's seed container` or `held by the shell
    /// in job 42's record`.
    pub fn holder(&self) -> String {
        match &self.kind {
            HeldKind::Mirror => "held by the mirror".to_owned(),
            HeldKind::Seed { profile } => format!("held by {profile}'s seed container"),
            HeldKind::Shell { job } => format!("held by the shell in job {job}'s record"),
        }
    }
}

/// What holds a part of the budget beside the slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldKind {
    /// The registry mirror.
    Mirror,
    /// A profile's seed container, while it reseeds.
    Seed {
        /// The profile it reseeds.
        profile: String,
    },
    /// A shell open in a job's record.
    Shell {
        /// The job.
        job: u64,
    },
}

/// What the bar leaves free past every block, less what the host holds back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreePart {
    /// In bytes.
    pub memory: u64,
    /// Its figure and what it means, such as `8G` `of 24G free`, or while the host binds `6.52G` `free; the host
    /// leaves the slice 25.27G of its 46G` (`Budget::host_share`).
    pub reading: Reading,
}

/// Room past the bar's blocks that no runner may start in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedPart {
    /// In bytes.
    pub memory: u64,
    /// What closes it.
    pub kind: ClosedKind,
    /// Its figure and what it means, such as `20.73G` `held by the host for its other programs and page cache,
    /// which leave the slice 25.27G`, or past `MemoryMax` `1.5G` `held back by what runs reserves: the host leaves the
    /// slice 24.5G for the 22G reserved there`.
    pub reading: Reading,
}

/// What closes room past the bar's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedKind {
    /// The host's other programs and its page cache need it.
    Host,
    /// The slice is past 90% of its `MemoryMax` under measured admission (`Budget::held`).
    Held,
    /// The host's programs stall waiting for memory (`HostBudget::stalled`).
    Stalled,
}

impl ClosedKind {
    /// Its name under its block or in a key: `held by the host`, `closed past 90%` or `closed while the host
    /// stalls`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Host => "held by the host",
            Self::Held => "closed past 90%",
            Self::Stalled => "closed while the host stalls",
        }
    }
}

#[cfg(test)]
mod tests;

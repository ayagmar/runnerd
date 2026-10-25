//! What the memory budget means as every client shows it: its line, what is reserved and free of it, what the host's
//! memory holds back of it, and the parts a bar draws. Plain derivations of [`Status`]; clients only lay them out.

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
    /// `MemoryMax` `8G` `reserved in runnerd.slice, which has no MemoryMax`; how runners are admitted
    /// (`Budget::admission_note`) follows the slice.
    pub fn memory_reading(&self) -> Reading {
        let of = self
            .memory_max
            .map_or_else(String::new, |max| format!("of {} ", ByteSize(max)));
        let mut text = format!("{of}reserved in {}", self.slice);
        if let Some(note) = self.admission_note() {
            text = format!("{text} {note}");
        }
        if self.memory_max.is_none() {
            text.push_str(", which has no MemoryMax");
        }
        Reading {
            figure: ByteSize(self.memory_in_use).to_string(),
            text,
        }
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
    /// slice 40G`; when that is less room than `MemoryMax` leaves (`Budget::host_withheld`), `…; leaves the slice 25G
    /// of its 46G, so 6.5G is free`; while it stalls, `…; stalling, so no runner starts`. With programs outside the
    /// slice large enough to name, `…; programs outside CI hold 30G, most java 9.3G` (`HostMemory::outside_line`).
    /// `None` without a recent reading of the host.
    pub fn host_line(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        let mut line = format!("host: {}", host.memory.available_line());
        if let Some(full) = host.memory.full.filter(|full| full.avg10 > 0) {
            line = format!("{line}, {} stalled for memory", percent(full.avg10));
        }
        line = match (host.stalled, self.memory_max.zip(self.host_withheld())) {
            (true, _) => format!("{line}; stalling, so no runner starts"),
            (false, Some((max, _))) => {
                let free = match host.spare {
                    0 => "nothing".to_owned(),
                    spare => ByteSize(spare).to_string(),
                };
                format!(
                    "{line}; leaves the slice {} of its {}, so {free} is free",
                    ByteSize(host.limit),
                    ByteSize(max),
                )
            }
            (false, None) => format!("{line}; leaves the slice {}", ByteSize(host.limit)),
        };
        if !host.memory.holders.is_empty() {
            line = format!("{line}; {}", host.memory.outside_line());
        }
        Some(line)
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

    /// What `MemoryMax` leaves beside every running container's reservation, in bytes: what a new runner must fit
    /// in. `None` without `MemoryMax`.
    pub fn remaining(&self) -> Option<u64> {
        self.memory_max
            .map(|max| max.saturating_sub(self.memory_in_use))
    }
}

impl HostMemory {
    /// What the host has available of its memory, as the budget's host clause and doctor say it: `31G of 64G
    /// available`.
    pub fn available_line(&self) -> String {
        format!(
            "{} of {} available",
            ByteSize(self.available),
            ByteSize(self.total)
        )
    }
}

impl Status {
    /// The budget as a bar draws it: a block per slot as large as its cap (under measured admission, a block per
    /// runner as large as what it reserves), the mirror, seed containers and shells, then what is free and what of
    /// it the host's memory holds back; and the whole the blocks are scaled against.
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
        let (free, host) = budget.room_parts(drawn, unclaimed);
        BudgetParts {
            slots,
            held,
            free,
            host,
            whole,
        }
    }
}

impl Budget {
    /// The bar's room past `drawn` bytes of blocks, of which `unclaimed` are idle slots' caps: what is free, and
    /// what of it the host's memory holds back.
    fn room_parts(&self, drawn: u64, unclaimed: u64) -> (Option<FreePart>, Option<HostPart>) {
        let withheld = self.host_withheld();
        let room = self.memory_max.map_or(0, |max| max.saturating_sub(drawn));
        // The host's part ends the bar, within the free room: under caps, idle slots' blocks stay drawn though the
        // host may hold their room too.
        let host = withheld
            .map(|withheld| withheld.min(room))
            .filter(|&memory| memory > 0);
        let (host_limit, spare) = self
            .host
            .as_ref()
            .map_or((0, 0), |host| (host.limit, host.spare));
        let free = self.memory_max.and_then(|max| {
            let memory = room - host.unwrap_or(0);
            let text = match (unclaimed, withheld) {
                (0, None) => format!("of {} free", ByteSize(max)),
                (_, None) => format!(
                    "of {} free past every slot's cap; {} is free beside what runs",
                    ByteSize(max),
                    ByteSize(self.remaining().unwrap_or(0))
                ),
                (0, Some(_)) => format!(
                    "free; the host leaves the slice {} of its {}",
                    ByteSize(host_limit),
                    ByteSize(max)
                ),
                (_, Some(_)) => format!(
                    "free past every slot's cap; {} is free beside what runs, as the host leaves the slice {} of \
                     its {}",
                    ByteSize(spare),
                    ByteSize(host_limit),
                    ByteSize(max)
                ),
            };
            (memory > 0).then(|| FreePart {
                memory,
                reading: Reading {
                    figure: ByteSize(memory).to_string(),
                    text,
                },
            })
        });
        let host = host.map(|memory| HostPart {
            memory,
            reading: Reading {
                figure: ByteSize(memory).to_string(),
                text: format!(
                    "{} for its other programs and page cache, which leave the slice {}",
                    HostPart::NAME,
                    ByteSize(host_limit)
                ),
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
    /// What `MemoryMax` leaves past every block, less what the host holds back; `None` without `MemoryMax` or room
    /// past them and the host's part.
    pub free: Option<FreePart>,
    /// What the host's memory holds back of the room past every block (`Budget::host_withheld`), drawn after what is
    /// free; `None` while the host leaves the slice as much as `MemoryMax`.
    pub host: Option<HostPart>,
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
    /// leaves the slice 25.27G of its 46G`.
    pub reading: Reading,
}

/// What the host's memory holds back of the bar's free room: no runner may start in it while the host's other
/// programs and its page cache need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPart {
    /// In bytes.
    pub memory: u64,
    /// Its figure and what it means, such as `20.73G` `held by the host for its other programs and page cache,
    /// which leave the slice 25.27G`.
    pub reading: Reading,
}

impl HostPart {
    /// Its name under its block or in a key: `held by the host`.
    pub const NAME: &'static str = "held by the host";
}

#[cfg(test)]
mod tests;

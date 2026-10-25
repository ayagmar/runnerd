//! What a config check says of the memory budget as every client shows it: its verdict, what the admission rule
//! holds to `MemoryMax` and what that leaves, the blocks a preview draws, and doctor's check. Derived from
//! [`api::ConfigCheck`] and the [`Config`] it checked; clients only lay them out.

use super::{ByteSize, Config};
use crate::api::{AdmissionPolicy, Check, CheckStatus, ConfigCheck, Tone};

/// A config check's verdict, from `ConfigCheck::verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Valid, and what its admission rule holds fits `MemoryMax`.
    Fits,
    /// Valid, but the slice has no `MemoryMax`, or it could not be read: nothing holds runners to a budget.
    NoBudget,
    /// Not valid: what its admission rule holds is more than `MemoryMax`.
    DoesNotFit,
    /// Not valid for another reason, given in `ConfigCheck::message`: it does not parse, a setting is wrong, or the
    /// slice could not be read.
    Invalid,
}

impl Verdict {
    /// As a heading says it: `Fits the budget`, `Valid, with no MemoryMax to fit`, `Does not fit` or `Not valid`.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Fits => "Fits the budget",
            Verdict::NoBudget => "Valid, with no MemoryMax to fit",
            Verdict::DoesNotFit => "Does not fit",
            Verdict::Invalid => "Not valid",
        }
    }

    /// What it means for the reader.
    pub fn tone(self) -> Tone {
        match self {
            Verdict::Fits => Tone::Good,
            Verdict::NoBudget => Tone::Warn,
            Verdict::DoesNotFit | Verdict::Invalid => Tone::Bad,
        }
    }
}

/// What a checked config's admission rule holds to `MemoryMax`, from `ConfigCheck::budget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckBudget {
    /// The admission rule it was checked under.
    pub admission: AdmissionPolicy,
    /// What the rule must fit in `MemoryMax`, in bytes.
    pub needed: u64,
    /// The slice's `MemoryMax`; `None` when unlimited or unknown.
    pub memory_max: Option<u64>,
    /// Every slot and the mirror at their caps at once, in bytes, when known.
    pub memory_caps: Option<u64>,
}

impl CheckBudget {
    /// What `needed` is, as a figure's label: `Memory caps` or `Largest runner + mirror`.
    pub fn needed_label(&self) -> &'static str {
        match self.admission {
            AdmissionPolicy::Caps => "Memory caps",
            AdmissionPolicy::Measured => "Largest runner + mirror",
        }
    }

    /// Whether `needed` is more than `MemoryMax`.
    pub fn over(&self) -> bool {
        self.memory_max.is_some_and(|max| self.needed > max)
    }

    /// What `MemoryMax` leaves past `needed`, or how far `needed` goes past it. `None` without `MemoryMax`.
    pub fn remainder(&self) -> Option<Remainder> {
        let max = self.memory_max?;
        Some(if self.over() {
            Remainder {
                label: "Over by",
                memory: self.needed - max,
            }
        } else {
            Remainder {
                label: "Left",
                memory: max - self.needed,
            }
        })
    }

    /// Under measured admission, what that means for the slots: `Admission is measured: runners reserve their
    /// predicted peaks, and slots only bound how many run at once. Every slot at its cap at once would take 40G.`
    pub fn admission_note(&self) -> Option<String> {
        if self.admission != AdmissionPolicy::Measured {
            return None;
        }
        let mut note = "Admission is measured: runners reserve their predicted peaks, and slots only bound how many \
                        run at once."
            .to_owned();
        if let Some(caps) = self.memory_caps {
            note = format!(
                "{note} Every slot at its cap at once would take {}.",
                ByteSize(caps)
            );
        }
        Some(note)
    }
}

/// What `MemoryMax` leaves past a check's budget, or how far the budget goes past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remainder {
    /// `Left` or `Over by`.
    pub label: &'static str,
    /// In bytes.
    pub memory: u64,
}

impl ConfigCheck {
    /// Its verdict: whether it is valid, and whether its budget fits.
    pub fn verdict(&self) -> Verdict {
        let over = self.budget(None).is_some_and(|budget| budget.over());
        match (self.valid, over) {
            (false, true) => Verdict::DoesNotFit,
            (false, false) => Verdict::Invalid,
            (true, _) if self.memory_max.is_none() => Verdict::NoBudget,
            (true, _) => Verdict::Fits,
        }
    }

    /// What its admission rule holds to `MemoryMax`, when known: from the check, or from a daemon before it said,
    /// from `parsed`, the config it checked. `None` when neither knows, as when the caps overflow.
    pub fn budget(&self, parsed: Option<&Config>) -> Option<CheckBudget> {
        let (admission, needed) = match (self.memory_needed, parsed) {
            (Some(needed), _) => (self.admission, needed),
            (None, Some(config)) => (
                config.host.admission.policy(),
                config.memory_needed().ok()?.0,
            ),
            (None, None) if self.admission == AdmissionPolicy::Caps => {
                (self.admission, self.memory_caps?)
            }
            (None, None) => return None,
        };
        Some(CheckBudget {
            admission,
            needed,
            memory_max: self.memory_max,
            memory_caps: self.memory_caps,
        })
    }

    /// Doctor's check of the config the daemon runs with: it passes with its message, or with its
    /// warnings, or fails with why.
    pub fn doctor_check(&self) -> Check {
        if !self.valid {
            return Check {
                name: "config".to_owned(),
                status: CheckStatus::Fail,
                detail: self.message.clone(),
                fix: Some("change runnerd.toml or the slice's limits".to_owned()),
            };
        }
        let detail = if self.warnings.is_empty() {
            self.message.clone()
        } else {
            format!(
                "{}, with warnings: {}",
                self.message,
                self.warnings.join("; ")
            )
        };
        Check {
            name: "config".to_owned(),
            status: CheckStatus::Pass,
            detail,
            fix: None,
        }
    }
}

/// A profile's slots or the mirror, as a budget preview draws it, from `Config::budget_blocks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetBlock {
    /// What it is: `jvm ×2` or `mirror`.
    pub label: String,
    /// In bytes.
    pub memory: u64,
    /// The profile's index in the config's order; `None` for the mirror.
    pub profile: Option<usize>,
}

impl Config {
    /// What `admission` holds to `MemoryMax`, a block per profile: under caps, every slot of each profile at its
    /// cap; under measured, one runner of the largest profile. The mirror with either. `None` when the caps
    /// overflow, as `Config::memory_caps` refuses them.
    pub fn budget_blocks(&self, admission: AdmissionPolicy) -> Option<Vec<BudgetBlock>> {
        let profiles = self
            .profiles
            .iter()
            .enumerate()
            .map(|(index, (name, profile))| (index, name, profile));
        let mut blocks: Vec<BudgetBlock> = match admission {
            AdmissionPolicy::Caps => profiles
                .map(|(index, name, profile)| {
                    Some(BudgetBlock {
                        label: format!("{name} ×{}", profile.slots),
                        memory: profile.memory.0.checked_mul(u64::from(profile.slots))?,
                        profile: Some(index),
                    })
                })
                .collect::<Option<_>>()?,
            AdmissionPolicy::Measured => profiles
                .max_by_key(|(_, _, profile)| profile.memory.0)
                .map(|(index, name, profile)| BudgetBlock {
                    label: format!("{name} ×1"),
                    memory: profile.memory.0,
                    profile: Some(index),
                })
                .into_iter()
                .collect(),
        };
        if self.caches.mirror.enabled {
            blocks.push(BudgetBlock {
                label: "mirror".to_owned(),
                memory: self.mirror_memory(),
                profile: None,
            });
        }
        let total = blocks
            .iter()
            .try_fold(0u64, |total, block| total.checked_add(block.memory));
        total.map(|_| blocks)
    }

    /// Validates the parsed config against `memory_max`, the slice's `MemoryMax`: what `Config::check` answers once
    /// the text parses and the slice is read, and what doctor checks the config the daemon runs with by.
    pub fn check_against(&self, memory_max: Option<ByteSize>) -> ConfigCheck {
        let figures = self
            .validate(memory_max)
            .and_then(|warnings| Ok((warnings, self.memory_caps()?, self.memory_needed()?)));
        match figures {
            Ok((warnings, total, needed)) => {
                let message = match (self.host.admission, memory_max) {
                    (super::Admission::Measured, Some(max)) => format!(
                        "valid under admission = \"measured\": the largest runner with the mirror needs {needed} \
                         of the {max} MemoryMax; every slot at its cap at once would take {total}"
                    ),
                    _ => format!("valid, memory caps sum to {total}"),
                };
                ConfigCheck {
                    valid: true,
                    message,
                    warnings: warnings.iter().map(ToString::to_string).collect(),
                    admission: self.host.admission.policy(),
                    memory_caps: Some(total.0),
                    memory_needed: Some(needed.0),
                    memory_max: memory_max.map(|max| max.0),
                    revision: None,
                }
            }
            Err(error) => invalid(error.to_string(), Some(self), memory_max),
        }
    }
}

/// A check that is not valid for `message`, with what `config`, when it parsed, says of its budget.
pub(super) fn invalid(
    message: String,
    config: Option<&Config>,
    max: Option<ByteSize>,
) -> ConfigCheck {
    ConfigCheck {
        valid: false,
        message,
        warnings: Vec::new(),
        admission: config
            .map(|config| config.host.admission.policy())
            .unwrap_or_default(),
        memory_caps: config
            .and_then(|config| config.memory_caps().ok())
            .map(|caps| caps.0),
        memory_needed: config
            .and_then(|config| config.memory_needed().ok())
            .map(|need| need.0),
        memory_max: max.map(|max| max.0),
        revision: None,
    }
}

#[cfg(test)]
mod tests;

//! The confirmations this daemon issued and the job actions being sent to GitHub, at most one per workflow run (ADR
//! 0134). A confirmation is an opaque random token bound to what its preview showed and the config revision it was
//! shown under; it lasts a few minutes and is sent once: the same token while its flight is in the air joins that
//! flight, and after it landed it is refused. Concurrent requests for one run meet here, so a double click sends one
//! request. It holds nothing across a restart, so a confirmation from before one is unknown; it never sends anything
//! itself.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::api::{ActionResult, ActionTarget, JobAction, RunSeen, Superseded};
use crate::sync::lock;

/// The most confirmations kept: past it the oldest goes, as if it ran out.
const ISSUED_MAX: usize = 256;

/// A workflow run of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunKey {
    repository: String,
    run: u64,
}

/// What a confirmation was issued for: the target its preview showed, the run's attempt in it, and the config
/// revision it was shown under.
#[derive(Debug, Clone)]
pub(super) struct Issued {
    /// What, of which job, on which run and attempt.
    pub(super) target: ActionTarget,
    /// The policy revision of the preview.
    pub(super) revision: u64,
}

/// How a flight ended.
#[derive(Debug, Clone)]
pub(super) enum Landed {
    /// GitHub was asked, or the request ended unknown.
    Done(ActionResult),
    /// Nothing was sent: the run was not as the confirmation showed it, and the requests get a new preview of it.
    Repreview {
        /// The run as read before the send.
        run: RunSeen,
        /// Why the confirmation was not sent.
        why: Superseded,
    },
}

/// One confirmation and what became of it.
struct Confirmation {
    issued: Issued,
    at: Instant,
    used: Use,
}

#[derive(Clone, Copy)]
enum Use {
    /// Not sent yet.
    Open,
    /// Boarded the flight with this ID.
    Boarded(u64),
}

/// What one flight asks: the action, from which job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Asked {
    action: JobAction,
    job: u64,
}

/// One action in flight and where its answer goes.
struct Flight {
    id: u64,
    asked: Asked,
    answer: watch::Sender<Option<Landed>>,
}

/// The confirmations and flights. Only its methods write it, each in one critical section.
struct Book {
    issued: HashMap<String, Confirmation>,
    runs: HashMap<RunKey, Flight>,
    next_flight: u64,
}

/// The confirmations issued and the actions in flight. A `Landing` has it take its run's entry out when dropped.
pub(super) struct Flights {
    /// How long a confirmation lasts.
    lasts: Duration,
    // Locked with `sync::lock`: every critical section is a few lookups, inserts or removes.
    book: Mutex<Book>,
}

/// What a confirmation got.
pub(super) enum Redeemed {
    /// It holds: the request joins the run's flights, to send what it was issued for.
    Board {
        /// How.
        boarding: Boarding,
        /// What it was issued for.
        issued: Issued,
    },
    /// It does not: the request gets a new preview, and nothing is sent.
    Stale(Superseded),
}

/// How a request joins the run's flights.
pub(super) enum Boarding {
    /// Nothing was in flight for the run: this request sends it, and lands it once GitHub answered.
    Lead {
        /// Takes the run's entry out when dropped.
        landing: Landing,
        /// Where the answer comes.
        answer: watch::Receiver<Option<Landed>>,
    },
    /// The same action from the same job is in flight: this request gets its answer.
    Follow(watch::Receiver<Option<Landed>>),
    /// Another action is in flight for the run.
    Busy(JobAction),
}

/// The lead's hold on its run's entry. Dropped without landing, as by a panic or the runtime's end, the entry goes and
/// the requests waiting see the channel close.
pub(super) struct Landing {
    flights: Arc<Flights>,
    key: RunKey,
    id: u64,
}

impl Flights {
    /// Confirmations that last `lasts`.
    pub(super) fn new(lasts: Duration) -> Flights {
        Flights {
            lasts,
            book: Mutex::new(Book {
                issued: HashMap::new(),
                runs: HashMap::new(),
                next_flight: 0,
            }),
        }
    }

    /// A new confirmation for `issued`, at `now`.
    pub(super) fn issue(&self, issued: Issued, now: Instant) -> io::Result<String> {
        let token = token()?;
        lock(&self.book).issue(token.clone(), issued, now, self.lasts);
        Ok(token)
    }

    /// Takes `token` for `action` from `job`, under the config revision `revision`, at `now`.
    pub(super) fn redeem(
        self: &Arc<Flights>,
        token: &str,
        action: JobAction,
        job: u64,
        revision: u64,
        now: Instant,
    ) -> Redeemed {
        let mut book = lock(&self.book);
        book.forget(now, self.lasts);
        match book.redeem(token, Asked { action, job }, revision) {
            Taken::Stale(why) => Redeemed::Stale(why),
            Taken::Follow { answer, issued } => Redeemed::Board {
                boarding: Boarding::Follow(answer),
                issued,
            },
            Taken::Busy { other, issued } => Redeemed::Board {
                boarding: Boarding::Busy(other),
                issued,
            },
            Taken::Lead {
                key,
                id,
                answer,
                issued,
            } => Redeemed::Board {
                boarding: Boarding::Lead {
                    landing: Landing {
                        flights: self.clone(),
                        key,
                        id,
                    },
                    answer,
                },
                issued,
            },
        }
    }

    /// How many runs have an action in flight.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        lock(&self.book).runs.len()
    }

    /// How many requests wait for a flight's answer.
    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        lock(&self.book)
            .runs
            .values()
            .map(|flight| flight.answer.receiver_count())
            .sum()
    }
}

/// What [`Book::redeem`] did.
enum Taken {
    Stale(Superseded),
    Follow {
        answer: watch::Receiver<Option<Landed>>,
        issued: Issued,
    },
    Busy {
        other: JobAction,
        issued: Issued,
    },
    Lead {
        key: RunKey,
        id: u64,
        answer: watch::Receiver<Option<Landed>>,
        issued: Issued,
    },
}

impl Book {
    fn issue(&mut self, token: String, issued: Issued, now: Instant, lasts: Duration) {
        self.forget(now, lasts);
        while self.issued.len() >= ISSUED_MAX {
            let oldest = self
                .issued
                .iter()
                .min_by_key(|(_, confirmation)| confirmation.at)
                .map(|(token, _)| token.clone());
            match oldest {
                Some(oldest) => self.issued.remove(&oldest),
                None => break,
            };
        }
        self.issued.insert(
            token,
            Confirmation {
                issued,
                at: now,
                used: Use::Open,
            },
        );
    }

    /// Drops the confirmations issued more than `lasts` before `now`.
    fn forget(&mut self, now: Instant, lasts: Duration) {
        self.issued
            .retain(|_, confirmation| now.saturating_duration_since(confirmation.at) < lasts);
    }

    fn redeem(&mut self, token: &str, asked: Asked, revision: u64) -> Taken {
        let Some(confirmation) = self.issued.get(token) else {
            return Taken::Stale(Superseded::Unknown);
        };
        let issued = confirmation.issued.clone();
        let target = &issued.target;
        if target.action != asked.action || target.job != asked.job {
            return Taken::Stale(Superseded::OtherRequest);
        }
        let key = RunKey {
            repository: target.repository.clone(),
            run: target.run,
        };
        let flight = self.runs.get(&key);
        if let Use::Boarded(boarded) = confirmation.used {
            return match flight {
                Some(flight) if flight.id == boarded => Taken::Follow {
                    answer: flight.answer.subscribe(),
                    issued,
                },
                _ => Taken::Stale(Superseded::Used),
            };
        }
        if issued.revision != revision {
            self.issued.remove(token);
            return Taken::Stale(Superseded::ConfigChanged);
        }
        let joined = match flight {
            Some(flight) if flight.asked == asked => (flight.id, flight.answer.subscribe()),
            Some(flight) => {
                // Not taken: it holds once the other action landed.
                return Taken::Busy {
                    other: flight.asked.action,
                    issued,
                };
            }
            None => {
                let id = self.next_flight;
                self.next_flight += 1;
                let (answer, receiver) = watch::channel(None);
                self.runs.insert(key.clone(), Flight { id, asked, answer });
                if let Some(confirmation) = self.issued.get_mut(token) {
                    confirmation.used = Use::Boarded(id);
                }
                return Taken::Lead {
                    key,
                    id,
                    answer: receiver,
                    issued,
                };
            }
        };
        let (id, answer) = joined;
        if let Some(confirmation) = self.issued.get_mut(token) {
            confirmation.used = Use::Boarded(id);
        }
        Taken::Follow { answer, issued }
    }

    /// Gives the requests waiting for flight `id` its answer.
    fn answer(&self, key: &RunKey, id: u64, landed: Landed) {
        if let Some(flight) = self.runs.get(key).filter(|flight| flight.id == id) {
            flight.answer.send_replace(Some(landed));
        }
    }

    /// Takes flight `id` out: a later request sends anew, and its confirmations are used.
    fn remove(&mut self, key: &RunKey, id: u64) {
        if self.runs.get(key).is_some_and(|flight| flight.id == id) {
            self.runs.remove(key);
        }
    }
}

impl Landing {
    /// Gives every request waiting the answer, and takes the run's entry out.
    pub(super) fn land(self, landed: Landed) {
        lock(&self.flights.book).answer(&self.key, self.id, landed);
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        lock(&self.flights.book).remove(&self.key, self.id);
    }
}

/// 128 random bits, in hex: a confirmation no one can guess.
fn token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(32), |mut hex, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

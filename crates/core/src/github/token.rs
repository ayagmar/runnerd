//! The GitHub client's token (ADR 0134, ADR 0181): read from its source one read at a time and kept, a command's
//! until GitHub refuses it, an expiring one until shortly before it expires. A command's source (`gh auth token`) runs
//! on a thread of its own, bounded by `command`'s deadline and killed with its process group at a stop; an expiring
//! one runs as a task on the runtime within [`MINT_DEADLINE`]. A request that stops waiting, at its deadline or a
//! stop, leaves the read running for the next request to join, so a source that hangs never piles reads up nor holds
//! the runtime's shutdown, and a read that fails, panics, times out or is dropped with its runtime ends the read and
//! answers everyone waiting.
//!
//! It must not put the token in a log line, a `Debug` output or an error.

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures_util::FutureExt as _;
use tokio::sync::watch;

use super::{Error, ExpiringSource, ExpiringToken, Token};
use crate::sync::lock;

/// How long before it expires an expiring token is read again: a request sent with it still has its time.
pub(super) const REFRESH_MARGIN: Duration = Duration::from_mins(5);

/// The longest an expiring token is kept, whatever its source says: a GitHub App installation's lasts an hour, so a
/// later expiry is a clock's error, not the token's.
pub(super) const MAX_LIFETIME: Duration = Duration::from_hours(1);

/// How long an expiring token's read may take before it is given up: under a job action's deadline, so the action can
/// still say what happened.
pub(super) const MINT_DEADLINE: Duration = super::TOKEN_DEADLINE;

/// The wait after an expiring token's first failed read before the next is tried; each failure in a row doubles it, up
/// to [`BACKOFF_MOST`]. A request meanwhile fails at once with the last read's error.
pub(super) const BACKOFF_FIRST: Duration = Duration::from_secs(10);

/// The longest wait between two failed reads of an expiring token.
pub(super) const BACKOFF_MOST: Duration = Duration::from_mins(5);

/// Where the token comes from.
pub(super) enum Source {
    /// A command's token, kept until GitHub refuses it: run on a thread of its own. A failed read is tried again by
    /// the next request.
    Command(Arc<dyn Fn() -> Result<Token, Error> + Send + Sync>),
    /// A token that expires: run as a task on the runtime within [`MINT_DEADLINE`]. A failed read waits out a backoff.
    Expiring(ExpiringSource),
}

/// The two clocks a token's expiry is checked against: the monotonic one, which a change of the wall clock leaves
/// alone but which stops while the host sleeps, and the wall clock, which goes on through a sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Now {
    pub(super) monotonic: Instant,
    pub(super) wall: SystemTime,
}

impl Now {
    fn system() -> Now {
        Now {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

/// What the token's expiry is checked by: the system's clocks, or a test's.
pub(super) type Clock = Arc<dyn Fn() -> Now + Send + Sync>;

/// A read's answer: the token and how long it lasts, or why there is none, as text.
type Read = Option<Result<Granted, String>>;

/// A token as a request gets it: with how long it lasts, so the request can check it is still usable right before it
/// sends it.
#[derive(Clone)]
pub(super) struct Granted {
    token: Token,
    lasts: Lasts,
}

impl Granted {
    /// The token.
    pub(super) fn token(&self) -> &Token {
        &self.token
    }
}

/// The token and the read of it. Only its methods here, and its read's [`Producer`], write it; it survives nothing: a
/// new client reads anew, and a reload leaves it as it is.
pub(super) struct Tokens {
    source: Arc<Source>,
    clock: Clock,
    state: Arc<Mutex<Hold>>,
}

enum Hold {
    /// None read yet, or a command's last read failed.
    Missing,
    /// Read, and kept for as long as `lasts` says.
    Held { token: Token, lasts: Lasts },
    /// A read runs; its answer comes on this channel.
    Reading(watch::Receiver<Read>),
    /// An expiring token's last `failures` reads failed; none is tried before `retry`, and a request meanwhile gets
    /// `message`.
    Failed {
        retry: Instant,
        failures: u32,
        message: String,
    },
}

/// How long a held token is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lasts {
    /// Until GitHub refuses it: a command's.
    UntilRefused,
    /// Until either clock passes its side of this, the token's expiry less the margin, or GitHub refuses it.
    Until(Now),
}

impl Lasts {
    /// Whether a token that lasts this may still be sent `now`: both clocks before its end, so a wall clock set back
    /// and a host that slept each end it.
    fn fresh(self, now: Now) -> bool {
        match self {
            Lasts::UntilRefused => true,
            Lasts::Until(end) => now.monotonic < end.monotonic && now.wall < end.wall,
        }
    }

    /// How long an expiring token minted from `started` on lasts, if its source's answer may be used: a token that is
    /// not one an `Authorization` header carries, or that expires within the margin, or already did, is not.
    fn of(minted: &ExpiringToken, started: Now) -> Result<Lasts, String> {
        let value = &minted.token.0;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err("the token minted is not a token".to_owned());
        }
        let lifetime = minted
            .expires_at
            .duration_since(started.wall)
            .unwrap_or_default()
            .min(MAX_LIFETIME);
        let Some(used) = lifetime
            .checked_sub(REFRESH_MARGIN)
            .filter(|used| !used.is_zero())
        else {
            return Err("the token minted expires too soon".to_owned());
        };
        Ok(Lasts::Until(Now {
            monotonic: started.monotonic + used,
            wall: started.wall + used,
        }))
    }
}

/// The wait after the `failures`th failed read in a row.
pub(super) fn backoff_of(failures: u32) -> Duration {
    BACKOFF_FIRST
        .saturating_mul(1_u32 << failures.saturating_sub(1).min(16))
        .min(BACKOFF_MOST)
}

impl Tokens {
    pub(super) fn new(source: Source) -> Tokens {
        Tokens {
            source: Arc::new(source),
            clock: Arc::new(Now::system),
            state: Arc::new(Mutex::new(Hold::Missing)),
        }
    }

    /// Checks the tokens' expiry by `clock` from now on.
    #[cfg(test)]
    pub(super) fn set_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    /// Whether `granted` may still be sent now, by both clocks: a request checks it again after anything it awaited
    /// once the token was in hand.
    pub(super) fn usable(&self, granted: &Granted) -> bool {
        granted.lasts.fresh((self.clock)())
    }

    /// The token held, unless it is the one GitHub `refused` or it is about to expire; otherwise a read's: the one
    /// running, or a new one. Of several requests refused, cold or expiring at once, the first starts the read and the
    /// others join it. A refusal of a token no longer held changes nothing: the one held since is served. A read's
    /// token is checked again as it is handed over: one that expired meanwhile, as the host slept, is an error.
    pub(super) async fn get(&self, refused: Option<&Token>) -> Result<Granted, Error> {
        let (mut answer, started) = {
            let mut state = lock(&self.state);
            let now = (self.clock)();
            match &*state {
                Hold::Held { token, lasts }
                    if refused.is_none_or(|refused| refused.0 != token.0) && lasts.fresh(now) =>
                {
                    return Ok(Granted {
                        token: token.clone(),
                        lasts: *lasts,
                    });
                }
                Hold::Reading(answer) => (answer.clone(), None),
                Hold::Failed { retry, message, .. } if now.monotonic < *retry => {
                    return Err(Error::Token(message.clone()));
                }
                Hold::Failed { failures, .. } => {
                    let failures = *failures;
                    let (answer, producer) = self.begin(&mut state, failures);
                    (answer, Some((producer, now)))
                }
                Hold::Held { .. } | Hold::Missing => {
                    let (answer, producer) = self.begin(&mut state, 0);
                    (answer, Some((producer, now)))
                }
            }
        };
        // Launched with the lock let go: a read that cannot start answers at once, which takes the lock.
        if let Some((producer, now)) = started {
            self.launch(producer, now)?;
        }
        let read = answer
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Token("the token read stopped".to_owned()))?
            .clone();
        match read {
            Some(Ok(granted)) if self.usable(&granted) => Ok(granted),
            Some(Ok(_)) => Err(Error::Token(
                "the token read expired before it could be used".to_owned(),
            )),
            Some(Err(message)) => Err(Error::Token(message)),
            None => Err(Error::Token("the token read gave nothing".to_owned())),
        }
    }

    /// Says a read runs, `failures` reads in a row having failed before it, and makes the producer that will answer it.
    fn begin(&self, state: &mut Hold, failures: u32) -> (watch::Receiver<Read>, Producer) {
        let (send, answer) = watch::channel(None);
        *state = Hold::Reading(answer.clone());
        let producer = Producer {
            send,
            shared: self.state.clone(),
            source: self.source.clone(),
            clock: self.clock.clone(),
            failures,
            settled: false,
        };
        (answer, producer)
    }

    /// Starts `producer`'s read, which keeps or drops its answer whoever still waits for it: a command's on a thread
    /// of its own, an expiring one's as a task on the runtime, its expiry counted from `now`.
    fn launch(&self, producer: Producer, now: Now) -> Result<(), Error> {
        match &*self.source {
            Source::Command(read) => {
                let read = read.clone();
                std::thread::Builder::new()
                    .name("github-token".to_owned())
                    .spawn(move || {
                        // A source that panics is a failed read like any other.
                        let token = std::panic::catch_unwind(AssertUnwindSafe(|| read()))
                            .unwrap_or_else(|_| {
                                Err(Error::Token("the token read failed".to_owned()))
                            })
                            .map(|token| (token, Lasts::UntilRefused))
                            .map_err(message);
                        producer.settle(token);
                    })
                    .map_err(|error| {
                        Error::Token(format!("cannot start the token read: {error}"))
                    })?;
            }
            Source::Expiring(mint) => {
                let runtime = tokio::runtime::Handle::try_current()
                    .map_err(|_| Error::Token("no runtime to read the token on".to_owned()))?;
                let mint = mint.clone();
                runtime.spawn(async move {
                    // Dropped with the runtime, the producer still answers: see its `Drop`.
                    let minted = tokio::time::timeout(
                        MINT_DEADLINE,
                        AssertUnwindSafe(async move { mint().await }).catch_unwind(),
                    )
                    .await;
                    let token = match minted {
                        Ok(Ok(Ok(minted))) => {
                            Lasts::of(&minted, now).map(|lasts| (minted.token, lasts))
                        }
                        Ok(Ok(Err(error))) => Err(message(error)),
                        Ok(Err(_)) => Err("the token read failed".to_owned()),
                        Err(_) => Err(format!(
                            "the token read took longer than {}s",
                            MINT_DEADLINE.as_secs()
                        )),
                    };
                    producer.settle(token);
                });
            }
        }
        Ok(())
    }
}

/// An error as the text a read's answer carries.
fn message(error: Error) -> String {
    match error {
        Error::Token(message) => message,
        other => other.to_string(),
    }
}

/// The one read running: it ends `Reading` with its answer and gives the answer to everyone waiting. Dropped without
/// an answer, as its task is with a runtime that stops, it ends the read as failed, so no later request waits on it.
struct Producer {
    send: watch::Sender<Read>,
    shared: Arc<Mutex<Hold>>,
    source: Arc<Source>,
    clock: Clock,
    /// Reads in a row that failed before this one.
    failures: u32,
    settled: bool,
}

impl Producer {
    fn settle(mut self, read: Result<(Token, Lasts), String>) {
        self.answer(read);
    }

    fn answer(&mut self, read: Result<(Token, Lasts), String>) {
        self.settled = true;
        // A read that ends past its own token's end, as the host slept while it ran, failed like any other.
        let read = read.and_then(|(token, lasts)| {
            if lasts.fresh((self.clock)()) {
                Ok(Granted { token, lasts })
            } else {
                Err("the token read expired before it could be used".to_owned())
            }
        });
        // The read started with this lock held until it said a read runs, so the answer lands after that.
        let mut state = lock(&self.shared);
        *state = match &read {
            Ok(granted) => Hold::Held {
                token: granted.token.clone(),
                lasts: granted.lasts,
            },
            Err(message) => match *self.source {
                Source::Command(_) => Hold::Missing,
                Source::Expiring(_) => {
                    let failures = self.failures.saturating_add(1);
                    Hold::Failed {
                        retry: (self.clock)().monotonic + backoff_of(failures),
                        failures,
                        message: message.clone(),
                    }
                }
            },
        };
        drop(state);
        self.send.send_replace(Some(read));
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        if !self.settled {
            self.answer(Err("the token read stopped".to_owned()));
        }
    }
}

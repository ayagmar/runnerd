//! The GitHub client's token (ADR 0134): read from its source on a thread of its own, one read at a time, and kept
//! until GitHub refuses it. A request that stops waiting, at its deadline or a stop, leaves the read running for the
//! next request to join, so a source that hangs never piles reads up nor holds the runtime's shutdown; `gh_token`
//! itself is bounded by `command`'s deadline and killed with its process group at a stop.
//!
//! It must not put the token in a log line, a `Debug` output or an error.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::{Error, Token};
use crate::sync::lock;

/// Where the token comes from: the client's [`TokenSource`](super::TokenSource), shared with the thread that runs it.
pub(super) type Source = Arc<dyn Fn() -> Result<Token, Error> + Send + Sync>;

/// A read's answer: the token, or why there is none, as text.
type Read = Option<Result<Token, String>>;

/// The token and the read of it. Only its methods here write it; it survives nothing: a new client reads anew.
pub(super) struct Tokens {
    source: Source,
    state: Arc<Mutex<Hold>>,
}

enum Hold {
    /// None read yet, or the last read failed.
    Missing,
    /// Read, and kept until GitHub refuses it.
    Held(Token),
    /// A read runs; its answer comes on this channel.
    Reading(watch::Receiver<Read>),
}

impl Tokens {
    pub(super) fn new(source: Source) -> Tokens {
        Tokens {
            source,
            state: Arc::new(Mutex::new(Hold::Missing)),
        }
    }

    /// The token held, unless it is the one GitHub `refused`; otherwise a read's: the one running, or a new one. Of
    /// several requests refused at once, the first starts the read and the others join it.
    pub(super) async fn get(&self, refused: Option<&Token>) -> Result<Token, Error> {
        let mut answer = {
            let mut state = lock(&self.state);
            match &*state {
                Hold::Held(token) if refused.is_none_or(|refused| refused.0 != token.0) => {
                    return Ok(token.clone());
                }
                Hold::Reading(answer) => answer.clone(),
                Hold::Held(_) | Hold::Missing => self.start(&mut state)?,
            }
        };
        let read = answer
            .wait_for(Option::is_some)
            .await
            .map_err(|_| Error::Token("the token read stopped".to_owned()))?
            .clone();
        match read {
            Some(Ok(token)) => Ok(token),
            Some(Err(message)) => Err(Error::Token(message)),
            None => Err(Error::Token("the token read gave nothing".to_owned())),
        }
    }

    /// Starts a read on a thread of its own, which keeps or drops its answer whoever still waits for it.
    fn start(&self, state: &mut Hold) -> Result<watch::Receiver<Read>, Error> {
        let (send, answer) = watch::channel(None);
        let source = self.source.clone();
        let shared = self.state.clone();
        std::thread::Builder::new()
            .name("github-token".to_owned())
            .spawn(move || {
                // A source that panics is a failed read like any other: without an answer the state would say a
                // read runs for ever, and no later request could start one.
                let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source()))
                    .unwrap_or_else(|_| Err(Error::Token("the token read failed".to_owned())));
                // The caller holds this lock until it has said a read runs, so the answer lands after that.
                *lock(&shared) = match &read {
                    Ok(token) => Hold::Held(token.clone()),
                    Err(_) => Hold::Missing,
                };
                send.send_replace(Some(read.map_err(|error| match error {
                    Error::Token(message) => message,
                    other => other.to_string(),
                })));
            })
            .map_err(|error| Error::Token(format!("cannot start the token read: {error}")))?;
        *state = Hold::Reading(answer.clone());
        Ok(answer)
    }
}

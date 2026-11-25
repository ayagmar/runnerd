//! The action archives' anonymous reads of a pinned action's repository (ADR 0181): `GET /repos/{owner}/{repo}` sent
//! without any credential, following no redirect, with answers kept for conditional requests, a backoff and an hourly
//! cap of their own. GitHub gives anonymous requests 60 an hour per address, shared with whatever else on the host or
//! its network asks; the cap keeps runnerd to half of that, every send counted, a 304 and a failed one included.
//!
//! It must never send the client's token, nor touch what the authenticated reads keep: their rate limit, counts,
//! backoff and conditional answers, which the scheduler paces by. A limit used up or the cap spent is an error a
//! caller can tell from a repository that is not there.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;

use super::{Cached, Conditional, Error, REQUEST_TIMEOUT, decode, rate_limited_until};
use crate::sync::lock;

/// The most anonymous requests sent in any hour, whatever GitHub answered: half of GitHub's 60, the rest left for
/// whatever else shares the address.
pub(super) const HOURLY_CAP: usize = 30;

/// The window the cap counts sends in.
const CAP_WINDOW: Duration = Duration::from_hours(1);

/// The most bytes of answers kept for conditional requests: a repository's details are a few KB.
const CONDITIONAL_BYTES: usize = 1024 * 1024;

/// How long an answer asked for is not dropped to make room for another.
const CONDITIONAL_FRESH: Duration = Duration::from_hours(1);

/// A client for GitHub's API that never carries a credential.
pub(super) struct Anonymous {
    http: reqwest::Client,
    base: String,
    ledger: Mutex<Ledger>,
}

/// What the anonymous reads keep between requests. Nothing survives a restart, and a reload does not touch it.
struct Ledger {
    /// When each send of the last hour went, oldest first.
    sends: VecDeque<Instant>,
    /// When GitHub said anonymous requests may resume, if it said they may not.
    blocked_until: Option<SystemTime>,
    etags: Conditional,
}

impl Ledger {
    /// Takes one send of the cap `now`, if the limit and the cap allow one.
    fn take(&mut self, now: Instant, wall: SystemTime) -> Result<(), Error> {
        if let Some(until) = self.blocked_until.filter(|until| *until > wall) {
            return Err(Error::RateLimited { until });
        }
        while self
            .sends
            .front()
            .is_some_and(|sent| now.saturating_duration_since(*sent) >= CAP_WINDOW)
        {
            self.sends.pop_front();
        }
        if self.sends.len() >= HOURLY_CAP {
            return Err(Error::Held(
                "this hour's anonymous requests to GitHub are spent",
            ));
        }
        self.sends.push_back(now);
        Ok(())
    }
}

impl Anonymous {
    pub(super) fn new(base: &str) -> Result<Anonymous, Error> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        let http = reqwest::Client::builder()
            .user_agent(concat!("runnerd/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|source| Error::Http {
                method: Method::GET,
                path: base.to_owned(),
                source,
            })?;
        Ok(Anonymous {
            http,
            base: base.trim_end_matches('/').to_owned(),
            ledger: Mutex::new(Ledger {
                sends: VecDeque::new(),
                blocked_until: None,
                etags: Conditional::new(CONDITIONAL_BYTES, CONDITIONAL_FRESH),
            }),
        })
    }

    /// The answer at `path`, read without a credential and conditional on the last one kept for it. A redirect is an
    /// error, as is any answer but a success or a 304: a 404 or 410 says the repository is not there to anyone, a
    /// rate limit is `RateLimited`, the cap spent is `Held`.
    pub(super) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let cached = {
            let mut ledger = lock(&self.ledger);
            ledger.take(Instant::now(), SystemTime::now())?;
            ledger.etags.get(path, Instant::now())
        };
        let mut request = self.http.get(format!("{}{path}", self.base));
        if let Some(cached) = &cached {
            request = request.header(header::IF_NONE_MATCH, cached.etag.clone());
        }
        let _ = super::REQUESTS.try_with(|sent| sent.set(sent.get() + 1));
        let response = request.send().await.map_err(|source| Error::Http {
            method: Method::GET,
            path: path.to_owned(),
            source: source.without_url(),
        })?;
        let status = response.status();
        if let Some(until) = rate_limited_until(status, response.headers(), SystemTime::now()) {
            lock(&self.ledger).blocked_until = Some(until);
            if matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            ) {
                return Err(Error::RateLimited { until });
            }
        }
        if status == StatusCode::NOT_MODIFIED
            && let Some(cached) = cached
        {
            return decode(path, &cached.body);
        }
        let etag = response.headers().get(header::ETAG).cloned();
        if !status.is_success() {
            lock(&self.ledger).etags.remove(path);
            // Not GitHub's words: the repository is another's, and the answer may be anything.
            return Err(Error::Status {
                method: Method::GET,
                path: path.to_owned(),
                status,
                message: "the anonymous read was refused".to_owned(),
            });
        }
        let bytes = response.bytes().await.map_err(|source| Error::Http {
            method: Method::GET,
            path: path.to_owned(),
            source: source.without_url(),
        })?;
        let body: T = decode(path, &bytes)?;
        let mut ledger = lock(&self.ledger);
        match etag {
            Some(etag) => ledger.etags.insert(
                path,
                Cached {
                    etag,
                    body: bytes.to_vec().into(),
                    next: false,
                },
                Instant::now(),
            ),
            None => ledger.etags.remove(path),
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime};

    use super::{CAP_WINDOW, Conditional, HOURLY_CAP, Ledger};
    use crate::github::Error;

    fn ledger() -> Ledger {
        Ledger {
            sends: std::collections::VecDeque::new(),
            blocked_until: None,
            etags: Conditional::new(1024, Duration::from_secs(1)),
        }
    }

    #[test]
    fn the_cap_counts_every_send_of_the_last_hour_and_frees_one_as_it_ages_out() {
        let mut ledger = ledger();
        let start = Instant::now();
        let wall = SystemTime::now();
        for nth in 0..HOURLY_CAP {
            let at = start + Duration::from_secs(u64::try_from(nth).unwrap());
            assert!(
                ledger.take(at, wall).is_ok(),
                "send {nth} is within the cap"
            );
        }
        let full = start + Duration::from_secs(60);
        assert!(matches!(ledger.take(full, wall), Err(Error::Held(_))));
        assert!(
            ledger.take(start + CAP_WINDOW, wall).is_ok(),
            "the first send aged out"
        );
        assert!(matches!(
            ledger.take(start + CAP_WINDOW, wall),
            Err(Error::Held(_))
        ));
    }

    #[test]
    fn a_limit_github_set_holds_sends_until_its_reset_without_taking_the_cap() {
        let mut ledger = ledger();
        let wall = SystemTime::now();
        ledger.blocked_until = Some(wall + Duration::from_secs(60));
        assert!(matches!(
            ledger.take(Instant::now(), wall),
            Err(Error::RateLimited { .. })
        ));
        assert!(ledger.sends.is_empty(), "nothing was sent");
        assert!(
            ledger
                .take(Instant::now(), wall + Duration::from_secs(61))
                .is_ok()
        );
    }
}

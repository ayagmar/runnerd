//! Domain logic and API types shared by the runnerd daemon and its clients.
//!
//! `api` and `config` build everywhere. The `host` feature (on by default) adds what only the daemon runs: Docker, the
//! GitHub client, the caches, the scheduler, sizing advice, insights, the job history store and what observes running jobs.
//! Clients depend on this crate without it, and with the `client` feature for `client`, their connection to the daemon.

#![warn(missing_docs)]

#[cfg(feature = "host")]
pub mod admission;
pub mod api;
#[cfg(feature = "host")]
pub mod cache;
#[cfg(feature = "client")]
pub mod client;
pub mod config;
#[cfg(feature = "host")]
pub mod docker;
#[cfg(feature = "host")]
pub mod github;
#[cfg(feature = "host")]
pub mod insights;
#[cfg(feature = "host")]
pub mod isolation;
#[cfg(feature = "host")]
pub mod observe;
#[cfg(feature = "host")]
pub mod record;
#[cfg(feature = "host")]
pub mod scheduler;
#[cfg(feature = "host")]
pub mod sizing;
#[cfg(feature = "host")]
pub mod store;
#[cfg(feature = "host")]
mod sync;
#[cfg(all(test, any(feature = "host", feature = "client")))]
mod testdir;
#[cfg(feature = "host")]
mod time;

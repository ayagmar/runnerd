//! Domain logic and API types shared by the runnerd daemon and its clients.
//!
//! `api` and `config` build everywhere. The `host` feature (on by default) adds what only the daemon runs: Docker,
//! the GitHub client, the caches and the scheduler. Clients depend on this crate without it.

#![warn(missing_docs)]

pub mod api;
#[cfg(feature = "host")]
pub mod cache;
pub mod config;
#[cfg(feature = "host")]
pub mod docker;
#[cfg(feature = "host")]
pub mod github;
#[cfg(feature = "host")]
pub mod isolation;
#[cfg(feature = "host")]
pub mod scheduler;

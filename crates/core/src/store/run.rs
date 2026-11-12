//! The jobs of one workflow run that the history has, which a job action's confirmation names beside its job
//! (ADR 0134). Reads only, a bounded number of jobs; it decides nothing about what GitHub's run holds.

use rusqlite::{Connection, params};

use super::read;
use crate::api;

/// Other jobs of a run the history has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunJobs {
    /// Newest attempt first, then by ID.
    pub jobs: Vec<api::JobSummary>,
    /// Whether the history has more than `jobs`.
    pub more: bool,
}

/// Up to `limit` jobs of `repository`'s run `run` but `except`, newest attempt first, and whether there are more.
pub(super) fn run_jobs(
    db: &Connection,
    repository: &str,
    run: u64,
    except: u64,
    limit: usize,
) -> rusqlite::Result<RunJobs> {
    let mut statement = db.prepare_cached(
        "SELECT id FROM jobs WHERE repository = ?1 AND run_id = ?2 AND id != ?3 \
         ORDER BY run_attempt DESC, id LIMIT ?4",
    )?;
    let ids = statement
        .query_map(
            params![
                repository,
                run,
                except,
                i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1)
            ],
            |row| row.get::<_, u64>(0),
        )?
        .collect::<rusqlite::Result<Vec<u64>>>()?;
    let more = ids.len() > limit;
    let mut jobs = Vec::new();
    for id in ids.into_iter().take(limit) {
        jobs.extend(read::summary(db, id)?);
    }
    Ok(RunJobs { jobs, more })
}

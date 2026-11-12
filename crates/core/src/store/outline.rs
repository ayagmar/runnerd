//! A job's log outline in the history (ADR 0133). The writer feeds each line it stores to the job's
//! [`Outliner`], resumed from `log_outlines` and the job's open groups, and keeps what it returns in the transaction
//! that stores the lines, so a restart resumes where the stored lines end. Reads assemble [`LogOutline`] from it in
//! one read transaction, so a batch stored or pruned meanwhile is in all of it or none; pruning deletes it after
//! the lines. Only the writer and pruning write these tables, and nothing here lives between
//! two batches of lines.

use rusqlite::{Connection, OptionalExtension, params};

use super::read;
use crate::api::{
    self, LogGroup, LogMark, LogOutline, MarkKind, OutlineChange, OutlineParts, Outliner,
    SavedOutline, StoredLog,
};

/// How `log_marks.kind` stores a mark.
fn kind_name(kind: MarkKind) -> &'static str {
    match kind {
        MarkKind::Error => "error",
        MarkKind::Warning | MarkKind::Unknown => "warning",
    }
}

/// `log_marks.kind` of a line that starts a step without a group.
const SECTION: &str = "section";

/// One job's outline while the writer stores a batch of its lines.
pub(super) struct Outlining<'db> {
    db: &'db Connection,
    job: u64,
    outliner: Outliner,
}

impl<'db> Outlining<'db> {
    /// The outline of `job`, whose log holds `count` lines before this batch: the one kept, or a new one from
    /// `count`, which is past 0 only for a log an older runnerd began.
    pub(super) fn load(
        db: &'db Connection,
        job: u64,
        count: u64,
    ) -> rusqlite::Result<Outlining<'db>> {
        let saved = db
            .prepare_cached(
                "SELECT from_line, errors, warnings, groups_kept, marks_kept, sections_kept, dropped, hidden \
                 FROM log_outlines WHERE job = ?1",
            )?
            .query_row([job], |row| {
                Ok(SavedOutline {
                    from: row.get(0)?,
                    errors: row.get(1)?,
                    warnings: row.get(2)?,
                    groups: row.get(3)?,
                    marks: row.get(4)?,
                    sections: row.get(5)?,
                    dropped: row.get(6)?,
                    hidden: row.get(7)?,
                    open: Vec::new(),
                })
            })
            .optional()?;
        let outliner = match saved {
            Some(mut saved) => {
                saved.open = db
                    .prepare_cached(
                        "SELECT start FROM log_groups WHERE job = ?1 AND end_line IS NULL ORDER BY start",
                    )?
                    .query_map([job], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                Outliner::resume(saved)
            }
            None => Outliner::new(count),
        };
        Ok(Outlining { db, job, outliner })
    }

    /// Reads stored line `line`, with text `text`, and keeps what it changes.
    pub(super) fn line(&mut self, line: u64, text: &str) -> rusqlite::Result<()> {
        let (db, job) = (self.db, self.job);
        match self.outliner.line(line, text) {
            Some(OutlineChange::Opened {
                start,
                depth,
                title,
            }) => {
                db.prepare_cached(
                    "INSERT OR REPLACE INTO log_groups (job, start, end_line, depth, title) \
                     VALUES (?1, ?2, NULL, ?3, ?4)",
                )?
                .execute(params![job, start, depth, title])?;
            }
            Some(OutlineChange::Closed { start, end }) => {
                db.prepare_cached(
                    "UPDATE log_groups SET end_line = ?3 WHERE job = ?1 AND start = ?2",
                )?
                .execute(params![job, start, end])?;
            }
            Some(OutlineChange::Marked(mark)) => {
                mark_line(db, job, mark.line, kind_name(mark.kind))?;
            }
            Some(OutlineChange::Section { line }) => mark_line(db, job, line, SECTION)?,
            None => {}
        }
        Ok(())
    }

    /// Keeps where the outline got to.
    pub(super) fn save(self) -> rusqlite::Result<()> {
        let saved = self.outliner.saved();
        self.db
            .prepare_cached(
                "INSERT OR REPLACE INTO log_outlines (job, from_line, errors, warnings, groups_kept, marks_kept, \
                 sections_kept, dropped, hidden) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                self.job,
                saved.from,
                saved.errors,
                saved.warnings,
                saved.groups,
                saved.marks,
                saved.sections,
                saved.dropped,
                saved.hidden,
            ])?;
        Ok(())
    }
}

fn mark_line(db: &Connection, job: u64, line: u64, kind: &str) -> rusqlite::Result<()> {
    db.prepare_cached("INSERT OR REPLACE INTO log_marks (job, line, kind) VALUES (?1, ?2, ?3)")?
        .execute(params![job, line, kind])?;
    Ok(())
}

/// Whether line `line` of `job`'s log is stored.
fn stored(db: &Connection, job: u64, line: u64) -> rusqlite::Result<bool> {
    db.prepare_cached("SELECT 1 FROM log_lines WHERE job = ?1 AND line = ?2")?
        .exists(params![job, line])
}

/// A job's outline, read at one time; `None` without the job.
pub(super) fn outline(db: &Connection, id: u64) -> rusqlite::Result<Option<LogOutline>> {
    // Deferred: a snapshot of the database as of the first read, kept to the last. Dropped, it rolls back: it wrote
    // nothing.
    let snapshot = db.unchecked_transaction()?;
    let db: &Connection = &snapshot;
    let Some(job) = read::job(db, id)? else {
        return Ok(None);
    };
    let log = &job.summary.log;
    let stored_log = StoredLog {
        lines: log.lines,
        truncated: log.truncated,
        kept: log.lines > 0 && stored(db, id, 0)? && stored(db, id, log.lines - 1)?,
    };
    let saved = db
        .prepare_cached(
            "SELECT from_line, errors, warnings, groups_kept, marks_kept, sections_kept, dropped, hidden \
             FROM log_outlines WHERE job = ?1",
        )?
        .query_row([id], |row| {
            Ok(SavedOutline {
                from: row.get(0)?,
                errors: row.get(1)?,
                warnings: row.get(2)?,
                groups: row.get(3)?,
                marks: row.get(4)?,
                sections: row.get(5)?,
                dropped: row.get(6)?,
                hidden: row.get(7)?,
                open: Vec::new(),
            })
        })
        .optional()?;
    let groups = db
        .prepare_cached(
            "SELECT start, end_line, depth, title FROM log_groups WHERE job = ?1 ORDER BY start",
        )?
        .query_map([id], |row| {
            Ok(LogGroup {
                start: row.get(0)?,
                end: row.get(1)?,
                depth: row.get(2)?,
                title: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut marks = Vec::new();
    let mut sections = Vec::new();
    let mut rows =
        db.prepare_cached("SELECT line, kind FROM log_marks WHERE job = ?1 ORDER BY line")?;
    let mut rows = rows.query([id])?;
    while let Some(row) = rows.next()? {
        let line: u64 = row.get(0)?;
        match row.get::<_, String>(1)?.as_str() {
            SECTION => sections.push(line),
            "error" => marks.push(LogMark {
                line,
                kind: MarkKind::Error,
            }),
            _ => marks.push(LogMark {
                line,
                kind: MarkKind::Warning,
            }),
        }
    }
    let parts = OutlineParts {
        saved,
        groups,
        marks,
        sections,
    };
    let mut failed = None;
    let outline = LogOutline::assemble(&job, stored_log, parts, &mut |want| {
        read::lines_from(db, id, want).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            Vec::new()
        })
    });
    match failed {
        Some(error) => Err(error),
        None => Ok(Some(outline)),
    }
}

/// Deletes a job's outline.
pub(super) fn delete(db: &Connection, job: u64) -> rusqlite::Result<()> {
    for sql in [
        "DELETE FROM log_groups WHERE job = ?1",
        "DELETE FROM log_marks WHERE job = ?1",
        "DELETE FROM log_outlines WHERE job = ?1",
    ] {
        db.prepare_cached(sql)?.execute([job])?;
    }
    Ok(())
}

/// Text one read of styled lines takes at most, in bytes, past its last line.
const STYLED_BYTES: u64 = 4 << 20;

/// Up to `limit` styled lines of a job's log from line `from`, fewer once they hold 4 MiB of text.
pub(super) fn styled_lines(
    db: &Connection,
    job: u64,
    from: u64,
    limit: u64,
) -> rusqlite::Result<Vec<api::StyledLine>> {
    let read = api::LineRead {
        from,
        limit,
        prefix: api::LINE_BYTES_MAX,
        bytes: STYLED_BYTES,
    };
    Ok(read::lines_from(db, job, read)?
        .iter()
        .map(api::StoredLine::styled)
        .collect())
}

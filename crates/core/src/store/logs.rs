//! A job's stored log lines (ADR 0171): rows of `log_lines` while its record may still grow, then, once it is final,
//! packs of `log_packs`, each a run of its lines deflated together. A job with packs is read from them alone and takes
//! no more lines, so a reader never mixes the two; every read of a job's lines goes through here, in one snapshot of
//! the database. Pruning packs a final job in three steps, none of which holds the write lock for long: it reads and
//! deflates the lines outside a write, stores the packs in a short transaction if no line came meanwhile, then
//! deletes the rows a few MiB at a time. A crash between them leaves rows the next prune deletes. Every step is
//! bounded in bytes: a log past `PACK_JOB_MAX`, one with a line longer than a pack holds, and one with a row that is
//! not UTF-8 text stay in rows, read as before. A pack is checked as it is inflated: past `PACK_MAX` bytes or
//! `PACK_LINES` lines, or with numbers that do not rise from its row's first line to its last, it is an error, never
//! lines. It decides nothing about what is kept: pruning still deletes a job's log whole once `[history] logs` is past.

use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::time::Instant;

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::prune::Db;
use crate::api;

/// The first byte of a pack's inflated data: the layout after it. A layout this build does not know reads as an
/// error, never as lines. Layout 1: each line, in order, as two LEB128 varints, how many numbers it skips past the
/// line before (the pack's first counts from 0) and its length, then its bytes. Consecutive lines skip 0, which
/// deflates to almost nothing.
const LAYOUT: u8 = 1;

/// Lines one pack holds at most.
const PACK_LINES: usize = 1024;

/// Text after which a pack takes no more lines, in bytes: a read of a page of lines inflates a few packs of about
/// this size, not the whole log.
const PACK_BYTES: usize = 256 << 10;

/// What one pack inflates to at most, in bytes, its layout byte and each line's varints counted: a read never
/// inflates more per pack, whatever its row holds. A line too long for it leaves its job in rows.
const PACK_MAX: usize = 1 << 20;

/// The most text of one job packed, in bytes: what one prune reads in one snapshot and stores in one transaction. A
/// longer log stays in rows.
const PACK_JOB_MAX: u64 = 16 << 20;

/// Rows of `log_lines` one delete takes at most.
const DELETE_ROWS: u64 = 2000;

/// Text one delete of `log_lines` takes at most, in bytes, past its first row.
const DELETE_BYTES: u64 = 4 << 20;

/// The highest line number SQLite stores, as a signed 64-bit integer.
const LINE_MAX: u64 = i64::MAX as u64;

/// Whether `job`'s log is packed.
pub(super) fn packed(db: &Connection, job: u64) -> rusqlite::Result<bool> {
    db.prepare_cached("SELECT 1 FROM log_packs WHERE job = ?1")?
        .exists([job])
}

/// `read` in one snapshot of the database, so a pack stored and its rows deleted meanwhile are seen both or neither;
/// as it is inside a transaction already, such as the writer's.
fn consistent<T>(
    db: &Connection,
    read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    if db.is_autocommit() {
        super::read::snapshot(db, read)
    } else {
        read(db)
    }
}

/// Calls `each` with each line of `job`'s packs from line `from` on, in order, until it returns `false`. A pack that
/// does not read as the lines its row names is an error.
fn each_packed(
    db: &Connection,
    job: u64,
    from: u64,
    mut each: impl FnMut(u64, &[u8]) -> rusqlite::Result<bool>,
) -> rusqlite::Result<()> {
    let mut statement = db.prepare_cached(
        "SELECT first_line, last_line, data FROM log_packs WHERE job = ?1 AND last_line >= ?2 \
         ORDER BY last_line",
    )?;
    let mut rows = statement.query(params![job, from])?;
    while let Some(row) = rows.next()? {
        let data = match row.get_ref(2)? {
            ValueRef::Blob(data) => data,
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Text(_) => {
                return Err(unreadable("a pack that is no blob"));
            }
        };
        let pack = unpack(data, row.get(0)?, row.get(1)?)?;
        for (number, text) in pack.lines() {
            if number >= from && !each(number, text)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A packed line's text, which was UTF-8 text when it was packed: anything else is a damaged pack.
fn text_of(bytes: &[u8]) -> rusqlite::Result<String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|error| rusqlite::Error::Utf8Error(0, error))
}

/// Up to `limit` lines of `job`'s log from line `from`, with their numbers.
pub(super) fn lines(
    db: &Connection,
    job: u64,
    from: u64,
    limit: u64,
) -> rusqlite::Result<Vec<(u64, String)>> {
    consistent(db, |db| {
        if !packed(db, job)? {
            return db
                .prepare_cached(
                    "SELECT line, text FROM log_lines WHERE job = ?1 AND line >= ?2 ORDER BY line LIMIT ?3",
                )?
                .query_map(params![job, from, limit], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect();
        }
        let mut lines = Vec::new();
        if limit > 0 {
            each_packed(db, job, from, |line, text| {
                lines.push((line, text_of(text)?));
                Ok((lines.len() as u64) < limit)
            })?;
        }
        Ok(lines)
    })
}

/// The stored lines of a job's log `read` asks for, their text read as UTF-8 with anything else replaced: what the
/// capture stores is text, but a history is a file anyone may have written. From rows, SQLite hands over only the
/// prefix of each line asked for; from packs, each line is cut as it is inflated. Either way the lines stop at the
/// byte budget, so no read holds more than it asked for beyond the packs it inflates, `PACK_MAX` each.
pub(super) fn lines_from(
    db: &Connection,
    job: u64,
    read: api::LineRead,
) -> rusqlite::Result<Vec<api::StoredLine>> {
    consistent(db, |db| {
        let mut lines = Vec::new();
        let mut bytes = 0;
        if packed(db, job)? {
            let prefix = usize::try_from(read.prefix).unwrap_or(usize::MAX);
            if read.limit > 0 && read.bytes > 0 {
                each_packed(db, job, read.from, |line, text| {
                    let stored = api::StoredLine::cut(
                        line,
                        &text[..text.len().min(prefix)],
                        text.len() as u64,
                    );
                    bytes += stored.text.len() as u64;
                    lines.push(stored);
                    Ok(bytes < read.bytes && (lines.len() as u64) < read.limit)
                })?;
            }
            return Ok(lines);
        }
        let mut statement = db.prepare_cached(
            "SELECT line, substr(CAST(text AS BLOB), 1, ?3), octet_length(text) FROM log_lines \
             WHERE job = ?1 AND line >= ?2 ORDER BY line LIMIT ?4",
        )?;
        let mut rows = statement.query(params![job, read.from, read.prefix, read.limit])?;
        while bytes < read.bytes
            && let Some(row) = rows.next()?
        {
            let text = match row.get_ref(1)? {
                ValueRef::Text(text) | ValueRef::Blob(text) => text,
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => &[],
            };
            let line = api::StoredLine::cut(row.get(0)?, text, row.get(2)?);
            bytes += line.text.len() as u64;
            lines.push(line);
        }
        Ok(lines)
    })
}

/// Every stored line of a job's log, in order.
pub(super) fn text(db: &Connection, job: u64) -> rusqlite::Result<Vec<String>> {
    consistent(db, |db| {
        if !packed(db, job)? {
            return db
                .prepare_cached("SELECT text FROM log_lines WHERE job = ?1 ORDER BY line")?
                .query_map([job], |row| row.get(0))?
                .collect();
        }
        let mut lines = Vec::new();
        each_packed(db, job, 0, |_, text| {
            lines.push(text_of(text)?);
            Ok(true)
        })?;
        Ok(lines)
    })
}

/// Whether line `line` of `job`'s log is stored.
pub(super) fn stored(db: &Connection, job: u64, line: u64) -> rusqlite::Result<bool> {
    consistent(db, |db| {
        if !packed(db, job)? {
            return db
                .prepare_cached("SELECT 1 FROM log_lines WHERE job = ?1 AND line = ?2")?
                .exists(params![job, line]);
        }
        let mut found = false;
        each_packed(db, job, line, |number, _| {
            found = number == line;
            Ok(false)
        })?;
        Ok(found)
    })
}

/// Deletes `job`'s packs; its rows are pruning's to delete.
pub(super) fn delete_packs(db: &Connection, job: u64) -> rusqlite::Result<()> {
    db.prepare_cached("DELETE FROM log_packs WHERE job = ?1")?
        .execute([job])?;
    Ok(())
}

/// Deletes `job`'s first rows: up to `DELETE_ROWS`, fewer once they hold `DELETE_BYTES` of text, one at least. How
/// many it deleted.
pub(super) fn delete_some_rows(db: &Connection, job: u64) -> rusqlite::Result<usize> {
    let mut statement = db.prepare_cached(
        "SELECT line, octet_length(text) FROM log_lines WHERE job = ?1 ORDER BY line LIMIT ?2",
    )?;
    let mut rows = statement.query(params![job, DELETE_ROWS])?;
    let mut last = None;
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        last = Some(row.get::<_, u64>(0)?);
        bytes = bytes.saturating_add(row.get::<_, Option<u64>>(1)?.unwrap_or(0));
        if bytes >= DELETE_BYTES {
            break;
        }
    }
    drop(rows);
    drop(statement);
    let Some(last) = last else {
        return Ok(0);
    };
    db.prepare_cached("DELETE FROM log_lines WHERE job = ?1 AND line <= ?2")?
        .execute(params![job, last])
}

/// Packs the logs of the jobs whose records were final before `before` (ms) and that still have rows, oldest first,
/// one job at a time with a pause after each as long as it took, until the prune is stopped. A log whose record
/// counts more than `PACK_JOB_MAX` bytes is not read.
pub(super) fn pack_final(db: &Db<'_>, before: u64) -> rusqlite::Result<()> {
    let jobs: Vec<u64> = db
        .prepare(
            "SELECT id FROM jobs WHERE state IN ('completed', 'lost', 'elsewhere') \
             AND coalesce(finished_at, queued_at) < ?1 AND log_bytes <= ?2 \
             AND EXISTS (SELECT 1 FROM log_lines WHERE job = id) ORDER BY id",
        )?
        .query_map(params![before, PACK_JOB_MAX], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for job in jobs {
        if db.stopped() {
            return Ok(());
        }
        let started = Instant::now();
        let made = make(db, job, &|| db.stopped())?;
        if db.stopped() {
            return Ok(());
        }
        if let Made::Unpackable(why) = &made {
            tracing::debug!(job, why, "a log stays in rows");
        }
        store(db, job, &made)?;
        std::thread::sleep(started.elapsed());
        delete_rows(db, job)?;
    }
    Ok(())
}

/// What `make` made of a job's rows.
pub(super) enum Made {
    /// Its packs, and its count of lines when they were read.
    Packs {
        count: Option<u64>,
        packs: Vec<Pack>,
    },
    /// It has packs already.
    Packed,
    /// It stays in rows: too long, or with a row that is not UTF-8 text.
    Unpackable(&'static str),
    /// The prune was stopped while they were made.
    Stopped,
}

/// Reads and deflates `job`'s rows in a snapshot, outside a write, so the writer goes on meanwhile; asks `stopped`
/// after each pack's worth of lines.
pub(super) fn make(
    db: &Connection,
    job: u64,
    stopped: &dyn Fn() -> bool,
) -> rusqlite::Result<Made> {
    super::read::snapshot(db, |db| {
        if packed(db, job)? {
            return Ok(Made::Packed);
        }
        let count = db
            .query_row("SELECT log_lines FROM jobs WHERE id = ?1", [job], |row| {
                row.get(0)
            })
            .optional()?;
        let mut statement =
            db.prepare_cached("SELECT line, text FROM log_lines WHERE job = ?1 ORDER BY line")?;
        let mut rows = statement.query([job])?;
        let mut packs = Packer::default();
        let mut total = 0u64;
        while let Some(row) = rows.next()? {
            // A row a strict read would not take as text keeps its job in rows, read as strictly as before.
            let ValueRef::Text(text) = row.get_ref(1)? else {
                return Ok(Made::Unpackable("a row that is not text"));
            };
            if std::str::from_utf8(text).is_err() {
                return Ok(Made::Unpackable("a row that is not UTF-8"));
            }
            total = total.saturating_add(text.len() as u64);
            if total > PACK_JOB_MAX {
                return Ok(Made::Unpackable("a log too long to pack"));
            }
            if packs.line(row.get(0)?, text)? == Added::TooLong {
                return Ok(Made::Unpackable("a line too long for a pack"));
            }
            if packs.lines % PACK_LINES == 0 && stopped() {
                return Ok(Made::Stopped);
            }
        }
        Ok(Made::Packs {
            count,
            packs: packs.finish()?,
        })
    })
}

/// Stores `made`'s packs as `job`'s in one short transaction, unless the job has packs or its count of lines moved
/// since they were read: whether it stored them. The rows stay for `delete_rows`. Anything but packs stores nothing.
pub(super) fn store(db: &Connection, job: u64, made: &Made) -> rusqlite::Result<bool> {
    let Made::Packs { count, packs } = made else {
        return Ok(false);
    };
    if packs.is_empty() {
        return Ok(false);
    }
    let transaction = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    let now: Option<u64> = transaction
        .query_row("SELECT log_lines FROM jobs WHERE id = ?1", [job], |row| {
            row.get(0)
        })
        .optional()?;
    if now != *count || packed(&transaction, job)? {
        // Lines came, or the job went: the next prune looks again.
        return Ok(false);
    }
    let mut insert = transaction.prepare_cached(
        "INSERT INTO log_packs (job, first_line, last_line, data) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for pack in packs {
        insert.execute(params![job, pack.first, pack.last, pack.data])?;
    }
    drop(insert);
    transaction.commit()?;
    Ok(true)
}

/// Deletes `job`'s rows a few MiB at a time once it is packed, pausing after each delete as long as it took.
fn delete_rows(db: &Db<'_>, job: u64) -> rusqlite::Result<()> {
    if !packed(db, job)? {
        return Ok(());
    }
    loop {
        if db.stopped() {
            return Ok(());
        }
        let started = Instant::now();
        if delete_some_rows(db, job)? == 0 {
            return Ok(());
        }
        std::thread::sleep(started.elapsed());
    }
}

/// One pack as stored: the lines it holds, from `first` to `last`, and its deflated data.
pub(super) struct Pack {
    first: u64,
    last: u64,
    data: Vec<u8>,
}

/// Lines in a pack being made, the packs made, and how many lines in all.
#[derive(Default)]
struct Packer {
    open: Option<Open>,
    packs: Vec<Pack>,
    lines: usize,
}

/// The pack being made: its first and last line, how many lines and bytes of text it holds, what it inflates to,
/// and its encoder.
struct Open {
    first: u64,
    last: u64,
    lines: usize,
    text: usize,
    inflated: usize,
    encoder: DeflateEncoder<Vec<u8>>,
}

/// What `Packer::line` did with a line.
#[derive(Debug, PartialEq, Eq)]
enum Added {
    Line,
    /// It would not fit a pack alone, or its number is past SQLite's: nothing was added.
    TooLong,
}

/// A line's two varints: how many numbers it skips and its length.
fn head(skipped: u64, length: usize) -> Vec<u8> {
    let mut head = Vec::with_capacity(20);
    varint(&mut head, skipped);
    varint(&mut head, length as u64);
    head
}

impl Packer {
    /// Adds line `number`, whose text is `text`, after the lines added before, which numbered less; closes the pack
    /// before it when that is full or would inflate past `PACK_MAX`.
    fn line(&mut self, number: u64, text: &[u8]) -> rusqlite::Result<Added> {
        // As the first line of a pack, its skip is its number.
        let alone = head(number, text.len());
        if number > LINE_MAX || 1 + alone.len() + text.len() > PACK_MAX {
            return Ok(Added::TooLong);
        }
        let mut next = alone;
        if let Some(open) = &self.open {
            let after = head(number.saturating_sub(open.last + 1), text.len());
            if open.lines >= PACK_LINES
                || open.text >= PACK_BYTES
                || open.inflated + after.len() + text.len() > PACK_MAX
            {
                self.close()?;
            } else {
                next = after;
            }
        }
        if self.open.is_none() {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&[LAYOUT]).map_err(failed)?;
            self.open = Some(Open {
                first: number,
                last: number,
                lines: 0,
                text: 0,
                inflated: 1,
                encoder,
            });
        }
        let Some(open) = &mut self.open else {
            return Ok(Added::TooLong);
        };
        open.encoder.write_all(&next).map_err(failed)?;
        open.encoder.write_all(text).map_err(failed)?;
        open.last = number;
        open.lines += 1;
        open.text += text.len();
        open.inflated += next.len() + text.len();
        self.lines += 1;
        Ok(Added::Line)
    }

    fn close(&mut self) -> rusqlite::Result<()> {
        if let Some(open) = self.open.take() {
            self.packs.push(Pack {
                first: open.first,
                last: open.last,
                data: open.encoder.finish().map_err(failed)?,
            });
        }
        Ok(())
    }

    fn finish(mut self) -> rusqlite::Result<Vec<Pack>> {
        self.close()?;
        Ok(self.packs)
    }
}

/// A pack's inflated data, and each of its lines: its number and where its text lies in the data.
struct Unpacked {
    data: Vec<u8>,
    lines: Vec<(u64, Range<usize>)>,
}

impl Unpacked {
    fn lines(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.lines
            .iter()
            .filter_map(|(number, at)| Some((*number, self.data.get(at.clone())?)))
    }
}

/// The lines of a pack's deflated `data`, whose row says it holds lines `first` to `last`. An error, never lines,
/// for data that inflates past `PACK_MAX`, holds more than `PACK_LINES` lines, is of another layout or cut short, or
/// whose numbers do not rise from `first` to `last` within SQLite's range.
fn unpack(data: &[u8], first: u64, last: u64) -> rusqlite::Result<Unpacked> {
    let mut inflated = Vec::new();
    DeflateDecoder::new(data)
        .take(PACK_MAX as u64 + 1)
        .read_to_end(&mut inflated)
        .map_err(|error| unreadable(&format!("a pack that does not inflate: {error}")))?;
    if inflated.len() > PACK_MAX {
        return Err(unreadable("a pack that inflates past what a pack holds"));
    }
    if inflated.first() != Some(&LAYOUT) {
        return Err(unreadable("a pack of a layout this runnerd does not know"));
    }
    let mut lines = Vec::new();
    let mut at = 1;
    let mut next = 0u64;
    while at < inflated.len() {
        if lines.len() >= PACK_LINES {
            return Err(unreadable("a pack of more lines than a pack holds"));
        }
        let mut rest = &inflated[at..];
        let before = rest.len();
        let number = read_varint(&mut rest).and_then(|skipped| next.checked_add(skipped));
        let length = read_varint(&mut rest).and_then(|length| usize::try_from(length).ok());
        let (Some(number), Some(length)) = (number, length) else {
            return Err(unreadable("a pack cut short"));
        };
        if number > LINE_MAX {
            return Err(unreadable("a pack with a line number past SQLite's"));
        }
        let start = at + (before - rest.len());
        let Some(end) = start
            .checked_add(length)
            .filter(|end| *end <= inflated.len())
        else {
            return Err(unreadable("a pack cut short"));
        };
        lines.push((number, start..end));
        next = number + 1;
        at = end;
    }
    let ends = (
        lines.first().map(|line| line.0),
        lines.last().map(|line| line.0),
    );
    if ends != (Some(first), Some(last)) {
        return Err(unreadable("a pack whose lines are not those its row names"));
    }
    Ok(Unpacked {
        data: inflated,
        lines,
    })
}

/// Appends `value` as a LEB128 varint.
fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Reads a LEB128 varint off the front of `bytes`; `None` when they end first or it does not fit 64 bits: the tenth
/// byte may carry only the 64th bit.
fn read_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// A pack that cannot be read, as SQLite's error for a value of the wrong kind.
fn unreadable(what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, Type::Blob, what.into())
}

/// An encoder that failed, which writing to memory does not.
fn failed(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::{Added, LAYOUT, PACK_MAX, Packer, read_varint, unpack, varint};

    /// `raw` deflated, as a pack's data.
    fn deflated(raw: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    /// Layout 1's bytes for `lines`, each its skip, its declared length and its bytes.
    fn raw(lines: &[(u64, u64, &[u8])]) -> Vec<u8> {
        let mut raw = vec![LAYOUT];
        for (skip, length, text) in lines {
            varint(&mut raw, *skip);
            varint(&mut raw, *length);
            raw.extend_from_slice(text);
        }
        raw
    }

    #[test]
    fn a_pack_gives_back_each_line_byte_for_byte_with_its_number() {
        let lines: Vec<(u64, Vec<u8>)> = vec![
            (0, b"".to_vec()),
            (1, "\u{e9}t\u{e9} \u{1f600}".as_bytes().to_vec()),
            (2, vec![0xff, 0xfe, b'\n', 0]),
            (7, vec![b'x'; 300_000]),
            (8, b"after a gap".to_vec()),
        ];
        let mut packer = Packer::default();
        for (number, text) in &lines {
            assert_eq!(packer.line(*number, text).unwrap(), Added::Line);
        }
        let packs = packer.finish().unwrap();
        // The 300 kB line fills a pack: the line after it starts the next.
        assert_eq!(
            packs
                .iter()
                .map(|pack| (pack.first, pack.last))
                .collect::<Vec<_>>(),
            [(0, 7), (8, 8)]
        );
        let mut read = Vec::new();
        for pack in &packs {
            let unpacked = unpack(&pack.data, pack.first, pack.last).unwrap();
            read.extend(
                unpacked
                    .lines()
                    .map(|(number, text)| (number, text.to_vec())),
            );
        }
        assert_eq!(read, lines);
    }

    #[test]
    fn a_line_no_pack_holds_is_refused_and_one_that_would_pass_a_pack_starts_the_next() {
        // Catches a pack made bigger than a read inflates, whose log would then never read again.
        let mut packer = Packer::default();
        assert_eq!(
            packer.line(0, &vec![b'a'; PACK_MAX]).unwrap(),
            Added::TooLong
        );
        assert_eq!(packer.line(0, &vec![b'a'; 200 << 10]).unwrap(), Added::Line);
        assert_eq!(packer.line(1, &vec![b'b'; 900 << 10]).unwrap(), Added::Line);
        let packs = packer.finish().unwrap();
        assert_eq!(
            packs.len(),
            2,
            "the second line does not fit beside the first"
        );
        for pack in &packs {
            assert!(
                unpack(&pack.data, pack.first, pack.last).is_ok(),
                "each pack reads"
            );
        }
    }

    #[test]
    fn a_damaged_pack_is_an_error_not_lines() {
        // Catches a pack read past its bounds: data that inflates without end, numbers past SQLite's that a page's
        // `line + 1` overflows, numbers other than the row's, and data cut short or of another layout.
        let good = deflated(&raw(&[(5, 2, b"hi"), (0, 1, b"x")]));
        assert_eq!(unpack(&good, 5, 6).unwrap().lines().count(), 2);
        assert!(
            unpack(&good, 4, 6).is_err(),
            "a first line other than the row's"
        );
        assert!(
            unpack(&good, 5, 7).is_err(),
            "a last line other than the row's"
        );
        assert!(
            unpack(&good[..good.len() / 2], 5, 6).is_err(),
            "half a pack"
        );
        assert!(
            unpack(&deflated(&[2, 0, 1, b'x']), 0, 0).is_err(),
            "a later layout"
        );
        let mut bomb = vec![LAYOUT];
        bomb.resize(64 << 20, 0);
        assert!(unpack(&deflated(&bomb), 0, 0).is_err(), "64 MiB of zeros");
        let past = deflated(&raw(&[(u64::MAX, 1, b"x")]));
        assert!(
            unpack(&past, u64::MAX, u64::MAX).is_err(),
            "a number past i64"
        );
        let long = deflated(&raw(&[(0, 10, b"short")]));
        assert!(unpack(&long, 0, 0).is_err(), "a length past the data");
        let many: Vec<(u64, u64, &[u8])> = vec![(0, 0, b""); 1025];
        assert!(
            unpack(&deflated(&raw(&many)), 0, 1024).is_err(),
            "more lines than a pack holds"
        );
        assert!(
            unpack(&deflated(&[LAYOUT]), 0, 0).is_err(),
            "no line at all"
        );
    }

    #[test]
    fn varints_round_trip_at_their_edges_and_refuse_bits_past_64() {
        for value in [0, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut bytes = Vec::new();
            varint(&mut bytes, value);
            let mut read = bytes.as_slice();
            assert_eq!(read_varint(&mut read), Some(value));
            assert!(read.is_empty(), "{value} reads to its end");
        }
        assert_eq!(read_varint(&mut [0x80u8; 11].as_slice()), None);
        // Nine bytes of seven bits, then a tenth that carries more than the 64th.
        let mut over = vec![0xffu8; 9];
        over.push(0x02);
        assert_eq!(read_varint(&mut over.as_slice()), None);
        over[9] = 0x01;
        assert_eq!(read_varint(&mut over.as_slice()), Some(u64::MAX));
    }
}

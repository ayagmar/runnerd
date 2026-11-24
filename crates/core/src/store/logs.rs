//! A job's stored log lines (ADR 0171): rows of `log_lines` while its record may still grow, then, once it is final,
//! packs of `log_packs`, each a run of its lines deflated together. A job with packs is read from them alone and takes
//! no more lines, so a reader never mixes the two; every read of a job's lines goes through here, in one snapshot of
//! the database. Pruning packs a final job in three steps, none of which holds the write lock for long: it reads and
//! deflates the lines outside a write, stores the packs in a short transaction if no line came meanwhile, then
//! deletes the rows a batch at a time. A crash between them leaves rows the next prune deletes. It decides nothing
//! about what is kept: pruning still deletes a job's log whole once `[history] logs` is past.

use std::io::{Read as _, Write as _};
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

/// Text one pack holds at most before its last line, in bytes: a read of a page of lines inflates a few packs of this
/// size, not the whole log.
const PACK_BYTES: usize = 256 << 10;

/// Rows of `log_lines` one delete takes at most once their job is packed.
const DELETE_BATCH: u64 = 2000;

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

/// Calls `each` with each line of `job`'s packs from line `from` on, in order, until it returns `false`.
fn each_packed(
    db: &Connection,
    job: u64,
    from: u64,
    mut each: impl FnMut(u64, &[u8]) -> bool,
) -> rusqlite::Result<()> {
    let mut statement = db.prepare_cached(
        "SELECT data FROM log_packs WHERE job = ?1 AND last_line >= ?2 ORDER BY last_line",
    )?;
    let mut rows = statement.query(params![job, from])?;
    while let Some(row) = rows.next()? {
        let data = match row.get_ref(0)? {
            ValueRef::Blob(data) => data,
            ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) | ValueRef::Text(_) => {
                return Err(unreadable(Type::Text, "a pack that is no blob"));
            }
        };
        for line in unpack(data)? {
            if line.number >= from && !each(line.number, &line.text) {
                return Ok(());
            }
        }
    }
    Ok(())
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
                lines.push((line, String::from_utf8_lossy(text).into_owned()));
                (lines.len() as u64) < limit
            })?;
        }
        Ok(lines)
    })
}

/// The stored lines of a job's log `read` asks for, their text read as UTF-8 with anything else replaced: what the
/// capture stores is text, but a history is a file anyone may have written. From rows, SQLite hands over only the
/// prefix of each line asked for; from packs, each line is cut as it is inflated. Either way the lines stop at the
/// byte budget, so no read holds more than it asked for beyond the packs it inflates.
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
                    bytes < read.bytes && (lines.len() as u64) < read.limit
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
            lines.push(String::from_utf8_lossy(text).into_owned());
            true
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
            false
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

/// Packs the logs of the jobs whose records were final before `before` (ms) and that still have rows, oldest first,
/// one job at a time with a pause after each as long as it took, until the prune is stopped.
pub(super) fn pack_final(db: &Db<'_>, before: u64) -> rusqlite::Result<()> {
    let jobs: Vec<u64> = db
        .prepare(
            "SELECT id FROM jobs WHERE state IN ('completed', 'lost', 'elsewhere') \
             AND coalesce(finished_at, queued_at) < ?1 AND EXISTS (SELECT 1 FROM log_lines WHERE job = id) \
             ORDER BY id",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for job in jobs {
        if db.stopped() {
            return Ok(());
        }
        let started = Instant::now();
        pack(db, job)?;
        std::thread::sleep(started.elapsed());
        delete_rows(db, job)?;
    }
    Ok(())
}

/// Stores `job`'s lines as packs, unless it has packs already or a line came while they were made.
fn pack(db: &Connection, job: u64) -> rusqlite::Result<()> {
    if let Some(made) = make(db, job)? {
        store(db, job, &made)?;
    }
    Ok(())
}

/// `job`'s lines packed, and its count of lines they were read at; `None` when it has packs already.
pub(super) struct Made {
    count: Option<u64>,
    packs: Vec<Pack>,
}

/// Reads and deflates `job`'s rows in a snapshot, outside a write, so the writer goes on meanwhile; `None` when it is
/// packed already.
pub(super) fn make(db: &Connection, job: u64) -> rusqlite::Result<Option<Made>> {
    super::read::snapshot(db, |db| {
        if packed(db, job)? {
            return Ok(None);
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
        while let Some(row) = rows.next()? {
            let text = match row.get_ref(1)? {
                ValueRef::Text(text) | ValueRef::Blob(text) => text,
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => &[],
            };
            packs.line(row.get(0)?, text)?;
        }
        Ok(Some(Made {
            count,
            packs: packs.finish()?,
        }))
    })
}

/// Stores `made` as `job`'s packs in one short transaction, unless the job has packs or its count of lines moved
/// since they were read: whether it stored them. The rows stay for `delete_rows`.
pub(super) fn store(db: &Connection, job: u64, made: &Made) -> rusqlite::Result<bool> {
    let transaction = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    let count: Option<u64> = transaction
        .query_row("SELECT log_lines FROM jobs WHERE id = ?1", [job], |row| {
            row.get(0)
        })
        .optional()?;
    if count != made.count || packed(&transaction, job)? {
        // Lines came, or the job went: the next prune looks again.
        return Ok(false);
    }
    let mut insert = transaction.prepare_cached(
        "INSERT INTO log_packs (job, first_line, last_line, data) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for pack in &made.packs {
        insert.execute(params![job, pack.first, pack.last, pack.data])?;
    }
    drop(insert);
    transaction.commit()?;
    Ok(true)
}

/// Deletes `job`'s rows a batch at a time once it is packed.
fn delete_rows(db: &Db<'_>, job: u64) -> rusqlite::Result<()> {
    if !packed(db, job)? {
        return Ok(());
    }
    let mut delete = db.prepare(
        "DELETE FROM log_lines WHERE job = ?1 AND line IN \
         (SELECT line FROM log_lines WHERE job = ?1 LIMIT ?2)",
    )?;
    loop {
        if db.stopped() {
            return Ok(());
        }
        let started = Instant::now();
        if delete.execute(params![job, DELETE_BATCH])? == 0 {
            return Ok(());
        }
        std::thread::sleep(started.elapsed());
    }
}

/// One pack as stored: the lines it holds, from `first` to `last`, and its deflated data.
struct Pack {
    first: u64,
    last: u64,
    data: Vec<u8>,
}

/// Lines in a pack being made, and the packs made.
#[derive(Default)]
struct Packer {
    open: Option<Open>,
    packs: Vec<Pack>,
}

/// The pack being made: its first and last line, how many lines and bytes of text it holds, and its encoder.
struct Open {
    first: u64,
    last: u64,
    lines: usize,
    bytes: usize,
    encoder: DeflateEncoder<Vec<u8>>,
}

impl Packer {
    /// Adds line `number`, whose text is `text`, closing the pack before it when that is full.
    fn line(&mut self, number: u64, text: &[u8]) -> rusqlite::Result<()> {
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.lines >= PACK_LINES || open.bytes >= PACK_BYTES)
        {
            self.close()?;
        }
        if self.open.is_none() {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&[LAYOUT]).map_err(failed)?;
            self.open = Some(Open {
                first: number,
                last: number,
                lines: 0,
                bytes: 0,
                encoder,
            });
        }
        let Some(open) = &mut self.open else {
            return Ok(());
        };
        let mut head = Vec::with_capacity(20);
        // The first line of a pack skips from 0, so a pack reads without the one before.
        let next = if open.lines == 0 { 0 } else { open.last + 1 };
        varint(&mut head, number.saturating_sub(next));
        varint(&mut head, text.len() as u64);
        open.encoder.write_all(&head).map_err(failed)?;
        open.encoder.write_all(text).map_err(failed)?;
        open.last = number;
        open.lines += 1;
        open.bytes += text.len();
        Ok(())
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

/// One line read from a pack.
struct Line {
    number: u64,
    text: Vec<u8>,
}

/// The lines of a pack's deflated `data`, in order.
fn unpack(data: &[u8]) -> rusqlite::Result<Vec<Line>> {
    let mut inflated = Vec::new();
    DeflateDecoder::new(data)
        .read_to_end(&mut inflated)
        .map_err(|error| {
            unreadable(
                Type::Blob,
                &format!("a pack that does not inflate: {error}"),
            )
        })?;
    let Some((&LAYOUT, mut rest)) = inflated.split_first() else {
        return Err(unreadable(
            Type::Blob,
            "a pack of a layout this runnerd does not know",
        ));
    };
    let mut lines = Vec::new();
    let mut next = 0u64;
    while !rest.is_empty() {
        let (Some(number), Some(length)) = (
            read_varint(&mut rest).and_then(|skipped| next.checked_add(skipped)),
            read_varint(&mut rest),
        ) else {
            return Err(unreadable(Type::Blob, "a pack cut short"));
        };
        next = number.saturating_add(1);
        let Some((text, after)) = usize::try_from(length)
            .ok()
            .and_then(|length| rest.split_at_checked(length))
        else {
            return Err(unreadable(Type::Blob, "a pack cut short"));
        };
        lines.push(Line {
            number,
            text: text.to_vec(),
        });
        rest = after;
    }
    Ok(lines)
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

/// Reads a LEB128 varint off the front of `bytes`; `None` when they end first or it does not fit 64 bits.
fn read_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// A pack that cannot be read, as SQLite's error for a value of the wrong kind.
fn unreadable(kind: Type, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, kind, what.into())
}

/// An encoder that failed, which writing to memory does not.
fn failed(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::{Packer, read_varint, unpack, varint};

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
            packer.line(*number, text).unwrap();
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
        let read: Vec<(u64, Vec<u8>)> = packs
            .iter()
            .flat_map(|pack| unpack(&pack.data).unwrap())
            .map(|line| (line.number, line.text))
            .collect();
        assert_eq!(read, lines);
    }

    #[test]
    fn a_pack_cut_short_or_of_another_layout_is_an_error_not_lines() {
        let mut packer = Packer::default();
        packer.line(0, b"hello").unwrap();
        let data = packer.finish().unwrap().remove(0).data;
        assert!(
            unpack(&data[..data.len() / 2]).is_err(),
            "half a pack does not read"
        );
        let mut other = Vec::new();
        let mut encoder =
            flate2::write::DeflateEncoder::new(&mut other, flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &[2, 0, 1, b'x']).unwrap();
        encoder.finish().unwrap();
        assert!(
            unpack(&other).is_err(),
            "a later layout does not read as this one"
        );
    }

    #[test]
    fn varints_round_trip_at_their_edges() {
        for value in [0, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut bytes = Vec::new();
            varint(&mut bytes, value);
            let mut read = bytes.as_slice();
            assert_eq!(read_varint(&mut read), Some(value));
            assert!(read.is_empty(), "{value} reads to its end");
        }
        assert_eq!(read_varint(&mut [0x80u8; 11].as_slice()), None);
    }
}

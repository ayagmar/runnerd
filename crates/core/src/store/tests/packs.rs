//! Schema 33: a final job's log packed by pruning (ADR 0171), read as it was stored through every read of a log,
//! beside a running job's rows, across a crash part way through and a restart, and pruned whole.

use rusqlite::Connection;

use super::DAY_MS;
use super::log_outline::{HOUR, POSITION, at, finish, log, running};
use crate::api::{
    LINE_BYTES_MAX, LineRead, LogOutline, LogSearch, LogSearchQuery, Search, StoredLine, StyledLine,
};
use crate::config::History;
use crate::store::{Store, logs, read};
use crate::testdir::TestDir;

/// When job 1's record is final, in ms: `finish` ends it 12 s past `HOUR`.
const FINAL: u64 = (HOUR + 12) * 1000;

/// A prune at which job 1's log is packed, 11 minutes after its record is final, long before `[history] logs`.
const PACK: u64 = FINAL + 11 * 60 * 1000;

/// `log()`'s groups, warning and failure, then lines that fill several packs: numbered lines with a character
/// outside ASCII, a line longer than a styled line shows, and one past a pack's bytes, of two-byte characters.
fn long_log() -> Vec<String> {
    let mut lines = log();
    lines.extend((0..2500).map(|n| at(10, &format!("line {n} \u{e9}t\u{e9}"))));
    lines.push("x".repeat(usize::try_from(LINE_BYTES_MAX).unwrap() * 2));
    lines.push("\u{e9}".repeat(150_000));
    lines.push(at(11, "##[error]after the long lines"));
    lines
}

/// What every read of job `id`'s log gives.
#[derive(Debug, PartialEq)]
struct Reads {
    lines: Vec<Vec<(u64, String)>>,
    styled: Vec<Vec<StyledLine>>,
    cut: Vec<Vec<StoredLine>>,
    outline: Option<LogOutline>,
    searches: Vec<LogSearch>,
    summary: (u64, u64, bool),
}

async fn reads(store: &Store, path: &std::path::Path, id: u64) -> Reads {
    let mut lines = Vec::new();
    for (from, limit) in [
        (0, 10_000),
        (0, 0),
        (17, 100),
        (1000, 1030),
        (2515, 10),
        (9000, 5),
    ] {
        lines.push(store.log_lines(id, from, limit).await.unwrap());
    }
    let mut styled = Vec::new();
    for (from, limit) in [(0, 10_000), (1020, 30), (2516, 3)] {
        styled.push(store.styled_lines(id, from, limit).await.unwrap());
    }
    // Cuts mid-character, at a byte budget, and at a prefix below a line's length.
    let db = Connection::open(path).unwrap();
    let cut = [
        (2515, 5, 11, 1 << 20),
        (0, 100, 30, 200),
        (1, 3, 0, 100),
        (2516, 2, 1, 0),
    ]
    .into_iter()
    .map(|(from, limit, prefix, bytes)| {
        let read = LineRead {
            from,
            limit,
            prefix,
            bytes,
        };
        read::lines_from(&db, id, read).unwrap()
    })
    .collect();
    let mut searches = Vec::new();
    for (q, from) in [
        ("line 24", None),
        ("line 24", Some(1500)),
        ("FAILED", None),
        ("absent", None),
    ] {
        let search = Search::new(&LogSearchQuery {
            q: q.to_owned(),
            from,
            ..LogSearchQuery::default()
        })
        .unwrap();
        searches.push(store.search_log(id, search).await.unwrap());
    }
    let log = store.job(id).await.unwrap().unwrap().summary.log;
    Reads {
        lines,
        styled,
        cut,
        outline: store.log_outline(id).await.unwrap(),
        searches,
        summary: (log.lines, log.bytes, log.truncated),
    }
}

/// Job `id`'s rows in `log_lines` and in `log_packs`.
fn rows(path: &std::path::Path, id: u64) -> (u64, u64) {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM log_lines WHERE job = ?1), (SELECT count(*) FROM log_packs WHERE job = ?1)",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn a_final_jobs_packed_log_reads_as_its_rows_did_beside_a_running_jobs_rows() {
    // Catches a pack that drops, reorders or cuts a line differently than the rows did, a read that mixes packs and
    // rows, a prune that packs a running job or leaves the packed job's rows, and a line stored past its pack.
    let dir = TestDir::new("packs");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    store.log(1, long_log(), POSITION, u64::MAX).await;
    finish(&store, 1, "failure");
    running(&store, 2);
    store.log(2, log(), POSITION, u64::MAX).await;
    store.flush().await;
    let (before, running_before) = (reads(&store, &path, 1).await, reads(&store, &path, 2).await);
    assert_eq!(
        before.lines[0].len(),
        long_log().len(),
        "the whole log reads"
    );
    assert!(
        before.styled[2].iter().any(|line| line.omitted > 0),
        "a long line is cut where it is read"
    );

    store
        .prune(FINAL + 9 * 60 * 1000, &History::default())
        .await
        .unwrap();
    assert_eq!(
        rows(&path, 1).1,
        0,
        "a log is not packed straight after it is final"
    );

    store.prune(PACK, &History::default()).await.unwrap();
    let (raw, packs) = rows(&path, 1);
    assert_eq!(raw, 0, "the packed job's rows are deleted");
    assert!(packs >= 3, "{packs} packs: a long log takes several");
    assert_eq!(
        rows(&path, 2),
        (log().len() as u64, 0),
        "a running job keeps its rows"
    );
    assert_eq!(reads(&store, &path, 1).await, before);
    assert_eq!(reads(&store, &path, 2).await, running_before);
    let (stored, packed): (u64, u64) = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT (SELECT log_bytes FROM jobs WHERE id = 1), (SELECT sum(length(data)) FROM log_packs)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(packed * 10 < stored, "{packed} bytes packed of {stored}");

    // A line after the pack, as a capture that outlived its job's record would send, is not kept.
    store.log(1, vec![at(12, "late")], POSITION, u64::MAX).await;
    store.flush().await;
    assert_eq!(reads(&store, &path, 1).await, before);

    // A crash after the packs were stored, before their rows were all deleted: the packs are read, and the next prune
    // deletes what is left.
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "INSERT INTO log_lines (job, line, text) VALUES (1, 0, 'left'), (1, 1, 'over'), (1, 2600, 'beyond');",
    )
    .unwrap();
    assert_eq!(reads(&store, &path, 1).await, before);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(reads(&store, &path, 1).await, before, "after a restart");
    store.prune(PACK, &History::default()).await.unwrap();
    assert_eq!(rows(&path, 1).0, 0, "the rows a crash left are deleted");
    assert_eq!(reads(&store, &path, 1).await, before);

    // Past `[history] logs`, the packs go whole with the outline.
    store
        .prune(FINAL + 31 * DAY_MS, &History::default())
        .await
        .unwrap();
    assert_eq!(rows(&path, 1), (0, 0));
    let gone = reads(&store, &path, 1).await;
    assert!(
        gone.lines.iter().all(Vec::is_empty)
            && gone.searches.iter().all(|search| search.matches.is_empty()),
        "no line of a pruned log reads"
    );
}

#[tokio::test]
async fn a_log_that_grows_while_its_packs_are_made_keeps_its_rows_until_the_next_prune() {
    // Catches packs stored over lines they did not read: the writer stores a line between the read and the write,
    // and the packed job would then read without it.
    let dir = TestDir::new("packs");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    store.log(1, log(), POSITION, u64::MAX).await;
    finish(&store, 1, "success");
    store.flush().await;
    let pruner = Connection::open(&path).unwrap();
    let made = logs::make(&pruner, 1, &|| false).unwrap();
    assert!(
        matches!(made, logs::Made::Packs { .. }),
        "a final log packs"
    );
    store.log(1, vec![at(12, "late")], POSITION, u64::MAX).await;
    store.flush().await;
    assert!(
        !logs::store(&pruner, 1, &made).unwrap(),
        "packs read before the late line are not stored"
    );
    assert_eq!(rows(&path, 1), (log().len() as u64 + 1, 0));
    store.prune(PACK, &History::default()).await.unwrap();
    assert_eq!(rows(&path, 1), (0, 1), "the next prune packs every line");
    let lines = store.log_lines(1, 0, 100).await.unwrap();
    assert_eq!(lines.len(), log().len() + 1);
    assert_eq!(
        lines.last().map(|(_, text)| text.as_str()),
        Some(at(12, "late").as_str())
    );
    // Made again once the job is packed, there is nothing to make.
    assert!(
        matches!(
            logs::make(&pruner, 1, &|| false).unwrap(),
            logs::Made::Packed
        ),
        "a packed job is not packed twice"
    );
}

/// Job 1 with `log()`, final and packed by a prune; its store and path.
async fn packed_job(dir: &TestDir) -> (Store, std::path::PathBuf) {
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    store.log(1, log(), POSITION, u64::MAX).await;
    finish(&store, 1, "failure");
    store.flush().await;
    store.prune(PACK, &History::default()).await.unwrap();
    assert_eq!(rows(&path, 1), (0, 1), "the log is packed");
    (store, path)
}

/// Layout 1's bytes for `lines`, each its skip past the line before, its declared length and its bytes, deflated.
fn pack_data(lines: &[(u64, u64, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(u8::try_from(value & 0x7f).unwrap() | 0x80);
            value >>= 7;
        }
        out.push(u8::try_from(value).unwrap());
    }
    let mut raw = vec![1];
    for (skip, length, text) in lines {
        varint(&mut raw, *skip);
        varint(&mut raw, *length);
        raw.extend_from_slice(text);
    }
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn a_damaged_pack_is_an_error_through_every_read_of_its_log_never_lines() {
    // Catches a pack read past its bounds through the API: data that inflates to 64 MiB, which a one-line read or the
    // outline's check would hold whole, a line number past SQLite's that a page's `line + 1` overflows, and lines
    // other than those its row names, which a page asked from a line would skip or repeat.
    let dir = TestDir::new("packs");
    let (store, path) = packed_job(&dir).await;
    let mut bomb = vec![1, 0];
    bomb.extend(std::iter::repeat_n(0, 64 << 20));
    let bomb = {
        use std::io::Write as _;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bomb).unwrap();
        encoder.finish().unwrap()
    };
    let past = u64::try_from(i64::MAX).unwrap() + 1;
    let damaged: [(&str, u64, u64, Vec<u8>); 4] = [
        ("inflates to 64 MiB", 0, 0, bomb),
        ("a line past i64", past, past, pack_data(&[(past, 1, b"x")])),
        (
            "a first line past its row's",
            0,
            1,
            pack_data(&[(1, 1, b"a"), (0, 1, b"b")]),
        ),
        (
            "other lines than its row's",
            0,
            30,
            pack_data(&[(0, 1, b"a"), (0, 1, b"b")]),
        ),
    ];
    let db = Connection::open(&path).unwrap();
    for (what, first, last, data) in damaged {
        db.execute(
            "UPDATE log_packs SET first_line = ?1, last_line = ?2, data = ?3 WHERE job = 1",
            rusqlite::params![first.min(i64::MAX as u64), last.min(i64::MAX as u64), data],
        )
        .unwrap();
        assert!(store.log_lines(1, 0, 1).await.is_err(), "{what}: lines");
        assert!(
            store.styled_lines(1, 0, 1).await.is_err(),
            "{what}: styled lines"
        );
        let search = Search::new(&LogSearchQuery {
            q: "a".to_owned(),
            ..LogSearchQuery::default()
        })
        .unwrap();
        assert!(store.search_log(1, search).await.is_err(), "{what}: search");
        assert!(store.log_outline(1).await.is_err(), "{what}: outline");
        let read = LineRead {
            from: 0,
            limit: 1,
            prefix: 10,
            bytes: 10,
        };
        assert!(
            read::lines_from(&db, 1, read).is_err(),
            "{what}: a cut read"
        );
        assert!(read::log_text(&db, 1).is_err(), "{what}: the plain text");
    }
}

#[tokio::test]
async fn a_log_with_a_row_that_is_not_utf8_text_stays_in_rows_and_reads_as_before() {
    // Catches a pack of a damaged legacy row: the plain text read of its rows fails, and of its pack would hand over
    // the bytes replaced, silently.
    let dir = TestDir::new("packs");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    for id in 1..=3 {
        running(&store, id);
        store.log(id, log(), POSITION, u64::MAX).await;
        finish(&store, id, "success");
    }
    store.flush().await;
    let db = Connection::open(&path).unwrap();
    db.execute_batch(
        "UPDATE log_lines SET text = CAST(x'c3a9ff' AS TEXT) WHERE job = 1 AND line = 2; \
         UPDATE log_lines SET text = x'c3a9' WHERE job = 2 AND line = 2; \
         UPDATE log_lines SET text = '\u{e9}t\u{e9} \u{1f600}' WHERE job = 3 AND line = 2;",
    )
    .unwrap();
    let before: Vec<_> = (1..=3)
        .map(|id| read::log_text(&db, id).map_err(|error| error.to_string()))
        .collect();
    assert!(
        before[0].is_err() && before[1].is_err(),
        "a strict read refuses the damaged rows: {before:?}"
    );
    store.prune(PACK, &History::default()).await.unwrap();
    assert_eq!(
        rows(&path, 1),
        (log().len() as u64, 0),
        "invalid UTF-8 stays"
    );
    assert_eq!(rows(&path, 2), (log().len() as u64, 0), "a blob stays");
    assert_eq!(rows(&path, 3), (0, 1), "valid Unicode packs");
    let after: Vec<_> = (1..=3)
        .map(|id| read::log_text(&db, id).map_err(|error| error.to_string()))
        .collect();
    assert_eq!(after, before);
}

#[tokio::test]
async fn a_log_too_long_for_one_prune_stays_in_rows_and_a_stopped_prune_stores_nothing() {
    // Catches unbounded work under one snapshot and one write: a log past 16 MiB, or with a line no pack holds, would
    // be read, deflated and stored whole while the writer waits, and a stop would wait for it.
    let dir = TestDir::new("packs");
    let path = dir.0.join("runnerd.db");
    let store = Store::open(&path).unwrap();
    running(&store, 1);
    let chunk = "x".repeat(450 << 10);
    store.log(1, vec![chunk; 40], POSITION, u64::MAX).await;
    finish(&store, 1, "success");
    running(&store, 2);
    store
        .log(2, vec!["y".repeat(3 << 20)], POSITION, u64::MAX)
        .await;
    finish(&store, 2, "success");
    running(&store, 3);
    store.log(3, long_log(), POSITION, u64::MAX).await;
    finish(&store, 3, "success");
    store.flush().await;
    let pruner = Connection::open(&path).unwrap();
    for id in [1, 2] {
        assert!(
            matches!(
                logs::make(&pruner, id, &|| false).unwrap(),
                logs::Made::Unpackable(_)
            ),
            "job {id} is not packed"
        );
    }
    let stopped = logs::make(&pruner, 3, &|| true).unwrap();
    assert!(
        matches!(stopped, logs::Made::Stopped),
        "a stop is seen while a long log is deflated"
    );
    assert!(
        !logs::store(&pruner, 3, &stopped).unwrap(),
        "a stopped make stores nothing"
    );
    assert_eq!(rows(&path, 3).1, 0);
    store.prune(PACK, &History::default()).await.unwrap();
    assert_eq!(rows(&path, 1), (40, 0), "a log past 16 MiB stays in rows");
    assert_eq!(rows(&path, 2), (1, 0), "a line past a pack stays in rows");
    assert_eq!(rows(&path, 3).0, 0, "the rest packs");
    assert_eq!(store.log_lines(1, 39, 5).await.unwrap().len(), 1);
    // Past `[history] logs`, rows go a few MiB at a time.
    store
        .prune(FINAL + 31 * DAY_MS, &History::default())
        .await
        .unwrap();
    assert_eq!(rows(&path, 1), (0, 0));
    assert_eq!(rows(&path, 2), (0, 0));
}

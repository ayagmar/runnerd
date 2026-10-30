//! GitHub Actions' cache through runners' sockets, as `actions/cache` and BuildKit reach it: a save is a restore in
//! another runner, by key or prefix, after a restart too; each branch's and pull request's entries are their own, so
//! no pull request changes what `main` restores; saves a job may not make are answered and dropped; a save made
//! before its job's decision is answered at once and becomes an entry only once the job may write, at most one per key
//! and scope, and never across a restart; block uploads, ranges and limits; and every other results service request
//! goes to GitHub as it came, with the cache turned off too.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a cache that cannot open fails the test"
)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::super::{ActionsCache, BuildCache, Scopes, Writes};
use super::{call, path_of, restore, save, send};
use crate::sync::lock;
use crate::testdir::TestDir;

fn open(dir: &TestDir, max_size: u64) -> BuildCache {
    BuildCache::open(&dir.0.join("entries"), &dir.0.join("sockets"), max_size).unwrap()
}

/// The scopes of a run of `branch` in a repository whose default branch is `main`.
fn branch(branch: &str) -> Scopes {
    Scopes {
        own: Some(format!("refs/heads/{branch}")),
        base: None,
        default: Some("refs/heads/main".to_owned()),
    }
}

/// The scopes of a job that may not write, in a repository whose default branch is `main`.
fn reader() -> Scopes {
    Scopes {
        default: Some("refs/heads/main".to_owned()),
        ..Scopes::default()
    }
}

/// Saves as `save` does, answered at once whatever GitHub has said of the job: a job's post-step waits for it.
async fn save_now(dir: &Path, key: &str, archive: &[u8]) -> bool {
    tokio::time::timeout(Duration::from_secs(5), save(dir, key, archive))
        .await
        .expect("a save is answered at once")
}

/// Reserves a save of `key`, as BuildKit does before it sends blocks, and returns its upload URL's path.
async fn reserve(dir: &Path, key: &str) -> String {
    let reserved = call(
        dir,
        "CreateCacheEntry",
        &serde_json::json!({ "key": key, "version": super::VERSION }),
    )
    .await;
    assert_eq!(reserved["ok"], true);
    path_of(&reserved["signed_upload_url"])
}

/// Sends `block` as block `id` of the save whose upload URL is `url`, and its answer's status.
async fn put_block(dir: &Path, url: &str, id: &str, block: &[u8]) -> u16 {
    send(
        dir,
        "PUT",
        &format!("{url}?comp=block&blockid={}", id.replace('=', "%3D")),
        &[],
        block,
    )
    .await
    .status
}

/// Joins the blocks `ids` of the save whose upload URL is `url`, and its answer's status.
async fn put_block_list(dir: &Path, url: &str, ids: &[&str]) -> u16 {
    let mut list = String::from("<BlockList>");
    for id in ids {
        list.extend(["<Latest>", id, "</Latest>"]);
    }
    list.push_str("</BlockList>");
    send(
        dir,
        "PUT",
        &format!("{url}?comp=blocklist"),
        &[],
        list.as_bytes(),
    )
    .await
    .status
}

/// Finalizes the save of `key` of `size` bytes, as `@actions/cache` does: whether it was answered as kept.
async fn finalize(dir: &Path, key: &str, size: usize) -> bool {
    call(
        dir,
        "FinalizeCacheEntryUpload",
        &serde_json::json!({ "key": key, "version": super::VERSION, "size_bytes": size.to_string() }),
    )
    .await["ok"]
        == true
}

/// Starts a PUT of `length` bytes to `path` and sends the first of them, `first`: the connection, the rest unsent, as
/// a sender's that stalls.
async fn stalled_put(dir: &Path, path: &str, length: usize, first: &[u8]) -> UnixStream {
    let mut stream = UnixStream::connect(dir.join(crate::buildcache::SOCKET))
        .await
        .unwrap();
    let head = format!(
        "PUT {path} HTTP/1.1\r\nHost: 10.0.0.5:41772\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(first).await.unwrap();
    stream
}

/// Waits, at most 5 s, until `done`, which says what it waits for.
async fn until(done: impl Fn() -> bool, what: &str) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never {what}");
}

/// How many files under `dir` this process still has open though they were removed: their disk stays taken.
fn removed_but_open(dir: &TestDir) -> usize {
    let dir = dir.0.to_string_lossy().into_owned();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .map(|target| target.to_string_lossy().into_owned())
        .filter(|target| target.starts_with(&dir) && target.ends_with(" (deleted)"))
        .count()
}

/// What the uploads not yet entries take of the uploads' room.
fn uploading(cache: &BuildCache) -> u64 {
    lock(&cache.inner.state).uploading
}

/// How many uploads wait on disk to become entries, for the cache in `dir`.
fn uploads(dir: &TestDir) -> usize {
    std::fs::read_dir(dir.0.join("entries").join(super::super::UPLOADS))
        .unwrap()
        .count()
}

/// The scopes of a run of pull request `number` into `base`.
fn pull_request(number: u64, base: &str) -> Scopes {
    Scopes {
        own: Some(format!("refs/pull/{number}/merge")),
        base: Some(format!("refs/heads/{base}")),
        default: Some("refs/heads/main".to_owned()),
    }
}

#[tokio::test]
async fn mains_save_is_restored_by_another_runner_by_key_or_newest_prefix_after_a_restart() {
    let dir = TestDir::new("actions-hit");
    let cache = open(&dir, 1 << 20);
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    assert!(
        main.join(crate::buildcache::ACTIONS_MARKER).is_file(),
        "the image serves the actions cache only when told"
    );
    cache.decide("main", Writes::Allowed, branch("main"));
    assert!(save(&main, "deps-linux-0a1b", b"node_modules v1").await);
    assert!(save(&main, "deps-linux-ffff", b"node_modules v2").await);
    assert!(
        save(&main, "deps-linux-ffff", b"again").await,
        "a key its branch has is answered as kept, as BuildKit fails a build on a refused one"
    );
    assert_eq!(cache.detach("main").unwrap().written_bytes, 30);
    let pull = cache.attach("pull", "o/app", ActionsCache::Served).unwrap();
    let before_restart = call(
        &pull,
        "GetCacheEntryDownloadURL",
        &serde_json::json!({ "key": "deps-linux-ffff", "version": super::VERSION }),
    )
    .await;

    let reopened = open(&dir, 1 << 20);
    let pull = reopened
        .attach("pull", "o/app", ActionsCache::Served)
        .unwrap();
    reopened.decide("pull", Writes::Refused, branch("feature"));
    assert_eq!(
        restore(&pull, "deps-linux-0a1b", &[]).await,
        Some(("deps-linux-0a1b".to_owned(), b"node_modules v1".to_vec()))
    );
    assert_eq!(
        send(
            &pull,
            "GET",
            &path_of(&before_restart["signed_download_url"]),
            &[],
            b""
        )
        .await
        .status,
        404,
        "a download URL from before the restart names no entry this runner found since"
    );
    assert_eq!(
        restore(&pull, "deps-linux-9999", &["deps-linux-", "deps-"]).await,
        Some(("deps-linux-ffff".to_owned(), b"node_modules v2".to_vec())),
        "the newest entry of the first restore key that matches"
    );
    assert_eq!(
        restore(&pull, "deps-macos-0a1b", &["deps-macos-"]).await,
        None
    );
    let counts = reopened.detach("pull").unwrap();
    assert_eq!((counts.hits, counts.misses, counts.read_bytes), (2, 1, 30));

    reopened.clear();
    let after = reopened
        .attach("after", "o/app", ActionsCache::Served)
        .unwrap();
    assert_eq!(
        restore(&after, "deps-linux-0a1b", &[]).await,
        None,
        "a trimmed entry is not found"
    );
}

#[tokio::test]
async fn no_pull_request_changes_what_main_restores() {
    let dir = TestDir::new("actions-scopes");
    let cache = open(&dir, 1 << 20);
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    let own = cache.attach("own", "o/app", ActionsCache::Served).unwrap();
    let fork = cache.attach("fork", "o/app", ActionsCache::Served).unwrap();
    cache.decide("main", Writes::Allowed, branch("main"));
    // A pull request of the repository's own branch that may write (ADR 0053), and a fork's that may not.
    cache.decide("own", Writes::Allowed, pull_request(7, "main"));
    cache.decide("fork", Writes::Refused, branch("main"));
    assert!(save(&main, "deps", b"main's").await);
    assert!(save(&main, "lint", b"main's lint").await);
    assert!(save(&own, "deps", b"feature's").await);
    assert!(
        save(&fork, "deps-1", b"poison").await,
        "a refused save answers as kept, as BuildKit fails a build on a refused one"
    );
    assert_eq!(
        restore(&main, "deps", &["deps-"]).await,
        Some(("deps".to_owned(), b"main's".to_vec()))
    );
    assert_eq!(
        restore(&own, "deps", &[]).await,
        Some(("deps".to_owned(), b"feature's".to_vec())),
        "a pull request reads its own entries first"
    );
    assert_eq!(
        restore(&own, "lint", &[]).await,
        Some(("lint".to_owned(), b"main's lint".to_vec())),
        "then its base branch's"
    );
    assert_eq!(restore(&fork, "deps-1", &[]).await, None);
    assert_eq!(cache.detach("fork").unwrap().refused, 1);

    // Another repository's runner reads none of them.
    let other = cache
        .attach("other", "o/other", ActionsCache::Served)
        .unwrap();
    cache.decide("other", Writes::Allowed, branch("main"));
    assert_eq!(restore(&other, "deps", &[]).await, None);
}

#[tokio::test]
async fn an_undecided_jobs_saves_are_answered_at_once_and_become_entries_once_it_may_write() {
    let dir = TestDir::new("actions-undecided");
    let cache = open(&dir, 1 << 20);
    let first = cache
        .attach("first", "o/app", ActionsCache::Served)
        .unwrap();
    cache.decide("first", Writes::Allowed, branch("main"));
    assert!(save(&first, "gradle-0a1b", b"wrapper").await);
    cache.detach("first");

    let next = cache.attach("next", "o/app", ActionsCache::Served).unwrap();
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    cache.decide("main", Writes::Allowed, branch("main"));
    // Restores come first in a job, before GitHub has said what it is.
    assert_eq!(
        restore(&next, "gradle-0a1b", &[]).await,
        Some(("gradle-0a1b".to_owned(), b"wrapper".to_vec())),
        "an undecided job reads the default branch the last decision named"
    );
    // GitHub's jobs API names no runner for the job yet when its post-step saves, whole and in blocks.
    assert!(save_now(&next, "deps", b"dependencies").await);
    assert!(
        finalize(&next, "deps", 12).await,
        "finalized again, it answers as kept"
    );
    let layers = reserve(&next, "layers").await;
    assert_eq!(
        put_block(&next, &layers, "QkxPQ0sxPT0=", b"layer").await,
        201
    );
    assert_eq!(
        restore(&main, "deps", &[]).await,
        None,
        "nothing is read before GitHub says the job may write"
    );

    cache.decide("next", Writes::Allowed, branch("main"));
    assert_eq!(put_block_list(&next, &layers, &["QkxPQ0sxPT0="]).await, 201);
    assert!(finalize(&next, "layers", 5).await);
    assert_eq!(
        restore(&main, "deps", &[]).await,
        Some(("deps".to_owned(), b"dependencies".to_vec()))
    );
    assert_eq!(
        restore(&main, "layers", &[]).await,
        Some(("layers".to_owned(), b"layer".to_vec()))
    );
    assert_eq!(cache.size().entries, 3, "each save became one entry");
    assert_eq!(uploads(&dir), 0);
    assert_eq!(cache.detach("next").unwrap().written_bytes, 17);
}

#[tokio::test]
async fn a_fork_or_a_job_github_never_names_saves_nothing_and_frees_what_it_sent() {
    let dir = TestDir::new("actions-refused-staged");
    let cache = open(&dir, 64 << 10);
    let fork = cache.attach("fork", "o/app", ActionsCache::Served).unwrap();
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    cache.decide("main", Writes::Allowed, branch("main"));
    // Before GitHub says what the fork's job is: a finalized save, one past the quarter of the limit an undecided
    // job may hold, and one still being sent.
    assert!(save_now(&fork, "deps", &[1; 12 << 10]).await);
    assert!(
        save_now(&fork, "more", &[2; 8 << 10]).await,
        "past its room, answered as kept"
    );
    let layers = reserve(&fork, "layers").await;
    assert_eq!(
        put_block(&fork, &layers, "QkxPQ0sxPT0=", b"poison").await,
        201
    );
    assert_eq!(uploads(&dir), 2, "the finalized archive and the block");

    cache.decide("fork", Writes::Refused, reader());
    assert_eq!(uploads(&dir), 0, "a refusal frees what it sent at once");
    // The rest of the save it was sending answers as Azure and GitHub would, and is dropped.
    assert_eq!(
        put_block(&fork, &layers, "QkxPQ0syPT0=", b"poison").await,
        201
    );
    assert_eq!(
        put_block_list(&fork, &layers, &["QkxPQ0sxPT0=", "QkxPQ0syPT0="]).await,
        201
    );
    assert!(finalize(&fork, "layers", 12).await);
    for key in ["deps", "more", "layers"] {
        assert_eq!(restore(&main, key, &[]).await, None, "{key}");
    }
    assert_eq!(cache.detach("fork").unwrap().refused, 3);

    // A job GitHub never named a runner for: its runner ends with a save finalized and one it cannot finish.
    let slow = cache.attach("slow", "o/app", ActionsCache::Served).unwrap();
    assert!(save_now(&slow, "deps", b"late").await);
    let half = reserve(&slow, "half").await;
    assert_eq!(put_block(&slow, &half, "QkxPQ0sxPT0=", b"half").await, 201);
    assert!(
        cache.close("slow"),
        "its finalized save waits for GitHub without its socket"
    );
    assert_eq!(uploads(&dir), 1, "the save it can no longer finish is gone");
    let counts = cache.detach("slow").unwrap();
    assert_eq!((counts.refused, counts.written_bytes), (1, 0));
    assert!(!counts.writer, "it never wrote");
    cache.decide("slow", Writes::Allowed, branch("main"));
    assert_eq!(restore(&main, "deps", &[]).await, None);
    assert_eq!(cache.size().entries, 0);
    assert_eq!(uploads(&dir), 0);
    assert_eq!(uploading(&cache), 0, "the uploads' room is all free again");
}

#[tokio::test]
async fn a_refused_jobs_uploads_give_back_their_room_while_their_senders_stall() {
    let dir = TestDir::new("actions-refused-stalled");
    let cache = open(&dir, 64 << 10);
    let fork = cache.attach("fork", "o/app", ActionsCache::Served).unwrap();
    let whole = reserve(&fork, "whole").await;
    let layers = reserve(&fork, "layers").await;
    let block = format!("{layers}?comp=block&blockid=QkxPQ0sxPT0%3D");
    // Each upload sends its first KiB, then stalls: an archive, a block and a Gradle entry.
    let length = 16 << 10;
    let mut stalled = Vec::new();
    for path in [whole.as_str(), block.as_str(), "/gradle/0a1b"] {
        stalled.push(stalled_put(&fork, path, length, &[1; 1 << 10]).await);
    }
    until(|| uploads(&dir) == 3, "were the three uploads on disk").await;

    // Another undecided job passes the quarter of the limit it may hold, then stalls.
    let greedy = cache
        .attach("greedy", "o/app", ActionsCache::Served)
        .unwrap();
    let big = reserve(&greedy, "big").await;
    let mut over = stalled_put(&greedy, &big, 32 << 10, &[3; 1 << 10]).await;
    until(|| uploads(&dir) == 4, "was the greedy upload on disk").await;
    over.write_all(&[3; 16 << 10]).await.unwrap();
    until(
        || uploads(&dir) == 3 && uploading(&cache) == 12 << 10,
        "did the upload past its room give it back",
    )
    .await;
    assert_eq!(
        removed_but_open(&dir),
        0,
        "the upload past its room closed its file while its sender stalls"
    );

    cache.decide("fork", Writes::Refused, reader());
    until(
        || uploads(&dir) == 0 && uploading(&cache) == 0,
        "did the refusal free the stalled uploads",
    )
    .await;
    assert_eq!(
        removed_but_open(&dir),
        0,
        "the refused uploads closed their files while their senders stall"
    );
    drop(over);
    // The rest of each body is read and dropped, and answered as stored.
    for mut stream in stalled {
        stream
            .write_all(&vec![1; length - (1 << 10)])
            .await
            .unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.unwrap();
        let status = String::from_utf8_lossy(&answer[9..12]).into_owned();
        assert!(status == "201" || status == "200", "{status}");
    }
    assert_eq!((uploads(&dir), uploading(&cache)), (0, 0));
    let other = cache
        .attach("other", "o/other", ActionsCache::Served)
        .unwrap();
    cache.decide("other", Writes::Allowed, branch("main"));
    assert!(save_now(&other, "deps", &[2; 16 << 10]).await);
    assert_eq!(
        restore(&other, "deps", &[])
            .await
            .map(|(_, archive)| archive.len()),
        Some(16 << 10),
        "another repository's save finds the room"
    );
}

#[tokio::test]
async fn a_runner_removed_while_its_blocks_join_leaves_nothing_behind() {
    let dir = TestDir::new("actions-join-removed");
    let cache = open(&dir, 1 << 20);
    let gone = cache.attach("gone", "o/app", ActionsCache::Served).unwrap();
    let ended = cache
        .attach("ended", "o/app", ActionsCache::Served)
        .unwrap();
    // `ended` keeps a finalized save for GitHub's decision once its runner is removed.
    assert!(save_now(&ended, "kept", b"kept").await);
    cache.inner.join_gate.hold();
    let mut joins = Vec::new();
    for socket in [&gone, &ended] {
        let layers = reserve(socket, "layers").await;
        assert_eq!(
            put_block(socket, &layers, "QkxPQ0sxPT0=", b"layer").await,
            201
        );
        let socket = socket.clone();
        joins.push(tokio::spawn(async move {
            // The connection is cut when the runner goes.
            let list = b"<BlockList><Latest>QkxPQ0sxPT0=</Latest></BlockList>";
            let path = format!("{layers}?comp=blocklist");
            let mut stream = stalled_put(&socket, &path, list.len(), b"").await;
            let _ = stream.write_all(list).await;
            let _ = stream.read_to_end(&mut Vec::new()).await;
        }));
    }
    until(
        || cache.inner.join_gate.waiting() == 2,
        "did both joins start",
    )
    .await;

    cache.detach("gone");
    assert!(cache.close("ended"), "its finalized save waits for GitHub");
    cache.inner.join_gate.release();
    for join in joins {
        join.await.unwrap();
    }
    until(
        || uploads(&dir) == 1 && uploading(&cache) == 4 << 10,
        "did the joins remove what they no longer keep",
    )
    .await;
    cache.detach("ended");
    assert_eq!((uploads(&dir), uploading(&cache)), (0, 0));
}

#[tokio::test]
async fn an_empty_block_list_takes_the_room_any_archive_does() {
    let dir = TestDir::new("actions-empty-joins");
    // An undecided job may hold a quarter of it: one archive's least charge.
    let cache = open(&dir, 16 << 10);
    let fork = cache.attach("fork", "o/app", ActionsCache::Served).unwrap();
    for n in 0..3 {
        let key = format!("empty-{n}");
        let url = reserve(&fork, &key).await;
        assert_eq!(put_block_list(&fork, &url, &[]).await, 201);
        assert!(finalize(&fork, &key, 0).await);
    }
    assert_eq!(
        (uploads(&dir), uploading(&cache)),
        (1, 4 << 10),
        "only the first fit in the job's room"
    );
    cache.decide("fork", Writes::Refused, reader());
    assert_eq!((uploads(&dir), uploading(&cache)), (0, 0));
}

#[tokio::test]
async fn of_saves_of_one_key_into_one_scope_at_most_one_becomes_an_entry() {
    let dir = TestDir::new("actions-one-per-key");
    let cache = open(&dir, 1 << 20);
    let [first, second, decided] = ["first", "second", "decided"]
        .map(|runner| cache.attach(runner, "o/app", ActionsCache::Served).unwrap());
    cache.decide("decided", Writes::Allowed, branch("main"));
    // Two jobs of main save the same key before GitHub names their runners; a third, decided, saves a key the first
    // is waiting to save.
    assert!(save_now(&first, "deps", b"first's").await);
    assert!(save_now(&second, "deps", b"second's").await);
    assert!(save_now(&first, "lint", b"first's lint").await);
    assert!(save_now(&decided, "lint", b"decided's lint").await);
    cache.decide("first", Writes::Allowed, branch("main"));
    cache.decide("second", Writes::Allowed, branch("main"));
    assert_eq!(
        restore(&decided, "deps", &[]).await,
        Some(("deps".to_owned(), b"first's".to_vec())),
        "the first decided keeps the key"
    );
    assert_eq!(
        restore(&decided, "lint", &[]).await,
        Some(("lint".to_owned(), b"decided's lint".to_vec())),
        "the save already an entry keeps the key"
    );
    assert_eq!(cache.size().entries, 2);
    assert_eq!(uploads(&dir), 0, "the saves that lost are gone");

    // The same key in another branch, a pull request and another repository is each one's own.
    let others = [
        ("release", "o/app", branch("release")),
        ("pull", "o/app", pull_request(7, "main")),
        ("other", "o/other", branch("main")),
    ];
    for (runner, repository, scopes) in others {
        let socket = cache
            .attach(runner, repository, ActionsCache::Served)
            .unwrap();
        assert!(save_now(&socket, "deps", runner.as_bytes()).await);
        cache.decide(runner, Writes::Allowed, scopes);
        assert_eq!(
            restore(&socket, "deps", &[]).await,
            Some(("deps".to_owned(), runner.as_bytes().to_vec())),
            "{runner}"
        );
    }
    assert_eq!(cache.size().entries, 5);
}

#[tokio::test]
async fn a_restart_drops_saves_waiting_for_a_decision_and_their_upload_urls_reach_nothing_after() {
    let dir = TestDir::new("actions-restart-staged");
    let cache = open(&dir, 1 << 20);
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    let stale = reserve(&main, "deps").await;
    assert!(save_now(&main, "tools", b"tools").await);
    drop(cache);

    let cache = open(&dir, 1 << 20);
    assert_eq!(uploads(&dir), 0, "what waited for a decision is gone");
    // The running job is adopted, and GitHub decides it afresh.
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    cache.decide("main", Writes::Allowed, branch("main"));
    let fresh = reserve(&main, "other").await;
    assert_ne!(stale, fresh, "an upload URL is never given twice");
    // The job's upload client sends again to the URL it had before the restart.
    let resent = send(
        &main,
        "PUT",
        &stale,
        &[("x-ms-blob-type", "BlockBlob")],
        b"stale",
    )
    .await;
    assert_eq!(resent.status, 201);
    assert!(finalize(&main, "deps", 5).await);
    assert!(finalize(&main, "other", 5).await);
    for key in ["tools", "deps", "other"] {
        assert_eq!(restore(&main, key, &[]).await, None, "{key}");
    }
    assert_eq!(cache.size().entries, 0);
}

#[tokio::test]
async fn blocks_join_in_the_lists_order_and_a_range_reads_part_of_an_entry() {
    let dir = TestDir::new("actions-blocks");
    let cache = open(&dir, 1 << 20);
    let builder = cache
        .attach("builder", "o/app", ActionsCache::Served)
        .unwrap();
    cache.decide("builder", Writes::Allowed, branch("main"));
    let reserved = call(
        &builder,
        "CreateCacheEntry",
        &serde_json::json!({ "key": "buildkit-blob-sha256:0a1b", "version": super::VERSION }),
    )
    .await;
    let url = path_of(&reserved["signed_upload_url"]);
    // BuildKit's Azure client sends blocks in parallel, so they may arrive in any order; a block sent again
    // replaces the first.
    for (id, block) in [
        ("QkxPQ0syPT0=", "-layer"),
        ("QkxPQ0sxPT0=", "draft"),
        ("QkxPQ0sxPT0=", "image"),
    ] {
        let sent = send(
            &builder,
            "PUT",
            &format!("{url}?comp=block&blockid={}", id.replace('=', "%3D")),
            &[],
            block.as_bytes(),
        )
        .await;
        assert_eq!(sent.status, 201);
        assert!(
            sent.header("x-ms-request-id").is_some(),
            "BuildKit's client needs one"
        );
    }
    let list = "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>QkxPQ0sxPT0=</Latest>\
                <Latest>QkxPQ0syPT0=</Latest></BlockList>";
    assert_eq!(
        send(
            &builder,
            "PUT",
            &format!("{url}?comp=blocklist"),
            &[],
            list.as_bytes()
        )
        .await
        .status,
        201
    );
    let finalized = call(
        &builder,
        "FinalizeCacheEntryUpload",
        &serde_json::json!({ "key": "buildkit-blob-sha256:0a1b", "version": super::VERSION, "size_bytes": 11 }),
    )
    .await;
    assert_eq!(finalized["ok"], true);

    let found = call(
        &builder,
        "GetCacheEntryDownloadURL",
        &serde_json::json!({ "key": "buildkit-blob-sha256:0a1b", "restore_keys": [], "version": super::VERSION }),
    )
    .await;
    let entry = path_of(&found["signed_download_url"]);
    assert_eq!(
        send(&builder, "GET", &entry, &[], b"").await.body,
        b"image-layer"
    );
    let part = send(&builder, "GET", &entry, &[("x-ms-range", "bytes=6-")], b"").await;
    assert_eq!(
        (part.status, part.body.as_slice()),
        (206, b"layer".as_slice())
    );
    assert_eq!(part.header("content-range"), Some("bytes 6-10/11"));
    let beyond = send(&builder, "GET", &entry, &[("Range", "bytes=11-")], b"").await;
    assert_eq!(beyond.status, 416);
}

#[tokio::test]
async fn an_archive_over_a_quarter_of_the_limit_is_dropped_and_its_room_given_back() {
    let dir = TestDir::new("actions-limits");
    let cache = open(&dir, 64 << 10);
    let main = cache.attach("main", "o/app", ActionsCache::Served).unwrap();
    cache.decide("main", Writes::Allowed, branch("main"));
    assert!(
        save(&main, "huge", &vec![7; 20 << 10]).await,
        "answered as kept, as GitHub's service does"
    );
    assert_eq!(restore(&main, "huge", &[]).await, None);
    for n in 0..3 {
        assert!(save(&main, &format!("small-{n}"), &vec![1; 8 << 10]).await);
    }
    assert_eq!(cache.size().entries, 3, "the dropped archive took no room");
    let counts = cache.detach("main").unwrap();
    assert_eq!((counts.refused, counts.written_bytes), (1, 24 << 10));
}

#[tokio::test]
async fn an_archive_in_blocks_over_a_quarter_of_the_limit_is_dropped_and_its_room_given_back() {
    let dir = TestDir::new("actions-block-limits");
    let cache = open(&dir, 64 << 10);
    let builder = cache
        .attach("builder", "o/app", ActionsCache::Served)
        .unwrap();
    cache.decide("builder", Writes::Allowed, branch("main"));
    let reserved = call(
        &builder,
        "CreateCacheEntry",
        &serde_json::json!({ "key": "buildkit-blob-sha256:ffff", "version": super::VERSION }),
    )
    .await;
    let url = path_of(&reserved["signed_upload_url"]);
    // Each block is under the quarter; together they are over it.
    let mut list = String::from("<BlockList>");
    for id in ["QkxPQ0sxPT0=", "QkxPQ0syPT0=", "QkxPQ0szPT0="] {
        let sent = send(
            &builder,
            "PUT",
            &format!("{url}?comp=block&blockid={}", id.replace('=', "%3D")),
            &[],
            &vec![7; 8 << 10],
        )
        .await;
        assert_eq!(sent.status, 201, "answered as kept, as Azure would");
        list.extend(["<Latest>", id, "</Latest>"]);
    }
    list.push_str("</BlockList>");
    let joined = send(
        &builder,
        "PUT",
        &format!("{url}?comp=blocklist"),
        &[],
        list.as_bytes(),
    )
    .await;
    assert_eq!(joined.status, 201);
    let finalized = call(
        &builder,
        "FinalizeCacheEntryUpload",
        &serde_json::json!({ "key": "buildkit-blob-sha256:ffff", "version": super::VERSION, "size_bytes": 24 << 10 }),
    )
    .await;
    assert_eq!(finalized["ok"], true);
    assert_eq!(
        restore(&builder, "buildkit-blob-sha256:ffff", &[]).await,
        None
    );
    for n in 0..3 {
        assert!(save(&builder, &format!("small-{n}"), &vec![1; 8 << 10]).await);
    }
    assert_eq!(cache.size().entries, 3, "the dropped blocks took no room");
    let counts = cache.detach("builder").unwrap();
    assert_eq!((counts.refused, counts.written_bytes), (1, 24 << 10));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_artifact_request_goes_to_github_as_it_came_with_the_cache_turned_off_too() {
    use axum::routing::post;

    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = axum::Router::new().route(
        "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
        post({
            let seen = seen.clone();
            move |headers: axum::http::HeaderMap, body: String| async move {
                let token = headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                seen.lock().unwrap().push((token, body));
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    "{\"ok\":true,\"signed_upload_url\":\"https://blob.example/up\"}",
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = TestDir::new("actions-artifacts");
    let cache = open(&dir, 1 << 20);
    cache.set_results_origin(&origin);
    let app = cache.attach("app", "o/app", ActionsCache::Served).unwrap();
    cache.decide("app", Writes::Allowed, branch("main"));
    assert!(save(&app, "deps", b"main's").await);
    // A restart that turned it off adopts the running job, whose container still has the forwarder and preload.
    let adopted = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    assert_eq!(app, adopted);
    assert!(
        !app.join(crate::buildcache::ACTIONS_MARKER).exists(),
        "the slot's next runner uses GitHub's cache"
    );
    cache.decide("app", Writes::Allowed, branch("main"));
    for name in ["report", "logs"] {
        let answer = send(
            &app,
            "POST",
            "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
            &[
                ("Authorization", "Bearer runtime-token"),
                ("Content-Type", "application/json"),
            ],
            format!("{{\"name\":\"{name}\"}}").as_bytes(),
        )
        .await;
        assert_eq!(answer.status, 200);
        assert_eq!(
            answer.json()["signed_upload_url"],
            "https://blob.example/up"
        );
    }
    assert_eq!(
        *seen.lock().unwrap(),
        ["report", "logs"].map(|name| (
            "Bearer runtime-token".to_owned(),
            format!("{{\"name\":\"{name}\"}}")
        ))
    );
    assert_eq!(restore(&app, "deps", &[]).await, None, "it reads nothing");
    assert!(
        save(&app, "deps-1", b"late").await,
        "a save is answered as kept, as BuildKit fails a build on a refused one"
    );
    assert_eq!(cache.size().entries, 1, "and dropped");
}

//! GitHub Actions' cache through runners' sockets, as `actions/cache` and BuildKit reach it: a save is a restore in
//! another runner, by key or prefix, after a restart too; each branch's and pull request's entries are their own, so
//! no pull request changes what `main` restores; saves a job may not make are answered and dropped; a save waits for
//! its job's decision; block uploads, ranges and limits; and every other results service request goes to GitHub as
//! it came, with the cache turned off too.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a cache that cannot open fails the test"
)]

use std::sync::Arc;

use super::super::{ActionsCache, BuildCache, Scopes, Writes};
use super::{call, path_of, restore, save, send};
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
async fn an_undecided_jobs_save_waits_for_its_decision_and_its_lookups_read_the_default_branch() {
    let dir = TestDir::new("actions-undecided");
    let cache = open(&dir, 1 << 20);
    let first = cache
        .attach("first", "o/app", ActionsCache::Served)
        .unwrap();
    cache.decide("first", Writes::Allowed, branch("main"));
    assert!(save(&first, "gradle-0a1b", b"wrapper").await);
    cache.detach("first");

    let next = cache.attach("next", "o/app", ActionsCache::Served).unwrap();
    // Restores come first in a job, before GitHub has said what it is.
    assert_eq!(
        restore(&next, "gradle-0a1b", &[]).await,
        Some(("gradle-0a1b".to_owned(), b"wrapper".to_vec())),
        "an undecided job reads the default branch the last decision named"
    );
    let saving = tokio::spawn({
        let next = next.clone();
        async move { save(&next, "gradle-ffff", b"dependencies").await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!saving.is_finished(), "the save waits for the decision");
    cache.decide("next", Writes::Allowed, branch("main"));
    assert!(saving.await.unwrap());
    assert_eq!(
        restore(&next, "gradle-ffff", &[]).await,
        Some(("gradle-ffff".to_owned(), b"dependencies".to_vec()))
    );
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

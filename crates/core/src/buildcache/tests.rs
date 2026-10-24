//! The build cache through runners' sockets, as their tools reach it: who may write, what a repository sees of
//! another's, what an undecided job's uploads become, what a trim keeps, and what survives a restart.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a cache that cannot open fails the test"
)]

use super::*;
use crate::testdir::TestDir;

fn trigger(event: &str, branch: &str, head: &str) -> RunTrigger {
    RunTrigger {
        event: event.to_owned(),
        head_branch: Some(branch.to_owned()),
        head_repository: Some(head.to_owned()),
        head_sha: "0a1b2c".to_owned(),
    }
}

fn main_only() -> Vec<String> {
    vec!["main".to_owned()]
}

#[test]
fn only_pushes_schedules_and_manual_runs_of_a_writing_branch_may_write() {
    let decide = |trigger: &RunTrigger| writes("r1", Some("r1"), "o/app", trigger, &main_only());
    assert_eq!(decide(&trigger("push", "main", "o/app")), Writes::Allowed);
    assert_eq!(
        decide(&trigger("schedule", "main", "o/app")),
        Writes::Allowed
    );
    assert_eq!(
        decide(&trigger("workflow_dispatch", "main", "o/app")),
        Writes::Allowed
    );
    assert_eq!(
        decide(&trigger("pull_request", "main", "o/app")),
        Writes::Refused,
        "a pull request whose head branch is called main"
    );
    assert_eq!(
        decide(&trigger("pull_request_target", "main", "o/app")),
        Writes::Refused
    );
    assert_eq!(
        decide(&trigger("push", "feature", "o/app")),
        Writes::Refused
    );
    assert_eq!(
        decide(&trigger("push", "main", "fork/app")),
        Writes::Refused,
        "a commit from another repository"
    );
}

#[test]
fn a_job_github_places_on_another_runner_never_writes() {
    let push = trigger("push", "main", "o/app");
    assert_eq!(
        writes("r1", Some("r2"), "o/app", &push, &main_only()),
        Writes::Refused,
        "a runner whose log claims another runner's job"
    );
    assert_eq!(
        writes("r1", None, "o/app", &push, &main_only()),
        Writes::Undecided
    );
}

struct Cache {
    cache: BuildCache,
    _dir: TestDir,
}

fn open(name: &str, max_size: u64) -> Cache {
    let dir = TestDir::new(name);
    let cache = BuildCache::open(&dir.0.join("entries"), &dir.0.join("sockets"), max_size).unwrap();
    Cache { cache, _dir: dir }
}

#[tokio::test]
async fn a_main_builds_entry_is_a_hit_for_another_runner_of_its_repository() {
    let Cache { cache, _dir } = open("hit", 1 << 20);
    let main = cache.attach("main", "o/app").unwrap();
    let pull = cache.attach("pull", "o/app").unwrap();
    cache.decide("main", Writes::Allowed);
    assert_eq!(
        request(&main, "PUT", "/gradle/0a1b", b"outputs").await,
        (200, Vec::new())
    );
    assert_eq!(
        request(&pull, "GET", "/gradle/0a1b", b"").await,
        (200, b"outputs".to_vec())
    );
    assert_eq!(request(&pull, "GET", "/gradle/ffff", b"").await.0, 404);
    let counts = cache.detach("pull").unwrap();
    assert_eq!((counts.hits, counts.misses, counts.read_bytes), (1, 1, 7));
    let counts = cache.detach("main").unwrap();
    assert_eq!(counts.written_bytes, 7);
    assert!(counts.writer, "main's runner could write");
}

#[tokio::test]
async fn an_untrusted_jobs_upload_is_accepted_and_never_seen() {
    let Cache { cache, _dir } = open("untrusted", 1 << 20);
    let pull = cache.attach("pull", "o/app").unwrap();
    let main = cache.attach("main", "o/app").unwrap();
    cache.decide("pull", Writes::Refused);
    // A tool told its upload failed would stop using the cache, reads included.
    assert_eq!(
        request(&pull, "PUT", "/gradle/0a1b", b"poison").await.0,
        200
    );
    assert_eq!(request(&main, "GET", "/gradle/0a1b", b"").await.0, 404);
    let counts = cache.detach("pull").unwrap();
    assert_eq!((counts.refused, counts.written_bytes), (1, 0));
    assert!(!counts.writer, "a pull request's runner only reads");
}

#[tokio::test]
async fn uploads_before_the_decision_wait_for_it() {
    let Cache { cache, _dir } = open("held", 1 << 20);
    let early = cache.attach("early", "o/app").unwrap();
    let pull = cache.attach("pull", "o/app").unwrap();
    let reader = cache.attach("reader", "o/app").unwrap();
    assert_eq!(request(&early, "PUT", "/gradle/aa", b"main").await.0, 200);
    assert_eq!(request(&pull, "PUT", "/gradle/bb", b"poison").await.0, 200);
    assert_eq!(
        request(&reader, "GET", "/gradle/aa", b"").await.0,
        404,
        "not before GitHub says the job may write"
    );
    cache.decide("early", Writes::Allowed);
    cache.decide("pull", Writes::Refused);
    assert_eq!(
        request(&reader, "GET", "/gradle/aa", b"").await,
        (200, b"main".to_vec())
    );
    assert_eq!(request(&reader, "GET", "/gradle/bb", b"").await.0, 404);
    assert_eq!(cache.detach("pull").unwrap().refused, 1);
}

#[tokio::test]
async fn a_runner_that_ends_undecided_stores_nothing() {
    let Cache { cache, _dir } = open("ended", 1 << 20);
    let gone = cache.attach("gone", "o/app").unwrap();
    let reader = cache.attach("reader", "o/app").unwrap();
    assert_eq!(request(&gone, "PUT", "/gradle/aa", b"main").await.0, 200);
    assert_eq!(cache.detach("gone").unwrap().refused, 1);
    cache.decide("gone", Writes::Allowed);
    assert_eq!(request(&reader, "GET", "/gradle/aa", b"").await.0, 404);
    assert_eq!(cache.size().entries, 0);
}

#[tokio::test]
async fn repositories_never_read_each_others_entries() {
    let Cache { cache, _dir } = open("repositories", 1 << 20);
    let app = cache.attach("app", "o/app").unwrap();
    let other = cache.attach("other", "o/other").unwrap();
    cache.decide("app", Writes::Allowed);
    cache.decide("other", Writes::Allowed);
    assert_eq!(request(&app, "PUT", "/gradle/0a1b", b"app").await.0, 200);
    assert_eq!(request(&other, "GET", "/gradle/0a1b", b"").await.0, 404);
    assert_eq!(
        request(&other, "PUT", "/gradle/0a1b", b"other").await.0,
        200
    );
    assert_eq!(
        request(&app, "GET", "/gradle/0a1b", b"").await,
        (200, b"app".to_vec())
    );
}

#[tokio::test]
async fn each_tool_has_its_own_keys_and_no_path_leaves_them() {
    let Cache { cache, _dir } = open("paths", 1 << 20);
    let app = cache.attach("app", "o/app").unwrap();
    cache.decide("app", Writes::Allowed);
    assert_eq!(request(&app, "PUT", "/gradle/abc", b"gradle").await.0, 200);
    assert_eq!(
        request(&app, "PUT", "/sccache/a/b/c/abc", b"rustc").await.0,
        200
    );
    assert_eq!(request(&app, "PUT", "/bazel/cas/abc", b"blob").await.0, 200);
    assert_eq!(request(&app, "GET", "/bazel/ac/abc", b"").await.0, 404);
    assert_eq!(
        request(&app, "GET", "/sccache/a/b/c/abc", b"").await,
        (200, b"rustc".to_vec())
    );
    for path in [
        "/gradle/../../other/app/gradle/abc",
        "/gradle/%2e%2e/abc",
        "/turbo/v8/artifacts/abc",
        "/gradle/",
        "/abc",
    ] {
        assert_eq!(request(&app, "GET", path, b"").await.0, 404, "{path}");
        assert_eq!(request(&app, "PUT", path, b"x").await.0, 404, "{path}");
    }
    assert_eq!(cache.size().entries, 3);
}

#[tokio::test]
async fn a_trim_drops_the_least_recently_used_entries_first() {
    let Cache { cache, _dir } = open("trim", 1 << 20);
    let app = cache.attach("app", "o/app").unwrap();
    cache.decide("app", Writes::Allowed);
    for key in ["old", "used", "new"] {
        assert_eq!(
            request(&app, "PUT", &format!("/gradle/{key}"), &[0; 100])
                .await
                .0,
            200
        );
        // Entries are ordered by the millisecond they were last used.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(request(&app, "GET", "/gradle/used", b"").await.0, 200);
    cache.set_max_size(200);
    let trimmed = cache.trim();
    assert_eq!(trimmed.before.bytes, 300);
    assert_eq!(trimmed.after.bytes, 200);
    assert_eq!(request(&app, "GET", "/gradle/old", b"").await.0, 404);
    assert_eq!(request(&app, "GET", "/gradle/used", b"").await.0, 200);
    assert_eq!(request(&app, "GET", "/gradle/new", b"").await.0, 200);
    assert_eq!(
        cache.clear().after,
        Size {
            bytes: 0,
            entries: 0
        }
    );
    assert_eq!(request(&app, "GET", "/gradle/new", b"").await.0, 404);
}

#[tokio::test]
async fn a_restart_keeps_the_entries_and_an_adopted_runner_its_socket() {
    let dir = TestDir::new("restart");
    let (entries, sockets) = (dir.0.join("entries"), dir.0.join("sockets"));
    let first = BuildCache::open(&entries, &sockets, 1 << 20).unwrap();
    let mounted = first.attach("app", "o/app").unwrap();
    first.decide("app", Writes::Allowed);
    assert_eq!(
        request(&mounted, "PUT", "/gradle/abc", b"kept").await.0,
        200
    );
    drop(first);
    let second = BuildCache::open(&entries, &sockets, 1 << 20).unwrap();
    assert_eq!(
        second.size(),
        Size {
            bytes: 4,
            entries: 1
        }
    );
    second.forget_others(&["app"]);
    // The container still mounts the same directory.
    assert_eq!(second.attach("app", "o/app").unwrap(), mounted);
    assert_eq!(
        request(&mounted, "GET", "/gradle/abc", b"").await,
        (200, b"kept".to_vec())
    );
    second.forget_others(&[]);
    assert!(!mounted.exists(), "a runner gone while no daemon ran");
}

#[tokio::test]
async fn sockets_are_reachable_by_a_containers_root_and_entries_by_no_one_else() {
    let Cache { cache, _dir } = open("modes", 1 << 20);
    let dir = cache.attach("app", "o/app").unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.join(SOCKET)), 0o666);
    assert_eq!(mode(&dir), 0o755);
    assert_eq!(mode(dir.parent().unwrap()), 0o700);
    assert_eq!(mode(cache.directory()), 0o700);
}

/// How many uploads wait on disk to become entries.
fn waiting(cache: &BuildCache) -> usize {
    std::fs::read_dir(cache.directory().with_file_name(UPLOADS))
        .unwrap()
        .count()
}

#[tokio::test]
async fn undecided_jobs_uploads_take_at_most_the_size_limit() {
    let Cache { cache, _dir } = open("held-limit", 400);
    let mut forks = Vec::new();
    for fork in 0..5 {
        let name = format!("fork-{fork}");
        let socket = cache.attach(&name, "o/app").unwrap();
        assert_eq!(request(&socket, "PUT", "/gradle/a", &[0; 100]).await.0, 200);
        forks.push(name);
    }
    assert_eq!(waiting(&cache), 4, "the fifth upload was not kept on disk");
    for fork in &forks[..4] {
        cache.decide(fork, Writes::Refused);
    }
    // The room the held uploads took is free again.
    let last = cache.attach("fork-4", "o/app").unwrap();
    assert_eq!(request(&last, "PUT", "/gradle/a", &[0; 100]).await.0, 200);
    assert_eq!(waiting(&cache), 1);
    assert_eq!(cache.detach("fork-4").unwrap().refused, 1);
}

#[tokio::test]
async fn one_undecided_job_takes_at_most_a_quarter_of_the_uploads_room() {
    let Cache { cache, _dir } = open("held-share", 800);
    let fork = cache.attach("fork", "o/app").unwrap();
    for key in 0..8 {
        let path = format!("/gradle/{key}");
        assert_eq!(request(&fork, "PUT", &path, &[0; 100]).await.0, 200);
    }
    assert_eq!(waiting(&cache), 2, "the fork kept a quarter of the room");
    // Other jobs GitHub has not answered for yet, main's among them, find the rest.
    for other in ["main", "nightly", "release"] {
        let socket = cache.attach(other, "o/app").unwrap();
        for key in ["a", "b"] {
            let path = format!("/gradle/{key}");
            assert_eq!(request(&socket, "PUT", &path, &[0; 100]).await.0, 200);
        }
    }
    assert_eq!(waiting(&cache), 8, "every other job's uploads were kept");
    cache.decide("main", Writes::Allowed);
    assert_eq!(cache.size().entries, 2);
    cache.decide("fork", Writes::Refused);
    assert_eq!(cache.detach("fork").unwrap().refused, 8);
}

#[tokio::test]
async fn an_entry_over_a_quarter_of_the_limit_or_past_twice_it_is_refused() {
    let Cache { cache, _dir } = open("limits", 400);
    let app = cache.attach("app", "o/app").unwrap();
    cache.decide("app", Writes::Allowed);
    assert_eq!(request(&app, "PUT", "/gradle/big", &[0; 101]).await.0, 200);
    assert_eq!(request(&app, "GET", "/gradle/big", b"").await.0, 404);
    for key in 0..8 {
        let path = format!("/gradle/{key}");
        assert_eq!(request(&app, "PUT", &path, &[0; 100]).await.0, 200);
    }
    assert_eq!(cache.size().bytes, 800);
    assert_eq!(request(&app, "PUT", "/gradle/more", &[0; 100]).await.0, 200);
    assert_eq!(request(&app, "GET", "/gradle/more", b"").await.0, 404);
    let counts = cache.detach("app").unwrap();
    assert_eq!((counts.refused, counts.written_bytes), (2, 800));
}

#[tokio::test]
async fn an_entry_only_ever_sent_again_stays_in_use() {
    let Cache { cache, _dir } = open("sent-again", 1 << 20);
    let app = cache.attach("app", "o/app").unwrap();
    cache.decide("app", Writes::Allowed);
    for key in ["blob", "old"] {
        assert_eq!(
            request(&app, "PUT", &format!("/bazel/cas/{key}"), &[0; 100])
                .await
                .0,
            200
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Bazel sends the blob again with the next build's outputs, and never reads it.
    assert_eq!(
        request(&app, "PUT", "/bazel/cas/blob", &[0; 100]).await.0,
        200
    );
    cache.set_max_size(100);
    cache.trim();
    assert_eq!(request(&app, "GET", "/bazel/cas/blob", b"").await.0, 200);
    assert_eq!(request(&app, "GET", "/bazel/cas/old", b"").await.0, 404);
}

#[tokio::test]
async fn files_runnerd_did_not_write_in_its_directory_are_never_counted_or_removed() {
    let dir = TestDir::new("foreign");
    std::fs::create_dir_all(dir.0.join("o/app/gradle")).unwrap();
    std::fs::write(dir.0.join("o/app/gradle/notes"), b"mine").unwrap();
    std::fs::write(dir.0.join("photo.jpg"), b"mine").unwrap();
    let cache = BuildCache::open(&dir.0, &dir.0.join("sockets"), 1 << 20).unwrap();
    assert_eq!(cache.size().entries, 0);
    cache.clear();
    assert!(dir.0.join("o/app/gradle/notes").exists(), "notes stay");
    assert!(dir.0.join("photo.jpg").exists(), "the photo stays");
}

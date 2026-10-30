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
        by_user: true,
        pull_requests: Vec::new(),
    }
}

fn main_only() -> Vec<String> {
    vec!["main".to_owned()]
}

#[test]
fn only_pushes_schedules_and_manual_runs_of_a_writing_branch_may_write() {
    let decide = |trigger: &RunTrigger| {
        writes(
            "r1",
            Some("r1"),
            "o/app",
            trigger,
            &main_only(),
            BuildCacheWriters::DefaultBranch,
        )
    };
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
        writes(
            "r1",
            Some("r2"),
            "o/app",
            &push,
            &main_only(),
            BuildCacheWriters::DefaultBranch
        ),
        Writes::Refused,
        "a runner whose log claims another runner's job"
    );
    assert_eq!(
        writes(
            "r1",
            None,
            "o/app",
            &push,
            &main_only(),
            BuildCacheWriters::DefaultBranch
        ),
        Writes::Undecided
    );
}

/// A `pull_request` run of `head`'s branch `feature`, for pull request 7, that a person started.
fn pull_request(head: &str) -> RunTrigger {
    RunTrigger {
        pull_requests: vec![7],
        ..trigger(PULL_REQUEST_EVENT, "feature", head)
    }
}

#[test]
fn a_repository_that_lets_pull_requests_write_trusts_only_its_own_branches_and_people() {
    let decide = |trigger: &RunTrigger| {
        writes(
            "r1",
            Some("r1"),
            "o/app",
            trigger,
            &main_only(),
            BuildCacheWriters::PullRequests,
        )
    };
    assert_eq!(decide(&pull_request("o/app")), Writes::Allowed);
    assert_eq!(
        decide(&pull_request("fork/app")),
        Writes::Refused,
        "a fork's pull request"
    );
    assert_eq!(
        decide(&RunTrigger {
            by_user: false,
            ..pull_request("o/app")
        }),
        Writes::Refused,
        "a run a bot pushed or re-ran"
    );
    assert_eq!(
        decide(&RunTrigger {
            pull_requests: Vec::new(),
            ..pull_request("o/app")
        }),
        Writes::Refused,
        "a run GitHub ties to no open pull request"
    );
    assert_eq!(
        decide(&RunTrigger {
            event: "pull_request_target".to_owned(),
            ..pull_request("o/app")
        }),
        Writes::Refused
    );
    assert_eq!(
        decide(&trigger("push", "feature", "o/app")),
        Writes::Refused,
        "a push to a branch that is not the default one"
    );
    assert_eq!(
        writes(
            "r1",
            Some("r1"),
            "o/app",
            &pull_request("o/app"),
            &main_only(),
            BuildCacheWriters::DefaultBranch,
        ),
        Writes::Refused,
        "a repository that did not opt in"
    );
}

#[test]
fn only_a_pull_request_a_person_opened_from_the_runs_branch_lets_it_write() {
    let run = pull_request("o/app");
    let pull = PullRequest {
        by_user: true,
        head_repository: Some("o/app".to_owned()),
        head_branch: "feature".to_owned(),
        base_branch: "main".to_owned(),
    };
    assert!(
        opened_by_user(&pull, "o/app", &run),
        "a person's pull request writes"
    );
    assert!(
        !opened_by_user(
            &PullRequest {
                by_user: false,
                ..pull.clone()
            },
            "o/app",
            &run
        ),
        "Renovate's pull request only reads"
    );
    assert!(
        !opened_by_user(
            &PullRequest {
                head_repository: Some("fork/app".to_owned()),
                ..pull.clone()
            },
            "o/app",
            &run
        ),
        "a fork's pull request only reads"
    );
    assert!(
        !opened_by_user(
            &PullRequest {
                head_branch: "other".to_owned(),
                ..pull
            },
            "o/app",
            &run
        ),
        "a pull request of another branch says nothing of this run"
    );
}

/// An entry's bytes that count for what they are: the least any entry counts for (`MIN_CHARGE`).
const ENTRY: usize = 4 << 10;

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
    let main = cache.attach("main", "o/app", ActionsCache::Off).unwrap();
    let pull = cache.attach("pull", "o/app", ActionsCache::Off).unwrap();
    cache.decide("main", Writes::Allowed, Scopes::default());
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
    let pull = cache.attach("pull", "o/app", ActionsCache::Off).unwrap();
    let main = cache.attach("main", "o/app", ActionsCache::Off).unwrap();
    cache.decide("pull", Writes::Refused, Scopes::default());
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
    let early = cache.attach("early", "o/app", ActionsCache::Off).unwrap();
    let pull = cache.attach("pull", "o/app", ActionsCache::Off).unwrap();
    let reader = cache.attach("reader", "o/app", ActionsCache::Off).unwrap();
    assert_eq!(request(&early, "PUT", "/gradle/aa", b"main").await.0, 200);
    assert_eq!(request(&pull, "PUT", "/gradle/bb", b"poison").await.0, 200);
    assert_eq!(
        request(&reader, "GET", "/gradle/aa", b"").await.0,
        404,
        "not before GitHub says the job may write"
    );
    cache.decide("early", Writes::Allowed, Scopes::default());
    cache.decide("pull", Writes::Refused, Scopes::default());
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
    let gone = cache.attach("gone", "o/app", ActionsCache::Off).unwrap();
    let reader = cache.attach("reader", "o/app", ActionsCache::Off).unwrap();
    assert_eq!(request(&gone, "PUT", "/gradle/aa", b"main").await.0, 200);
    assert_eq!(cache.detach("gone").unwrap().refused, 1);
    cache.decide("gone", Writes::Allowed, Scopes::default());
    assert_eq!(request(&reader, "GET", "/gradle/aa", b"").await.0, 404);
    assert_eq!(cache.size().entries, 0);
}

#[tokio::test]
async fn repositories_never_read_each_others_entries() {
    let Cache { cache, _dir } = open("repositories", 1 << 20);
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    let other = cache.attach("other", "o/other", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
    cache.decide("other", Writes::Allowed, Scopes::default());
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
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
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
        "/turbo/abc",
        "/turbo/v8/artifacts/",
        "/turbo/v8/artifacts/../../../gradle/abc",
        "/nx/abc",
        "/gradle/",
        "/abc",
    ] {
        assert_eq!(request(&app, "GET", path, b"").await.0, 404, "{path}");
        assert_eq!(request(&app, "PUT", path, b"x").await.0, 404, "{path}");
    }
    assert_eq!(cache.size().entries, 3);
}

#[tokio::test]
async fn turbo_and_nx_restore_what_main_built_and_never_what_a_pull_request_sent() {
    let Cache { cache, _dir } = open("turbo", 1 << 20);
    let main = cache.attach("main", "o/app", ActionsCache::Off).unwrap();
    let pull = cache.attach("pull", "o/app", ActionsCache::Off).unwrap();
    let reader = cache.attach("reader", "o/app", ActionsCache::Off).unwrap();
    cache.decide("main", Writes::Allowed, Scopes::default());
    cache.decide("pull", Writes::Refused, Scopes::default());
    // As turbo and Nx send them, turbo with its team in the query: main's task, and one only the pull request ran.
    let turbo = |hash: &str| format!("/turbo/v8/artifacts/{hash}?slug=runnerd");
    let nx = |hash: &str| format!("/nx/v1/cache/{hash}");
    for (built, pulled) in [
        (turbo("4f9a1c2b3d4e5f60"), turbo("70e5d4c3b2a1f90e")),
        (nx("1234567890"), nx("9876543210")),
    ] {
        assert_eq!(request(&main, "PUT", &built, b"outputs").await.0, 200);
        assert_eq!(request(&pull, "PUT", &built, b"poison").await.0, 200);
        assert_eq!(request(&pull, "PUT", &pulled, b"poison").await.0, 200);
        assert_eq!(
            request(&reader, "GET", &built, b"").await,
            (200, b"outputs".to_vec()),
            "{built}: main's, not the pull request's"
        );
        assert_eq!(request(&reader, "GET", &pulled, b"").await.0, 404);
    }
    assert_eq!(
        request(&reader, "HEAD", &turbo("4f9a1c2b3d4e5f60"), b"").await,
        (200, Vec::new())
    );
    let counts = cache.detach("reader").unwrap();
    assert_eq!((counts.hits, counts.misses, counts.read_bytes), (2, 2, 14));
    assert_eq!(cache.detach("pull").unwrap().refused, 4);
    assert_eq!(cache.detach("main").unwrap().written_bytes, 14);
}

#[tokio::test]
async fn turbo_finds_its_cache_enabled_and_a_dry_run_looks_up_many_hashes_at_once() {
    let Cache { cache, _dir } = open("turbo-api", 1 << 20);
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    let other = cache.attach("other", "o/other", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
    assert_eq!(
        request(&app, "PUT", "/turbo/v8/artifacts/aa11", b"dist")
            .await
            .0,
        200
    );
    let (code, body) = request(&app, "GET", "/turbo/v8/artifacts/status", b"").await;
    assert_eq!(code, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({ "status": "enabled" })
    );
    assert_eq!(
        request(&app, "POST", "/turbo/v8/artifacts/events", b"[{}]")
            .await
            .0,
        200
    );
    let query = br#"{"hashes":["aa11","bb22","../aa11"]}"#;
    for (dir, hit) in [
        (&app, serde_json::json!({ "taskDurationMs": 0 })),
        (&other, serde_json::Value::Null),
    ] {
        let (code, body) = request(dir, "POST", "/turbo/v8/artifacts?slug=runnerd", query).await;
        assert_eq!(code, 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({ "aa11": hit, "bb22": null, "../aa11": null }),
            "a repository finds only its own artifacts"
        );
    }
    // turbo then asks each hash with a HEAD.
    assert_eq!(
        request(&app, "POST", "/turbo/v8/artifacts", b"not json")
            .await
            .0,
        400
    );
    assert_eq!(
        request(&app, "PUT", "/turbo/v8/artifacts/status", b"x")
            .await
            .0,
        405
    );
    let counts = cache.detach("app").unwrap();
    assert_eq!(
        (counts.hits, counts.misses),
        (0, 0),
        "a dry run's lookups are not a build's"
    );
    assert_eq!(cache.size().entries, 1);
}

#[tokio::test]
async fn turbos_artifacts_keep_to_the_limits_and_outlive_a_restart() {
    let dir = TestDir::new("turbo-restart");
    let (entries, sockets) = (dir.0.join("entries"), dir.0.join("sockets"));
    let first = BuildCache::open(&entries, &sockets, 4 * ENTRY as u64).unwrap();
    let mounted = first.attach("app", "o/app", ActionsCache::Off).unwrap();
    first.decide("app", Writes::Allowed, Scopes::default());
    let path = "/turbo/v8/artifacts/aa11";
    assert_eq!(request(&mounted, "PUT", path, &[1; ENTRY]).await.0, 200);
    assert_eq!(
        request(&mounted, "PUT", "/turbo/v8/artifacts/bb22", &[0; ENTRY + 1])
            .await
            .0,
        200,
        "answered as stored"
    );
    drop(first);
    let second = BuildCache::open(&entries, &sockets, 4 * ENTRY as u64).unwrap();
    assert_eq!(
        second.attach("app", "o/app", ActionsCache::Off).unwrap(),
        mounted
    );
    assert_eq!(
        request(&mounted, "GET", path, b"").await,
        (200, vec![1; ENTRY])
    );
    assert_eq!(
        request(&mounted, "GET", "/turbo/v8/artifacts/bb22", b"")
            .await
            .0,
        404,
        "over a quarter of the limit"
    );
    let query = br#"{"hashes":["aa11"]}"#;
    assert_eq!(
        request(&mounted, "POST", "/turbo/v8/artifacts", query)
            .await
            .1,
        br#"{"aa11":{"taskDurationMs":0}}"#.to_vec()
    );
}

#[tokio::test]
async fn a_trim_drops_the_least_recently_used_entries_first() {
    let Cache { cache, _dir } = open("trim", 1 << 20);
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
    for key in ["old", "used", "new"] {
        assert_eq!(
            request(&app, "PUT", &format!("/gradle/{key}"), &[0; ENTRY])
                .await
                .0,
            200
        );
        // Entries are ordered by the millisecond they were last used.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(request(&app, "GET", "/gradle/used", b"").await.0, 200);
    cache.set_max_size(2 * ENTRY as u64);
    let trimmed = cache.trim();
    assert_eq!(trimmed.before.bytes, 3 * ENTRY as u64);
    assert_eq!(trimmed.after.bytes, 2 * ENTRY as u64);
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
    let mounted = first.attach("app", "o/app", ActionsCache::Off).unwrap();
    first.decide("app", Writes::Allowed, Scopes::default());
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
    assert_eq!(
        second.attach("app", "o/app", ActionsCache::Off).unwrap(),
        mounted
    );
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
    let dir = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
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
    let Cache { cache, _dir } = open("held-limit", 4 * ENTRY as u64);
    let mut forks = Vec::new();
    for fork in 0..5 {
        let name = format!("fork-{fork}");
        let socket = cache.attach(&name, "o/app", ActionsCache::Off).unwrap();
        assert_eq!(
            request(&socket, "PUT", "/gradle/a", &[0; ENTRY]).await.0,
            200
        );
        forks.push(name);
    }
    assert_eq!(waiting(&cache), 4, "the fifth upload was not kept on disk");
    for fork in &forks[..4] {
        cache.decide(fork, Writes::Refused, Scopes::default());
    }
    // The room the held uploads took is free again.
    let last = cache.attach("fork-4", "o/app", ActionsCache::Off).unwrap();
    assert_eq!(request(&last, "PUT", "/gradle/a", &[0; ENTRY]).await.0, 200);
    assert_eq!(waiting(&cache), 1);
    assert_eq!(cache.detach("fork-4").unwrap().refused, 1);
}

#[tokio::test]
async fn one_undecided_job_takes_at_most_a_quarter_of_the_uploads_room() {
    let Cache { cache, _dir } = open("held-share", 8 * ENTRY as u64);
    let fork = cache.attach("fork", "o/app", ActionsCache::Off).unwrap();
    for key in 0..8 {
        let path = format!("/gradle/{key}");
        assert_eq!(request(&fork, "PUT", &path, &[0; ENTRY]).await.0, 200);
    }
    assert_eq!(waiting(&cache), 2, "the fork kept a quarter of the room");
    // Other jobs GitHub has not answered for yet, main's among them, find the rest.
    for other in ["main", "nightly", "release"] {
        let socket = cache.attach(other, "o/app", ActionsCache::Off).unwrap();
        for key in ["a", "b"] {
            let path = format!("/gradle/{key}");
            assert_eq!(request(&socket, "PUT", &path, &[0; ENTRY]).await.0, 200);
        }
    }
    assert_eq!(waiting(&cache), 8, "every other job's uploads were kept");
    cache.decide("main", Writes::Allowed, Scopes::default());
    assert_eq!(cache.size().entries, 2);
    cache.decide("fork", Writes::Refused, Scopes::default());
    assert_eq!(cache.detach("fork").unwrap().refused, 8);
}

#[tokio::test]
async fn an_entry_over_a_quarter_of_the_limit_or_past_twice_it_is_refused() {
    let Cache { cache, _dir } = open("limits", 4 * ENTRY as u64);
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
    assert_eq!(
        request(&app, "PUT", "/gradle/big", &[0; ENTRY + 1]).await.0,
        200
    );
    assert_eq!(request(&app, "GET", "/gradle/big", b"").await.0, 404);
    for key in 0..8 {
        let path = format!("/gradle/{key}");
        assert_eq!(request(&app, "PUT", &path, &[0; ENTRY]).await.0, 200);
    }
    assert_eq!(cache.size().bytes, 8 * ENTRY as u64);
    assert_eq!(
        request(&app, "PUT", "/gradle/more", &[0; ENTRY]).await.0,
        200
    );
    assert_eq!(request(&app, "GET", "/gradle/more", b"").await.0, 404);
    let counts = cache.detach("app").unwrap();
    assert_eq!(
        (counts.refused, counts.written_bytes),
        (2, 8 * ENTRY as u64)
    );
}

#[tokio::test]
async fn an_entry_only_ever_sent_again_stays_in_use() {
    let Cache { cache, _dir } = open("sent-again", 1 << 20);
    let app = cache.attach("app", "o/app", ActionsCache::Off).unwrap();
    cache.decide("app", Writes::Allowed, Scopes::default());
    for key in ["blob", "old"] {
        assert_eq!(
            request(&app, "PUT", &format!("/bazel/cas/{key}"), &[0; ENTRY])
                .await
                .0,
            200
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Bazel sends the blob again with the next build's outputs, and never reads it.
    assert_eq!(
        request(&app, "PUT", "/bazel/cas/blob", &[0; ENTRY]).await.0,
        200
    );
    cache.set_max_size(ENTRY as u64);
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

#[test]
fn a_repository_named_in_other_case_than_githubs_still_writes() {
    // The config names `Acme/App`; GitHub spells it `acme/app`, and its API paths ignore the case.
    assert_eq!(
        writes(
            "r1",
            Some("r1"),
            "Acme/App",
            &trigger("push", "main", "acme/app"),
            &main_only(),
            BuildCacheWriters::DefaultBranch,
        ),
        Writes::Allowed
    );
    let pull = PullRequest {
        by_user: true,
        head_repository: Some("acme/app".to_owned()),
        head_branch: "feature".to_owned(),
        base_branch: "main".to_owned(),
    };
    assert!(
        opened_by_user(&pull, "Acme/App", &pull_request("acme/app")),
        "a person's pull request writes"
    );
}

#[tokio::test]
async fn an_adopted_runner_the_cache_is_off_for_leaves_no_socket_once_removed() {
    let dir = TestDir::new("adopted-off");
    let (entries, sockets) = (dir.0.join("entries"), dir.0.join("sockets"));
    let first = BuildCache::open(&entries, &sockets, 1 << 20).unwrap();
    let mounted = first.attach("app", "o/app", ActionsCache::Off).unwrap();
    drop(first);
    // After the restart the build cache is off, so the adopted runner is not attached again, only kept.
    let second = BuildCache::open(&entries, &sockets, 1 << 20).unwrap();
    second.forget_others(&["app"]);
    assert!(mounted.exists(), "its container still mounts it");
    assert_eq!(second.detach("app"), None, "it was not open");
    assert!(!mounted.exists(), "its directory goes with it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn uploads_of_one_key_landing_together_leave_the_size_what_the_entry_holds() {
    let Cache { cache, _dir } = open("same-key", 1 << 30);
    let runners: Vec<_> = (0..8)
        .map(|index| {
            let name = format!("main-{index}");
            let socket = cache.attach(&name, "o/app", ActionsCache::Off).unwrap();
            cache.decide(&name, Writes::Allowed, Scopes::default());
            socket
        })
        .collect();
    // Every slot builds the same task at once, each output its own size.
    let puts = (0..400).map(|upload: usize| {
        let socket = runners[upload % runners.len()].clone();
        tokio::spawn(async move {
            request(&socket, "PUT", "/gradle/same", &vec![b'x'; 1 + upload % 97]).await
        })
    });
    for put in futures_util::future::join_all(puts).await {
        assert_eq!(put.unwrap().0, 200);
    }
    let (status, body) = request(&runners[0], "GET", "/gradle/same", b"").await;
    assert_eq!(status, 200);
    assert_eq!(
        cache.size(),
        Size {
            bytes: body.len() as u64,
            entries: 1
        }
    );
}

/// The directories under `dir` that hold nothing.
fn empty_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut empty = Vec::new();
    let mut items = std::fs::read_dir(dir).unwrap().peekable();
    if items.peek().is_none() {
        empty.push(dir.to_owned());
    }
    for item in items {
        let path = item.unwrap().path();
        if path.is_dir() {
            empty.extend(empty_dirs(&path));
        }
    }
    empty
}

#[tokio::test]
async fn empty_uploads_count_against_the_limit_and_leave_no_directories_once_trimmed() {
    let Cache { cache, _dir } = open("empty", 8 * ENTRY as u64);
    let put_empty = async |socket: &Path, keys: std::ops::Range<usize>| {
        for key in keys {
            let path = format!("/sccache/{key}/b/c/k");
            assert_eq!(request(socket, "PUT", &path, b"").await.0, 200);
        }
    };
    let fork = cache.attach("fork", "o/app", ActionsCache::Off).unwrap();
    put_empty(&fork, 0..20).await;
    assert_eq!(
        waiting(&cache),
        2,
        "an undecided job holds a quarter of the room, however small its uploads"
    );
    let main = cache.attach("main", "o/app", ActionsCache::Off).unwrap();
    cache.decide("main", Writes::Allowed, Scopes::default());
    put_empty(&main, 0..40).await;
    assert_eq!(
        cache.size(),
        Size {
            bytes: 0,
            entries: 16
        },
        "entries stop at twice the limit"
    );
    assert_eq!(cache.detach("main").unwrap().refused, 24);
    assert_eq!(cache.trim().after.entries, 8);
    let tool = cache.directory().join("o/app/sccache");
    assert_eq!(empty_dirs(&tool), Vec::<PathBuf>::new());
    cache.clear();
    assert_eq!(empty_dirs(&tool), vec![tool]);
}

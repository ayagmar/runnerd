//! Records: no secret of a runner survives into one, the limits they are kept within, and what a shell in one shows.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::{banner, expired, failed, listed, make_room, secrets_left};
use crate::api::{self, RecordRemoval};
use crate::config::{ByteSize, Records};
use crate::testdir::TestDir;

const TOKEN: &str = "ghs_16C7e42F292c6912E7710c838347Ae178B4a";
const JIT_CONFIG: &str = "eyJydW5uZXIiOiJqaXQtY29uZmlnIn0=";

/// Every file a runner container may hold a secret in, with that secret.
const SECRETS: &[(&str, &str)] = &[
    ("run/runnerd/jitconfig", JIT_CONFIG),
    (
        "home/runner/.runner",
        "{\"agentId\": 7, \"gitHubUrl\": \"https://github.com/o/app\"}",
    ),
    (
        "home/runner/.credentials",
        "{\"scheme\": \"OAuth\", \"data\": {\"clientId\": \"1\"}}",
    ),
    (
        "home/runner/.credentials_rsaparams",
        "{\"d\": \"private-key\"}",
    ),
    ("home/runner/_work/_temp/git-credentials-1f2e.config", TOKEN),
    (
        "home/runner/_work/_temp/_runner_file_commands/set_env_1f2e",
        "DEPLOY_KEY=from-a-secret",
    ),
    (
        "home/runner/_work/_temp/.npmrc",
        "//npm.pkg.github.com/:_authToken=from-a-secret",
    ),
    (
        "home/runner/.docker/config.json",
        "{\"auths\": {\"ghcr.io\": {\"auth\": \"from-a-secret\"}}}",
    ),
    ("home/runner/.git-credentials", TOKEN),
    (
        "home/runner/.netrc",
        "machine github.com password from-a-secret",
    ),
    (
        "home/runner/.npmrc",
        "//registry.npmjs.org/:_authToken=from-a-secret",
    ),
    ("home/runner/.pypirc", "password = from-a-secret"),
    (
        "home/runner/.cargo/credentials.toml",
        "token = \"from-a-secret\"",
    ),
    (
        "home/runner/.ssh/id_ed25519",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
    ),
    ("home/runner/.config/gh/hosts.yml", TOKEN),
    ("home/runner/.kube/config", "token: from-a-secret"),
    // What a job's own steps and tools leave, which no list of names covers.
    (
        "home/runner/.gitconfig",
        "[url \"https://x-access-token:github_pat_11ABCDEFG@github.com/\"]\n\tinsteadOf = https://github.com/",
    ),
    (
        "home/runner/.aws/credentials",
        "[default]\naws_secret_access_key = from-a-secret",
    ),
    (
        "home/runner/.gnupg/private-keys-v1.d/0A1B.key",
        "(private-key from-a-secret)",
    ),
    ("home/runner/.config/gcloud/credentials.db", "from-a-secret"),
    ("home/runner/.terraformrc", "token = \"from-a-secret\""),
    (
        "home/runner/.cargo/config.toml",
        "[registries.internal]\ntoken = \"from-a-secret\"",
    ),
    ("tmp/deploy.pem", "-----BEGIN RSA PRIVATE KEY-----"),
    ("var/tmp/kubeconfig", "token: from-a-secret"),
];

/// What the image has before the runner starts, which stays.
const IMAGE: &[(&str, &str)] = &[
    ("home/runner/.bashrc", "# ~/.bashrc"),
    ("tmp/hsperfdata_root/12", "jvm"),
    ("run/docker.pid", "42"),
];

/// What the job made, which a record is for.
const WORKSPACE: &[(&str, &str)] = &[
    ("home/runner/_work/app/app/src/main.rs", "fn main() {}"),
    ("home/runner/_work/app/app/target/debug/app", "binary"),
    (
        "home/runner/_work/_actions/actions/checkout/v7/action.yml",
        "name: checkout",
    ),
    (
        "home/runner/_diag/pages/1_job_1.log",
        "##[group]Run cargo test",
    ),
];

/// The Rust toolchain a job installed, which a shell needs to re-run its steps.
const TOOLCHAIN: &[(&str, &str)] = &[
    (
        "home/runner/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc",
        "rustc",
    ),
    (
        "home/runner/.rustup/settings.toml",
        "default_toolchain = \"stable\"",
    ),
    ("home/runner/.cargo/bin/cargo", "cargo"),
    (
        "home/runner/.cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/serde-1.0.0.crate",
        "crate",
    ),
];

/// A repository's config as `actions/checkout` leaves it when the job ends before its post step: with the token.
const GIT_CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/o/app\n\
     [http \"https://github.com/\"]\n\textraheader = AUTHORIZATION: basic eC1hY2Nlc3MtdG9rZW46Z2hzXzE2Qzc=\n";

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The paths under `root` changed after `since`, as a container's changes list them: `/home/runner/...`.
fn changed(root: &Path, since: (i64, i64)) -> Vec<String> {
    use std::os::unix::fs::MetadataExt as _;
    let mut found = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if (metadata.ctime(), metadata.ctime_nsec()) > since {
                found.push(format!("/{}", path.strip_prefix(root).unwrap().display()));
            }
            if metadata.is_dir() {
                dirs.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Touches the marker the entrypoint writes before the runner starts, a moment after what is there already.
fn runner_starts(root: &Path) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt as _;
    std::thread::sleep(Duration::from_millis(20));
    write(root, "run/runnerd-since", "");
    let marker = std::fs::metadata(root.join("run/runnerd-since")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    (marker.ctime(), marker.ctime_nsec())
}

#[test]
fn a_record_keeps_no_secret_of_the_runner_or_the_job() {
    let dir = TestDir::new("scrub");
    let root = &dir.0;
    for (path, text) in IMAGE {
        write(root, path, text);
    }
    let started = runner_starts(root);
    for (path, text) in SECRETS.iter().chain(WORKSPACE).chain(TOOLCHAIN) {
        write(root, path, text);
    }
    write(root, "home/runner/_work/app/app/.git/config", GIT_CONFIG);
    write(
        root,
        "home/runner/_work/app/app/.git/modules/lib/config",
        GIT_CONFIG,
    );

    let before = changed(root, started);
    let caught = secrets_left(before.iter().map(String::as_str));
    for (secret, _) in SECRETS {
        assert!(
            caught.iter().any(|path| *path == format!("/{secret}")),
            "/{secret} is caught before a commit: {caught:?}"
        );
    }

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../image/scrub.sh");
    let status = Command::new("bash").arg(script).arg(root).status().unwrap();
    assert!(status.success(), "scrub.sh exits 0");

    let after = changed(root, started);
    assert_eq!(
        secrets_left(after.iter().map(String::as_str)),
        Vec::<String>::new()
    );
    for path in &after {
        let file = root.join(path.trim_start_matches('/'));
        if file.is_file() {
            let text = std::fs::read_to_string(&file).unwrap();
            for (_, secret) in SECRETS {
                assert!(!text.contains(secret), "{path} keeps a secret: {text}");
            }
            assert!(
                !text.contains("extraheader"),
                "{path} keeps the token: {text}"
            );
        }
    }
    for (path, text) in WORKSPACE.iter().chain(IMAGE).chain(TOOLCHAIN) {
        assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), *text);
    }
    let config =
        std::fs::read_to_string(root.join("home/runner/_work/app/app/.git/config")).unwrap();
    assert!(
        config.contains("url = https://github.com/o/app"),
        "{config}"
    );
}

#[test]
fn a_refused_record_names_the_first_paths_and_counts_the_rest() {
    let paths: Vec<String> = (1..=7).map(|n| format!("/tmp/{n}")).collect();
    assert_eq!(
        listed(&paths),
        "/tmp/1, /tmp/2, /tmp/3, /tmp/4, /tmp/5 and 2 more"
    );
    assert_eq!(listed(&paths[..2]), "/tmp/1, /tmp/2");
}

#[test]
fn a_file_where_a_kept_toolchain_directory_goes_is_caught() {
    assert_eq!(
        secrets_left(["/home/runner/.cargo", "/home/runner/.rustup"]),
        ["/home/runner/.cargo", "/home/runner/.rustup"]
    );
    assert_eq!(
        secrets_left([
            "/home/runner/.cargo",
            "/home/runner/.cargo/bin",
            "/home/runner/.cargo/bin/cargo",
            "/home/runner/.cargo/credentials.toml",
        ]),
        ["/home/runner/.cargo/credentials.toml"]
    );
}

#[test]
fn only_a_failed_job_is_recorded() {
    let log = "√ Connected to GitHub\n2026-10-23 01:08:55Z: Listening for Jobs\n\
               2026-10-23 01:08:55Z: Running job: build\n";
    assert!(
        failed(&format!(
            "{log}2026-10-23 01:09:27Z: Job build completed with result: Failed\n"
        )),
        "a failed job is recorded"
    );
    for result in ["Succeeded", "Canceled"] {
        let log = format!("{log}2026-10-23 01:09:27Z: Job build completed with result: {result}\n");
        assert!(!failed(&log), "a job that {result} is not recorded");
    }
    assert!(!failed(log), "a job that never completed is not recorded");
}

fn record(job: u64, created_at: u64, gib: u64) -> api::Record {
    api::Record {
        job,
        repository: "o/app".to_owned(),
        profile: "app".to_owned(),
        workflow: None,
        name: None,
        created_at,
        bytes: gib << 30,
        image: super::image(job),
    }
}

fn limits() -> Records {
    Records {
        max_total: ByteSize(10 << 30),
        max_count: 3,
        max_age: Duration::from_hours(24),
        min_free: ByteSize(5 << 30),
    }
}

const NOW: u64 = 100_000;
const PLENTY: Option<u64> = Some(1 << 40);

#[test]
fn a_new_record_past_max_count_removes_the_oldest() {
    let records = [
        record(3, NOW - 10, 1),
        record(1, NOW - 30, 1),
        record(2, NOW - 20, 1),
    ];
    let removed = make_room(&records, &BTreeSet::new(), &limits(), 1 << 30, PLENTY, NOW);
    assert_eq!(removed, Ok(vec![(1, RecordRemoval::Count)]));
}

#[test]
fn a_new_record_past_max_total_removes_the_oldest_until_it_fits() {
    let records = [record(1, NOW - 30, 4), record(2, NOW - 20, 4)];
    let removed = make_room(&records, &BTreeSet::new(), &limits(), 3 << 30, PLENTY, NOW);
    assert_eq!(removed, Ok(vec![(1, RecordRemoval::Size)]));
    assert_eq!(
        make_room(&records, &BTreeSet::new(), &limits(), 11 << 30, PLENTY, NOW).unwrap_err(),
        "it would take 11G, more than records.max_total (10G)"
    );
}

#[test]
fn records_past_max_age_go() {
    let records = [record(1, NOW - 90_000, 1), record(2, NOW - 20, 1)];
    assert_eq!(
        expired(&records, &BTreeSet::new(), &limits(), NOW),
        vec![(1, RecordRemoval::Age)]
    );
    assert_eq!(
        make_room(&records, &BTreeSet::new(), &limits(), 1 << 30, PLENTY, NOW),
        Ok(vec![(1, RecordRemoval::Age)])
    );
}

#[test]
fn no_record_leaves_docker_less_than_min_free() {
    let records = [record(1, NOW - 30, 1)];
    let refused = make_room(
        &records,
        &BTreeSet::new(),
        &limits(),
        2 << 30,
        Some(6 << 30),
        NOW,
    );
    assert_eq!(
        refused.unwrap_err(),
        "Docker has 6G free; a record of 2G would leave less than records.min_free (5G)"
    );
    assert_eq!(
        make_room(
            &records,
            &BTreeSet::new(),
            &limits(),
            1 << 30,
            Some(6 << 30),
            NOW
        ),
        Ok(Vec::new())
    );
}

#[test]
fn a_record_with_a_shell_open_stays() {
    let records = [
        record(1, NOW - 90_000, 1),
        record(2, NOW - 30, 1),
        record(3, NOW - 20, 1),
    ];
    let open = BTreeSet::from([1, 2]);
    assert_eq!(expired(&records, &open, &limits(), NOW), Vec::new());
    let removed = make_room(&records, &open, &limits(), 1 << 30, PLENTY, NOW);
    assert_eq!(removed, Ok(vec![(3, RecordRemoval::Count)]));
    let all_open = BTreeSet::from([1, 2, 3]);
    assert!(
        make_room(&records, &all_open, &limits(), 1 << 30, PLENTY, NOW).is_err(),
        "no record is made when every one in its way has a shell open"
    );
}

fn step(number: u32, name: &str, conclusion: &str) -> api::Step {
    api::Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: None,
        completed_at: None,
    }
}

fn failed_job() -> api::Job {
    let summary: api::JobSummary = serde_json::from_value(serde_json::json!({
        "id": 42, "repository": "o/app", "run_id": 7, "run_attempt": 1, "workflow": "CI", "name": "test",
        "branch": "main", "url": null, "labels": [], "state": "completed", "conclusion": "failure",
        "runner": null, "timeline": {"queued_at": 1}, "metrics": {"mirror_bytes": 0},
        "log": {"lines": 0, "bytes": 0, "truncated": false}
    }))
    .unwrap();
    api::Job {
        admission: None,
        summary,
        steps: vec![
            step(1, "Set up job", "success"),
            step(2, "Set up runner", "success"),
            step(3, "Run actions/checkout@v7", "success"),
            step(4, "Unit tests", "failure"),
            step(5, "Lint", "skipped"),
        ],
        record: None,
    }
}

fn log() -> Vec<String> {
    [
        "2026-10-23T01:08:56.1Z ##[group]GITHUB_TOKEN Permissions",
        "2026-10-23T01:08:56.1Z Contents: read",
        "2026-10-23T01:08:56.1Z ##[endgroup]",
        "2026-10-23T01:08:58.4Z ##[group]Run '/usr/local/bin/runnerd-job-started.sh'",
        "2026-10-23T01:08:58.4Z ##[endgroup]",
        "2026-10-23T01:08:58.5Z ##[group]Run actions/checkout@v7",
        "2026-10-23T01:08:58.5Z with:",
        "2026-10-23T01:08:58.5Z   fetch-depth: 1",
        "2026-10-23T01:08:58.5Z ##[endgroup]",
        "2026-10-23T01:08:59.0Z ##[group]Fetching the repository",
        "2026-10-23T01:08:59.0Z ##[endgroup]",
        "2026-10-23T01:09:25.7Z ##[group]Run cargo test --locked",
        "2026-10-23T01:09:25.7Z \u{1b}[36;1mcargo test --locked\u{1b}[0m",
        "2026-10-23T01:09:25.7Z shell: /usr/bin/bash -e {0}",
        "2026-10-23T01:09:25.7Z env:",
        "2026-10-23T01:09:25.7Z   RUSTFLAGS: -D warnings",
        "2026-10-23T01:09:25.7Z ##[endgroup]",
        "2026-10-23T01:09:30.0Z test tests::adds ... FAILED",
        "2026-10-23T01:09:30.1Z ##[error]Process completed with exit code 101.",
        "2026-10-23T01:09:30.2Z Post job cleanup.",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn a_shell_shows_the_failed_step_and_what_it_ran() {
    let text = banner(&failed_job(), &log(), None);
    assert!(
        text.contains("It failed at step 4: Unit tests (exit code 101)\r\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "    cargo test --locked\r\n    shell: /usr/bin/bash -e {0}\r\n    env:\r\n      RUSTFLAGS: -D warnings\r\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("You are in /home/runner/_work/app/app"),
        "{text}"
    );
    assert!(
        !text.contains('\u{1b}'),
        "colour codes are stripped: {text}"
    );
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "every line ends in CRLF: {text}"
    );
}

#[test]
fn a_shell_shows_another_step_when_asked() {
    let text = banner(&failed_job(), &log(), Some(3));
    assert!(
        text.contains("It shows step 3: Run actions/checkout@v7\r\n"),
        "{text}"
    );
    assert!(
        text.contains("    with:\r\n      fetch-depth: 1\r\n"),
        "{text}"
    );
    let text = banner(&failed_job(), &log(), Some(9));
    assert!(text.contains("The job has no such step."), "{text}");
}

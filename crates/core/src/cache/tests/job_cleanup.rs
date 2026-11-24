//! `image/job-cleanup.sh` and the clean-exit marker in `image/layers.sh` (ADR 0169), run by bash over a temporary
//! root, with stand-ins for `docker` and `curl` that log their calls and answer from files. Never the host's Docker.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::testdir::TestDir;

/// A container's root in a temporary directory: the scripts with their absolute paths moved under it, and `bin`,
/// first on `PATH`, with the stand-ins. What the Engine answers is in `fake`: `containers`, `volumes` and `df` (the
/// build cache, as `GET /system/df?type=build-cache` gives it), each missing for nothing, and `down` while it does not
/// answer at all.
struct Root {
    dir: TestDir,
}

const DOCKER: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE/calls"
[ ! -e "$FAKE/down" ] || exit 1
case "$1 $2" in
    "ps -aq") cat "$FAKE/containers" 2> /dev/null ;;
    "volume ls") cat "$FAKE/volumes" 2> /dev/null ;;
    "builder prune") echo "Total: 0B" ;;
esac
exit 0
"#;

const CURL: &str = r#"#!/bin/sh
[ ! -e "$FAKE/down" ] && cat "$FAKE/df" 2> /dev/null || exit 7
"#;

/// Counts the walks of the store, then runs the real `du`.
const DU: &str = r#"#!/bin/sh
echo >> "$FAKE/du"
PATH=${PATH#*:} exec du "$@"
"#;

impl Root {
    fn new(name: &str) -> Root {
        let dir = TestDir::new(name);
        let root = &dir.0;
        for path in ["state/docker", "run", "home/runner", "lib", "bin", "fake"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::write(root.join("state/docker/layer"), vec![0; 64 * 1024]).unwrap();
        std::fs::write(root.join("run/runnerd-docker-budget-gb"), "20").unwrap();
        let image = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../image");
        let moved = |script: &str| {
            let text = std::fs::read_to_string(image.join(script)).unwrap();
            text.replace(
                "/usr/local/lib/runnerd-layers.sh",
                &at(root, "lib/layers.sh"),
            )
            .replace("/state/", &at(root, "state/"))
            .replace("/run/", &at(root, "run/"))
            .replace("/home/runner/", &at(root, "home/runner/"))
        };
        std::fs::write(root.join("lib/layers.sh"), moved("layers.sh")).unwrap();
        std::fs::write(root.join("lib/job-cleanup.sh"), moved("job-cleanup.sh")).unwrap();
        for (name, text) in [("docker", DOCKER), ("curl", CURL), ("du", DU)] {
            use std::os::unix::fs::PermissionsExt as _;
            let path = root.join("bin").join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Root { dir }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir.0.join(path)
    }

    /// Sets what the Engine answers with in `fake/<name>`; `None` removes it.
    fn engine(&self, name: &str, answer: Option<&str>) {
        let path = self.path("fake").join(name);
        match answer {
            Some(answer) => std::fs::write(path, answer).unwrap(),
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn bash(&self, script: &str, args: &[&str]) -> std::process::Output {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("bash")
            .arg("-c")
            .arg(script)
            .arg("test")
            .args(args)
            .env("PATH", path)
            .env("FAKE", self.path("fake"))
            .output()
            .unwrap()
    }

    /// Runs the cleanup; returns the `docker` calls it made, and clears them.
    fn clean_up(&self) -> Vec<String> {
        let output = self.bash(". \"$1\"", &[&at(&self.dir.0, "lib/job-cleanup.sh")]);
        assert!(
            output.status.success(),
            "the cleanup exits 0: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(self.path("fake/calls")).unwrap_or_default();
        let _ = std::fs::remove_file(self.path("fake/calls"));
        calls.lines().map(str::to_owned).collect()
    }

    /// The `docker builder prune` calls among `calls`, by their filters.
    fn prunes(calls: &[String]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|call| call.strip_prefix("builder prune -af "))
            .map(str::to_owned)
            .collect()
    }

    /// Whether `layers.sh`'s function `check` succeeds.
    fn holds(&self, check: &str) -> bool {
        let layers = at(&self.dir.0, "lib/layers.sh");
        self.bash(&format!(". \"$1\"; {check}"), &[&layers])
            .status
            .success()
    }

    fn marker(&self) -> Option<String> {
        std::fs::read_to_string(self.path("state/.runnerd-cleaned")).ok()
    }
}

fn at(root: &Path, path: &str) -> String {
    format!("{}/{path}", root.display())
}

/// A build cache record as the Engine lists it.
fn record(typ: &str, size: u64, shared: bool) -> String {
    format!(r#"{{"ID":"x","Type":"{typ}","Size":{size},"Shared":{shared},"InUse":false}}"#)
}

fn df(records: &[String]) -> String {
    format!(r#"{{"BuildCache":[{}]}}"#, records.join(","))
}

const GIB: u64 = 1 << 30;

// Catches: a cleanup whose Engine did not answer marking the slot clean, so the next container skips the cleanup that
// would remove the last job's containers; a finished one not marking it, so every container cleans up twice.
#[test]
fn a_cleanup_that_ends_marks_the_slot_clean_with_its_budget_and_one_the_engine_failed_does_not() {
    let root = Root::new("cleanup-marker");
    root.engine("df", Some(&df(&[])));
    root.clean_up();
    assert_eq!(root.marker().as_deref(), Some("20\n"));

    std::fs::remove_file(root.path("state/.runnerd-cleaned")).unwrap();
    root.engine("down", Some(""));
    root.clean_up();
    assert_eq!(
        root.marker(),
        None,
        "a cleanup the Engine failed leaves no marker"
    );
}

// Catches: the next container skipping its cleanup after an unclean stop, after a marker a job forged with something
// else than a budget, or when its budget is lower than the one the store was kept within.
#[test]
fn the_next_container_skips_its_cleanup_only_after_a_clean_end_within_a_budget_no_larger() {
    let root = Root::new("cleanup-skip");
    assert!(!root.holds("cleaned_up"), "no marker: an unclean end");
    std::fs::write(root.path("state/.runnerd-cleaned"), "20\n").unwrap();
    assert!(root.holds("cleaned_up"), "the same budget");
    std::fs::write(root.path("run/runnerd-docker-budget-gb"), "30").unwrap();
    assert!(root.holds("cleaned_up"), "a larger budget");
    std::fs::write(root.path("run/runnerd-docker-budget-gb"), "10").unwrap();
    assert!(!root.holds("cleaned_up"), "a lower budget cleans up first");
    std::fs::write(root.path("run/runnerd-docker-budget-gb"), "20").unwrap();
    for forged in ["", "x", "-1", "20 30"] {
        std::fs::write(root.path("state/.runnerd-cleaned"), forged).unwrap();
        assert!(!root.holds("cleaned_up"), "{forged:?} is no budget");
    }
}

// Catches: a container or volume that a job's background process made after its cleanup, or that a killed container
// of an older image left under a marker it never removes, reaching the slot's next job, which would skip its
// cleanup; an Engine that does not answer read as holding nothing.
#[test]
fn the_next_container_finds_the_store_clean_only_while_the_engine_holds_no_container_or_volume() {
    let root = Root::new("cleanup-left");
    assert!(root.holds("store_clean"), "nothing held");
    root.engine("containers", Some("4f2a\n"));
    assert!(!root.holds("store_clean"), "a container left");
    root.engine("containers", None);
    root.engine("volumes", Some("pgdata\n"));
    assert!(!root.holds("store_clean"), "a volume left");
    root.engine("volumes", None);
    root.engine("down", Some(""));
    assert!(!root.holds("store_clean"), "an Engine that does not answer");
}

// Catches: a prune skipped while the cache holds what it removes, as build contexts or a cache over half the budget;
// prunes skipped when the Engine does not list its cache; every prune run, and the store walked twice, for a job that
// built nothing.
#[test]
fn the_build_cache_is_pruned_of_what_it_holds_and_of_everything_when_the_engine_does_not_say() {
    let root = Root::new("cleanup-prunes");
    let half = "--filter type=regular --filter until=";
    // The job built nothing: the store is walked once, for its report.
    std::fs::write(root.path("run/runnerd-since"), "").unwrap();
    root.engine("df", Some(&df(&[record("regular", GIB, false)])));
    assert_eq!(Root::prunes(&root.clean_up()), Vec::<String>::new());
    let walks = std::fs::read_to_string(root.path("fake/du")).unwrap();
    assert_eq!(walks.lines().count(), 1, "the store is walked once");
    let report = std::fs::read_to_string(root.path("run/runnerd-docker-store")).unwrap();
    assert!(
        report.starts_with("held=") && report.ends_with(" share=20\n"),
        "{report}"
    );

    // Build contexts, and over half the 20 GiB budget in what no image shares; a shared record weighs nothing.
    root.engine(
        "df",
        Some(&df(&[
            record("source.local", 4096, false),
            record("regular", 9 * GIB, false),
            record("regular", 2 * GIB, false),
            record("regular", 40 * GIB, true),
        ])),
    );
    let prunes = Root::prunes(&root.clean_up());
    assert_eq!(prunes.len(), 2, "{prunes:?}");
    assert_eq!(prunes[0], "--filter type=source.local");
    assert!(prunes[1].starts_with(half), "{prunes:?}");

    root.engine(
        "df",
        Some(&df(&[
            record("regular", 9 * GIB, false),
            record("regular", 40 * GIB, true),
        ])),
    );
    assert_eq!(
        Root::prunes(&root.clean_up()),
        Vec::<String>::new(),
        "under half the budget, with what images share left out"
    );

    // An Engine that does not list its cache gets every prune, as before.
    root.engine("df", None);
    let prunes = Root::prunes(&root.clean_up());
    assert_eq!(prunes.len(), 3, "{prunes:?}");
    assert_eq!(prunes[0], "--filter type=source.local");
    assert_eq!(prunes[1], "--filter type=source.git.checkout");
    assert!(prunes[2].starts_with(half), "{prunes:?}");
}

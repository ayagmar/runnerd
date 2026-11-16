//! The mirror's links as `mirror_links.sh` reads them on a registry's storage laid out in a temporary directory, and
//! the doctor's check of what it found.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a file they cannot write or a script that does not run fails them"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::{DESCRIPTOR_CACHE_ENV, Dangling, Fault, Found, LINKS_SCRIPT, Links, mirror_check};
use crate::api::CheckStatus;
use crate::cache::Names;
use crate::testdir::TestDir;

/// A registry's storage, as `/var/lib/registry` holds it.
struct Storage(TestDir);

impl Storage {
    fn new() -> Storage {
        Storage(TestDir::new("mirror-links"))
    }

    fn v2(&self) -> PathBuf {
        self.0.0.join("docker/registry/v2")
    }

    /// The data file of the blob `hex`.
    fn data(&self, hex: &str) -> PathBuf {
        self.v2()
            .join(format!("blobs/sha256/{}/{hex}/data", &hex[..2]))
    }

    /// Stores a blob of `bytes` bytes under `hex`.
    fn blob(&self, hex: &str, bytes: usize) {
        let data = self.data(hex);
        std::fs::create_dir_all(data.parent().unwrap()).unwrap();
        std::fs::write(data, vec![b'x'; bytes]).unwrap();
    }

    /// Writes a link at `path` under the storage's `v2` that holds `target`, as the registry writes one: no newline.
    fn link(&self, path: &str, target: &str) -> PathBuf {
        let link = self.v2().join(path);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, target).unwrap();
        link
    }

    /// A layer link of `repository` to `hex`.
    fn layer(&self, repository: &str, hex: &str) -> PathBuf {
        self.link(
            &format!("repositories/{repository}/_layers/sha256/{hex}/link"),
            &format!("sha256:{hex}"),
        )
    }

    /// Runs the script in `mode` on it; what it printed.
    fn run(&self, mode: &str) -> String {
        run(mode, &self.0.0)
    }
}

/// Runs the script in `mode` on the storage at `root` with `sh`, as the image's busybox runs it; what it printed.
fn run(mode: &str, root: &Path) -> String {
    let output = Command::new("sh")
        .arg("-c")
        .arg(LINKS_SCRIPT)
        .arg("mirror-links")
        .arg(mode)
        .arg(root)
        .env_remove(DESCRIPTOR_CACHE_ENV)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the script exits 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn hex(n: usize) -> String {
    format!("{n:064x}")
}

#[test]
fn a_repair_removes_the_links_whose_blob_is_gone_or_empty_and_keeps_the_rest() {
    let storage = Storage::new();
    let (stored, gone, empty) = (hex(1), hex(2), hex(3));
    storage.blob(&stored, 10);
    storage.blob(&empty, 0);
    let kept = storage.layer("library/caddy", &stored);
    let shared = storage.layer("library/golang", &stored);
    let lost = storage.layer("library/caddy", &gone);
    let hollow = storage.layer("redocly/cli", &empty);
    let revision = storage.link(
        &format!("repositories/library/node/_manifests/revisions/sha256/{gone}/link"),
        &format!("sha256:{gone}"),
    );
    let tag = storage.link(
        "repositories/library/node/_manifests/tags/22/current/link",
        &format!("sha256:{gone}"),
    );
    let garbage = storage.link(
        &format!("repositories/library/node/_layers/sha256/{stored}/link"),
        "sha256:../../x",
    );
    // The digest of no bytes, whose data is rightly empty.
    let nothing = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    storage.blob(nothing, 0);
    let no_bytes = storage.layer("library/alpine", nothing);

    let links = Links::parse(&storage.run("repair")).unwrap();

    assert_eq!(links.count, 8);
    let mut removed: Vec<String> = links.dangling.iter().map(ToString::to_string).collect();
    removed.sort();
    assert_eq!(
        removed,
        [
            format!("library/caddy@sha256:{gone}"),
            format!("library/node (repositories/library/node/_layers/sha256/{stored}/link)"),
            format!("library/node@sha256:{gone}"),
            format!("library/node@sha256:{gone}"),
            format!("redocly/cli@sha256:{empty} (empty)"),
        ]
    );
    for link in [&lost, &hollow, &revision, &tag, &garbage] {
        assert!(!link.exists(), "{} is removed", link.display());
    }
    assert!(kept.exists(), "a link to a stored blob stays");
    assert!(shared.exists(), "another repository's link to it stays");
    assert!(
        !storage.data(&empty).exists(),
        "an empty blob's data goes, which a fetch would otherwise keep"
    );
    assert!(storage.data(&stored).exists(), "a stored blob's data stays");
    assert!(
        no_bytes.exists() && storage.data(nothing).exists(),
        "the blob of no bytes is rightly empty and stays"
    );
    assert_eq!(
        Links::parse(&storage.run("repair")).unwrap().dangling,
        [],
        "a second repair finds nothing"
    );
}

#[test]
fn a_check_lists_the_links_whose_blob_is_gone_without_removing_them() {
    let storage = Storage::new();
    let lost = storage.layer("library/caddy", &hex(9));
    let output = storage.run("check");
    assert!(
        output.lines().any(|line| line == "cache image"),
        "it says the image's cache is kept: {output}"
    );
    assert_eq!(
        Links::parse(&output).unwrap().dangling,
        [Dangling {
            fault: Fault::Gone,
            repository: "library/caddy".to_owned(),
            digest: Some(format!("sha256:{}", hex(9))),
            link: format!("repositories/library/caddy/_layers/sha256/{}/link", hex(9)),
        }]
    );
    assert!(lost.exists(), "a check removes nothing");
}

#[test]
fn a_new_an_empty_or_a_foreign_layout_volume_has_no_links_and_nothing_is_touched() {
    let missing = TestDir::new("mirror-missing");
    let new = missing.0.join("never-made");
    let empty = TestDir::new("mirror-empty");
    // Another registry's layout: a link-like file, but not under `docker/registry/v2/repositories`.
    let other = TestDir::new("mirror-other");
    let foreign = other
        .0
        .join("v3/repositories/library/caddy/_layers/sha256/aa/link");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, "sha256:aa").unwrap();
    for root in [&new, &empty.0, &other.0] {
        let links = Links::parse(&run("repair", root)).unwrap();
        assert_eq!(
            links,
            Links {
                count: 0,
                dangling: Vec::new()
            },
            "{}",
            root.display()
        );
    }
    assert!(foreign.exists(), "another layout's files stay");
    assert!(!new.exists(), "a missing volume is not made");
}

#[test]
fn thousands_of_links_are_repaired_within_seconds() {
    let storage = Storage::new();
    for n in 0..5000 {
        storage.layer(&format!("library/repo{}", n % 40), &hex(n));
        if n % 1000 != 7 {
            storage.blob(&hex(n), 1);
        }
    }
    let started = Instant::now();
    let links = Links::parse(&storage.run("repair")).unwrap();
    let took = started.elapsed();
    assert_eq!(links.count, 5000);
    assert_eq!(links.dangling.len(), 5);
    // About 0.3 s on a desktop, and in the image's busybox.
    assert!(
        took < Duration::from_secs(10),
        "5000 links take {took:?}, under the repair's deadline"
    );
}

#[test]
fn output_cut_off_before_its_count_reads_as_unfinished() {
    let cut = format!(
        "gone sha256:{} repositories/a/_layers/sha256/x/link\n",
        hex(1)
    );
    assert_eq!(Links::parse(&cut), None);
    assert_eq!(Links::parse(""), None);
}

fn found(faults: &[Fault], cached: bool) -> Found {
    Found::Read {
        links: Links {
            count: 499,
            dangling: faults
                .iter()
                .enumerate()
                .map(|(n, fault)| Dangling {
                    fault: *fault,
                    repository: "library/caddy".to_owned(),
                    digest: Some(format!("sha256:{}", hex(n))),
                    link: format!("repositories/library/caddy/_layers/sha256/{}/link", hex(n)),
                })
                .collect(),
        },
        cached,
    }
}

#[test]
fn a_missing_blob_fails_the_doctor_while_the_mirror_keeps_its_cache_and_the_fix_removes_and_restarts()
 {
    let names = Names::new("runnerd");
    let check = mirror_check(&names, &found(&[Fault::Gone; 5], true));
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(
        check.detail.starts_with(&format!(
            "5 of its 499 links answer a pull with no data: library/caddy@sha256:{}, ",
            hex(0)
        )) && check.detail.contains("2 more")
            && check.detail.contains("got 0"),
        "{}",
        check.detail
    );
    let fix = check.fix.unwrap();
    assert!(
        fix.contains(&format!(
            "docker exec runnerd-mirror rm -f \
             /var/lib/registry/docker/registry/v2/repositories/library/caddy/_layers/sha256/{}/link ",
            hex(0)
        )) && fix.ends_with("docker restart runnerd-mirror   # empties the blob descriptor cache"),
        "the fix removes every such link, then empties the cache: {fix}"
    );
}

#[test]
fn an_empty_blob_fails_the_doctor_without_the_cache_and_its_fix_removes_the_data_too() {
    let names = Names::new("runnerd");
    let check = mirror_check(&names, &found(&[Fault::Gone, Fault::Empty], false));
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(
        check.detail.starts_with(&format!(
            "1 of its 499 links answer a pull with no data: library/caddy@sha256:{} (empty);",
            hex(1)
        )),
        "only the empty blob serves no data: {}",
        check.detail
    );
    let fix = check.fix.unwrap();
    let data = format!(
        "/var/lib/registry/docker/registry/v2/blobs/sha256/00/{}/data",
        hex(1)
    );
    assert!(
        fix.contains(&data) && fix.contains(&hex(0)) && !fix.contains("restart"),
        "it removes both links and the empty data, and needs no restart: {fix}"
    );
}

#[test]
fn the_doctor_notes_a_missing_blob_without_the_cache_and_warns_of_the_cache_alone() {
    let names = Names::new("runnerd");
    let gone = mirror_check(&names, &found(&[Fault::Gone], false));
    assert_eq!(gone.status, CheckStatus::Info);
    assert_eq!(gone.fix, None);
    assert_eq!(
        mirror_check(&names, &found(&[Fault::Garbled], false)).status,
        CheckStatus::Info
    );
    assert_eq!(
        mirror_check(&names, &found(&[], true)).status,
        CheckStatus::Warn,
        "a mirror an older runnerd started still keeps the cache"
    );
    assert_eq!(
        mirror_check(&names, &found(&[], false)).status,
        CheckStatus::Pass
    );
    assert_eq!(
        mirror_check(&names, &Found::NotRunning).status,
        CheckStatus::Info
    );
}

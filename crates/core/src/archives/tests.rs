//! The action archives on a fake GitHub and a temporary directory (ADR 0147): a cold store fills with every public
//! action the served repositories pin, by commit, and what those pin in turn; a warm one fetches nothing; an archive
//! no repository pins stays for `keep` and then goes, while a pinned one never does; a corrupt download never
//! reaches `archives/`; an archive changed on disk is quarantined and fetched again; a pin change keeps both
//! commits; a refresh killed midway leaves nothing a runner reads and recovers at the next start; a tree cut short or
//! a failed read keeps the last pins; a private action is never fetched. Then the same over generated sequences of
//! refreshes, restarts, failed downloads, pin changes and damage.
//!
//! Archives are built here as `git archive` writes them: a PAX global header whose `comment` is the commit, then one
//! top directory.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a fixture that cannot be made fails the test"
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use tar::{EntryType, Header};

use super::store::ArchiveStore;
use super::verify::{self, Rejected};
use super::{ActionRepository, Archives, Settings, Source, Tree, TreeFile};
use crate::api::ArchiveFailureKind;
use crate::github::Error;
use crate::testdir::TestDir;

const SERVED: &str = "o/app";
const CHECKOUT: &str = "08eba0b27e820071cde6df949e0beb9ba4906955";
const CHECKOUT_NEXT: &str = "11bd71901bbe5b1630ceea73d27597364c9af683";
const NODE: &str = "49933ea5288caeca8642d1e84afbd3f7d6820020";
const OAUTH: &str = "6cae46e2d796f1eaa3ff1a9a7d4b3d2c6b4bd8ad";
const MISE: &str = "9f1b3d2c6b4bd8ad6cae46e2d796f1eaa3ff1a9a";
const SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DAY: u64 = 86_400;
const KEEP: Duration = Duration::from_secs(14 * DAY);
const T0: u64 = 1_790_000_000;

/// An entry of a test archive.
enum Item<'item> {
    Dir(&'item str),
    File(&'item str, &'item str),
    Symlink(&'item str, &'item str),
    HardLink(&'item str, &'item str),
    Fifo(&'item str),
}

/// A header whose name is written raw, so a test can write the names `tar` refuses to.
fn header(kind: EntryType, name: &str, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    let bytes = name.as_bytes();
    header.as_gnu_mut().unwrap().name[..bytes.len()].copy_from_slice(bytes);
    header.set_mode(if kind == EntryType::Directory {
        0o755
    } else {
        0o644
    });
    header.set_size(size);
    header.set_mtime(1_700_000_000);
    header
}

/// The tar `git archive` writes for `comment` (a commit), with `items`.
fn tar_of(comment: &str, items: &[Item<'_>]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let record = format!(" comment={comment}\n");
    let length = record.len() + 2;
    let record = format!("{length}{record}");
    let mut global = header(
        EntryType::XGlobalHeader,
        "pax_global_header",
        record.len() as u64,
    );
    global.set_cksum();
    builder.append(&global, record.as_bytes()).unwrap();
    for item in items {
        match item {
            Item::Dir(name) => {
                let mut entry = header(EntryType::Directory, name, 0);
                entry.set_cksum();
                builder.append(&entry, &[][..]).unwrap();
            }
            Item::File(name, text) => {
                let mut entry = header(EntryType::Regular, name, text.len() as u64);
                entry.set_cksum();
                builder.append(&entry, text.as_bytes()).unwrap();
            }
            Item::Symlink(name, target) | Item::HardLink(name, target) => {
                let kind = if matches!(item, Item::Symlink(..)) {
                    EntryType::Symlink
                } else {
                    EntryType::Link
                };
                let mut entry = header(kind, name, 0);
                let bytes = target.as_bytes();
                entry.as_gnu_mut().unwrap().linkname[..bytes.len()].copy_from_slice(bytes);
                entry.set_cksum();
                builder.append(&entry, &[][..]).unwrap();
            }
            Item::Fifo(name) => {
                let mut entry = header(EntryType::Fifo, name, 0);
                entry.set_cksum();
                builder.append(&entry, &[][..]).unwrap();
            }
        }
    }
    builder.into_inner().unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// A well-formed archive of `sha`, with `action` as its root `action.yml`.
fn archive(sha: &str, action: &str) -> Vec<u8> {
    let top = format!("o-r-{}/", &sha[..7]);
    let file = format!("{top}action.yml");
    let link = format!("{top}dist/main.js");
    let readme = format!("{top}README.md");
    gzip(&tar_of(
        sha,
        &[
            Item::Dir(&top),
            Item::File(&file, action),
            Item::File(&readme, "an action"),
            Item::Symlink(&link, "../action.yml"),
        ],
    ))
}

/// A repository's tree as GitHub lists it, with its files and their text.
#[derive(Debug, Clone, Default)]
struct Files {
    truncated: bool,
    files: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct Fake {
    /// By name as asked, lower case: what GitHub's repository API answers.
    repositories: HashMap<String, ActionRepository>,
    /// The served repository's default branch head.
    head: String,
    /// By commit of the served repository.
    trees: HashMap<String, Files>,
    /// By `owner/repo@sha`, canonical: what the tarball endpoint sends.
    tarballs: HashMap<String, Vec<u8>>,
    /// Downloads asked for, `owner/repo@sha`, in order.
    downloads: Vec<String>,
    /// Trees read.
    trees_read: u32,
    /// Downloads that fail, by `owner/repo@sha`.
    failing: BTreeSet<String>,
    /// Whether every read fails, as GitHub down.
    down: bool,
}

/// A fake GitHub; clones share it.
#[derive(Debug, Clone, Default)]
struct FakeGithub(Arc<Mutex<Fake>>);

fn unreachable() -> Error {
    Error::Download("GitHub did not answer".to_owned())
}

impl Source for FakeGithub {
    async fn action_repository(&self, repository: &str) -> Result<ActionRepository, Error> {
        let fake = self.0.lock().unwrap();
        if fake.down {
            return Err(unreachable());
        }
        fake.repositories
            .get(&repository.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| Error::Status {
                method: reqwest::Method::GET,
                path: format!("/repos/{repository}"),
                status: reqwest::StatusCode::NOT_FOUND,
                message: "Not Found".to_owned(),
            })
    }

    async fn branch_head(&self, _: &str, _: &str) -> Result<String, Error> {
        let fake = self.0.lock().unwrap();
        if fake.down {
            return Err(unreachable());
        }
        Ok(fake.head.clone())
    }

    async fn tree(&self, _: &str, commit: &str) -> Result<Tree, Error> {
        let mut fake = self.0.lock().unwrap();
        if fake.down {
            return Err(unreachable());
        }
        fake.trees_read += 1;
        let files = fake.trees.get(commit).cloned().unwrap_or_default();
        Ok(Tree {
            truncated: files.truncated,
            files: files
                .files
                .iter()
                .map(|(path, text)| TreeFile {
                    path: path.clone(),
                    blob: format!("{commit}:{path}"),
                    size: text.len() as u64,
                })
                .collect(),
        })
    }

    async fn blob(&self, _: &str, blob: &str, _: u64) -> Result<Vec<u8>, Error> {
        let fake = self.0.lock().unwrap();
        let (commit, path) = blob.split_once(':').unwrap();
        Ok(fake.trees[commit].files[path].clone().into_bytes())
    }

    async fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
    ) -> Result<u64, Error> {
        let label = format!("{repository}@{commit}");
        let bytes = {
            let mut fake = self.0.lock().unwrap();
            fake.downloads.push(label.clone());
            if fake.down || fake.failing.contains(&label) {
                return Err(unreachable());
            }
            fake.tarballs.get(&label).cloned().unwrap()
        };
        assert!(bytes.len() as u64 <= limit);
        fs::write(to, &bytes).unwrap();
        Ok(bytes.len() as u64)
    }
}

impl FakeGithub {
    /// GitHub with `o/app` served at `head`, whose workflow pins `pins` (`uses:` values), and the actions this file
    /// knows: actions/checkout at two commits, actions/setup-node, a path action of tailscale's whose `action.yml`
    /// pins actions/checkout in turn, and a private action.
    fn new(head: &str, pins: &[&str]) -> FakeGithub {
        let github = FakeGithub::default();
        {
            let mut fake = github.0.lock().unwrap();
            let mut repository = |name: &str, public: bool| {
                fake.repositories.insert(
                    name.to_ascii_lowercase(),
                    ActionRepository {
                        full_name: name.to_owned(),
                        public,
                        default_branch: "main".to_owned(),
                    },
                );
            };
            repository(SERVED, false);
            repository("actions/checkout", true);
            repository("actions/setup-node", true);
            repository("Tailscale/github-action", true);
            repository("jdx/mise-action", true);
            repository("o/secret-action", false);
            let plain = "runs:\n  using: node20\n  main: dist/main.js\n";
            let nested = format!(
                "runs:\n  using: composite\n  steps:\n    - uses: actions/checkout@{CHECKOUT_NEXT}\n"
            );
            let path_action = gzip(&tar_of(
                OAUTH,
                &[
                    Item::Dir("tailscale-github-action-6cae46e/"),
                    Item::File("tailscale-github-action-6cae46e/action.yml", plain),
                    Item::Dir("tailscale-github-action-6cae46e/oauth/"),
                    Item::File("tailscale-github-action-6cae46e/oauth/action.yml", &nested),
                ],
            ));
            for (label, bytes) in [
                (
                    format!("actions/checkout@{CHECKOUT}"),
                    archive(CHECKOUT, plain),
                ),
                (
                    format!("actions/checkout@{CHECKOUT_NEXT}"),
                    archive(CHECKOUT_NEXT, plain),
                ),
                (format!("actions/setup-node@{NODE}"), archive(NODE, plain)),
                (format!("Tailscale/github-action@{OAUTH}"), path_action),
                (format!("jdx/mise-action@{MISE}"), archive(MISE, plain)),
                (format!("o/secret-action@{SECRET}"), archive(SECRET, plain)),
            ] {
                fake.tarballs.insert(label, bytes);
            }
        }
        github.push(head, pins);
        github
    }

    /// Moves `o/app`'s default branch to `head`, whose workflow pins `pins`.
    fn push(&self, head: &str, pins: &[&str]) {
        let workflow = pins.iter().fold(String::new(), |mut out, pin| {
            out.push_str("      - uses: ");
            out.push_str(pin);
            out.push('\n');
            out
        });
        let mut fake = self.0.lock().unwrap();
        fake.head = head.to_owned();
        let mut files = Files::default();
        files.files.insert(
            ".github/workflows/ci.yml".to_owned(),
            format!("jobs:\n  test:\n    steps:\n{workflow}"),
        );
        fake.trees.insert(head.to_owned(), files);
    }

    fn downloads(&self) -> Vec<String> {
        self.0.lock().unwrap().downloads.clone()
    }

    fn set_tarball(&self, label: &str, bytes: Vec<u8>) {
        self.0
            .lock()
            .unwrap()
            .tarballs
            .insert(label.to_owned(), bytes);
    }
}

fn settings() -> Settings {
    Settings {
        enabled: true,
        keep: KEEP,
        repositories: vec![SERVED.to_owned()],
    }
}

fn open(dir: &Path) -> Archives {
    Archives::new(ArchiveStore::open(dir).unwrap())
}

/// The archives under `archives/`, as `<dir>/<file>`.
fn on_disk(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for dir in fs::read_dir(root.join("archives")).unwrap() {
        let dir = dir.unwrap();
        for file in fs::read_dir(dir.path()).unwrap() {
            found.insert(format!(
                "{}/{}",
                dir.file_name().to_string_lossy(),
                file.unwrap().file_name().to_string_lossy()
            ));
        }
    }
    found
}

fn file_of(repository: &str, sha: &str) -> String {
    format!("{}/{sha}.tar.gz", repository.replace('/', "_"))
}

fn count(root: &Path, dir: &str) -> usize {
    fs::read_dir(root.join(dir)).unwrap().count()
}

fn pin(repository: &str, sha: &str) -> String {
    format!("{repository}@{sha}")
}

#[tokio::test(start_paused = true)]
async fn a_cold_store_fills_with_every_public_pinned_commit_and_what_those_pin_in_turn() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &format!("\"{}\" # v4", pin("actions/setup-node", NODE)),
            &pin("tailscale/github-action/oauth", OAUTH),
            &pin("o/secret-action", SECRET),
            "actions/cache@v4",
            "./.github/actions/mise",
        ],
    );
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;

    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([
            file_of("actions/checkout", CHECKOUT),
            file_of("actions/setup-node", NODE),
            // Named as GitHub names it, case and all, as the runner looks it up.
            file_of("Tailscale/github-action", OAUTH),
            // Pinned by the path action's own action.yml.
            file_of("actions/checkout", CHECKOUT_NEXT),
        ])
    );
    let downloads = github.downloads();
    assert!(
        !downloads.iter().any(|label| label.contains("secret")),
        "a private action is never fetched with runnerd's token: {downloads:?}"
    );
    let file = dir
        .0
        .join("archives")
        .join(file_of("actions/checkout", CHECKOUT));
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "every runner reads it");
    let status = archives.status();
    assert_eq!(status.entries, 4);
    assert_eq!(status.refreshed_at, Some(T0));
    assert!(status.failures.is_empty(), "{:?}", status.failures);
    assert_eq!(count(&dir.0, "staging"), 0, "nothing left half done");
}

#[tokio::test(start_paused = true)]
async fn a_warm_store_downloads_nothing_and_a_restart_does_not_either() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(github.downloads().len(), 1);
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(
        github.0.lock().unwrap().trees_read,
        1,
        "an unchanged head reads no tree"
    );
    drop(archives);
    let mut restarted = open(&dir.0);
    restarted.refresh(&github, &settings(), T0 + 1200).await;
    assert_eq!(github.downloads().len(), 1, "the index survives a restart");
    assert_eq!(restarted.status().entries, 1);
}

#[tokio::test(start_paused = true)]
async fn an_archive_no_longer_pinned_stays_for_keep_then_goes_and_a_pinned_one_never_does() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
        ],
    );
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    // A pin change: checkout moves to its next commit.
    github.push(
        "c2",
        &[
            &pin("actions/checkout", CHECKOUT_NEXT),
            &pin("actions/setup-node", NODE),
        ],
    );
    archives.refresh(&github, &settings(), T0 + DAY).await;
    let both = BTreeSet::from([
        file_of("actions/checkout", CHECKOUT),
        file_of("actions/checkout", CHECKOUT_NEXT),
        file_of("actions/setup-node", NODE),
    ]);
    assert_eq!(
        on_disk(&dir.0),
        both,
        "jobs at the old pin and the new both find theirs"
    );
    archives.refresh(&github, &settings(), T0 + 14 * DAY).await;
    assert_eq!(on_disk(&dir.0), both, "kept for 14 days after its last pin");
    archives.refresh(&github, &settings(), T0 + 15 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([
            file_of("actions/checkout", CHECKOUT_NEXT),
            file_of("actions/setup-node", NODE),
        ]),
        "the unpinned commit went; the pinned ones, a year old or not, stay"
    );
    archives.refresh(&github, &settings(), T0 + 400 * DAY).await;
    assert_eq!(on_disk(&dir.0).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_corrupt_download_never_reaches_archives_and_is_reported() {
    let top = "o-r-08eba0b/";
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("truncated gzip", {
            let good = archive(CHECKOUT, "runs: {}\n");
            good[..good.len() - 12].to_vec()
        }),
        ("wrong PAX comment", archive(CHECKOUT_NEXT, "runs: {}\n")),
        (
            "two top directories",
            gzip(&tar_of(
                CHECKOUT,
                &[
                    Item::Dir(top),
                    Item::Dir("other/"),
                    Item::File("other/a", "a"),
                ],
            )),
        ),
        (
            "a .. entry",
            gzip(&tar_of(
                CHECKOUT,
                &[Item::Dir(top), Item::File("o-r-08eba0b/../../evil", "x")],
            )),
        ),
        (
            "an escaping symlink",
            gzip(&tar_of(
                CHECKOUT,
                &[
                    Item::Dir(top),
                    Item::Symlink("o-r-08eba0b/a/link", "../../../etc/passwd"),
                ],
            )),
        ),
        (
            "a hard link",
            gzip(&tar_of(
                CHECKOUT,
                &[
                    Item::Dir(top),
                    Item::HardLink("o-r-08eba0b/link", "/etc/passwd"),
                ],
            )),
        ),
        (
            "a FIFO",
            gzip(&tar_of(
                CHECKOUT,
                &[Item::Dir(top), Item::Fifo("o-r-08eba0b/pipe")],
            )),
        ),
        (
            "no PAX header",
            gzip(&tar::Builder::new(Vec::new()).into_inner().unwrap()),
        ),
        ("a second gzip member", {
            let mut bytes = archive(CHECKOUT, "runs: {}\n");
            bytes.extend(gzip(b"more"));
            bytes
        }),
    ];
    for (case, bytes) in cases {
        let dir = TestDir::new("archives");
        let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
        github.set_tarball(&pin("actions/checkout", CHECKOUT), bytes);
        let mut archives = open(&dir.0);
        archives.refresh(&github, &settings(), T0).await;
        assert!(on_disk(&dir.0).is_empty(), "{case}: nothing a runner reads");
        assert_eq!(
            count(&dir.0, "staging"),
            0,
            "{case}: the download is removed"
        );
        let failures = archives.status().failures;
        assert_eq!(failures.len(), 1, "{case}: {failures:?}");
        assert_eq!(failures[0].kind, ArchiveFailureKind::Rejected, "{case}");
        assert!(
            failures[0].line().starts_with("actions/checkout@08eba0b: "),
            "{}",
            failures[0].line()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_archive_changed_on_disk_is_quarantined_and_fetched_again() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    let file = dir
        .0
        .join("archives")
        .join(file_of("actions/checkout", CHECKOUT));
    // A valid archive, but not the one verified.
    fs::write(&file, archive(CHECKOUT, "runs: {using: evil}\n")).unwrap();
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(github.downloads().len(), 2, "fetched again");
    assert_eq!(count(&dir.0, "quarantine"), 1, "kept aside, not deleted");
    let (sha256, _) = verify::hash_file(&file).unwrap();
    let expected = dir.0.join("expected.tar.gz");
    fs::write(
        &expected,
        archive(CHECKOUT, "runs:\n  using: node20\n  main: dist/main.js\n"),
    )
    .unwrap();
    let (good, _) = verify::hash_file(&expected).unwrap();
    assert_eq!(sha256, good, "the verified archive is back");
    let failures = archives.status().failures;
    assert!(
        failures.is_empty(),
        "fetched again, so nothing failed: {failures:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_killed_midway_leaves_nothing_a_runner_reads_and_recovers_at_the_next_start() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    drop(open(&dir.0));
    // Killed mid-download, and killed after a rename the index never heard of.
    fs::write(dir.0.join("staging/1f2e3d"), b"\x1f\x8b half").unwrap();
    let renamed = dir
        .0
        .join("archives")
        .join(file_of("actions/checkout", CHECKOUT));
    fs::create_dir_all(renamed.parent().unwrap()).unwrap();
    fs::write(&renamed, &archive(CHECKOUT, "runs: {}\n")[..40]).unwrap();
    let mut archives = open(&dir.0);
    assert_eq!(
        count(&dir.0, "staging"),
        0,
        "the half download is gone at start"
    );
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(
        count(&dir.0, "quarantine"),
        1,
        "the file the index never named"
    );
    assert_eq!(github.downloads().len(), 1);
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)])
    );
    assert!(verify::verify(&renamed, CHECKOUT, verify::BUDGET).is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_tree_cut_short_or_a_failed_read_keeps_the_last_pins() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    github.push("c2", &[]);
    github
        .0
        .lock()
        .unwrap()
        .trees
        .get_mut("c2")
        .unwrap()
        .truncated = true;
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    let failures = archives.status().failures;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].kind, ArchiveFailureKind::Pins);
    assert_eq!(failures[0].subject, SERVED);
    github.0.lock().unwrap().down = true;
    archives.refresh(&github, &settings(), T0 + 40 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)]),
        "still pinned by the last pins read"
    );
    // GitHub answers again, and the tree is whole.
    github.0.lock().unwrap().down = false;
    github.push("c3", &[&pin("actions/checkout", CHECKOUT)]);
    archives.refresh(&github, &settings(), T0 + 41 * DAY).await;
    assert_eq!(archives.status().failures, Vec::new());
}

#[tokio::test(start_paused = true)]
async fn a_failed_download_is_reported_and_tried_again_at_the_next_refresh() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    github
        .0
        .lock()
        .unwrap()
        .failing
        .insert(pin("actions/checkout", CHECKOUT));
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(github.downloads().len(), 3, "three attempts");
    let status = archives.status();
    assert_eq!(status.failures[0].kind, ArchiveFailureKind::Download);
    assert!(status.attention_title().is_some());
    github.0.lock().unwrap().failing.clear();
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(archives.status().entries, 1);
    assert_eq!(archives.status().failures, Vec::new());
}

#[test]
fn a_git_archive_shaped_tarball_with_confined_links_passes_and_lists_its_action_files() {
    let dir = TestDir::new("archives");
    let path = dir.0.join("a.tar.gz");
    fs::write(&path, archive(CHECKOUT, "runs: {}\n")).unwrap();
    let verified = verify::verify(&path, CHECKOUT, verify::BUDGET).unwrap();
    assert_eq!(verified.actions.keys().collect::<Vec<_>>(), vec![""]);
    let tight = verify::Budget {
        expanded: 1024,
        ..verify::BUDGET
    };
    assert!(matches!(
        verify::verify(&path, CHECKOUT, tight),
        Err(Rejected::Budget(_))
    ));
}

/// One step of a generated sequence.
#[derive(Debug, Clone, Copy)]
enum Step {
    Refresh,
    Restart,
    /// The served repository pins the `n`th of the pin sets below.
    Pins(usize),
    /// The next downloads of every archive fail, or succeed again.
    Down(bool),
    /// A kept archive's bytes change.
    Damage,
    /// A download killed midway: a staged file and a renamed one the index never named.
    Killed,
    Days(u64),
}

/// A small seeded generator: the same seed, the same sequence.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

fn steps(seed: u64) -> Vec<Step> {
    let mut lcg = Lcg(seed);
    (0..40)
        .map(|_| match lcg.next(10) {
            0..=2 => Step::Refresh,
            3 => Step::Restart,
            4 => Step::Pins(usize::try_from(lcg.next(3)).unwrap()),
            5 => Step::Down(lcg.next(2) == 0),
            6 => Step::Damage,
            7 => Step::Killed,
            _ => Step::Days(lcg.next(9)),
        })
        .collect()
}

/// After every refresh: every file runners can read under `archives/` is one the index names and verified, whole and
/// of its commit; nothing of `staging/` is visible; and every archive the current pins name and that was ever kept
/// since is still there unless it failed its check.
fn check_invariants(root: &Path, case: &str) {
    let index: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("index.json")).unwrap_or_else(|_| b"{\"entries\":[]}".to_vec()),
    )
    .unwrap();
    let named: BTreeMap<String, String> = index["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let repository = entry["repository"].as_str().unwrap();
            let sha = entry["sha"].as_str().unwrap();
            (
                file_of(repository, sha),
                entry["sha256"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for file in on_disk(root) {
        let sha256 = named
            .get(&file)
            .unwrap_or_else(|| panic!("{case}: {file} is readable but not in the index"));
        let path = root.join("archives").join(&file);
        let (found, _) = verify::hash_file(&path).unwrap();
        assert_eq!(&found, sha256, "{case}: {file} is not what was verified");
        let sha = file.rsplit('/').next().unwrap().trim_end_matches(".tar.gz");
        assert!(
            verify::verify(&path, sha, verify::BUDGET).is_ok(),
            "{case}: {file} does not verify"
        );
    }
    for file in named.keys() {
        assert!(
            on_disk(root).contains(file),
            "{case}: the index names {file}, which is gone"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_store_holds_only_verified_archives_over_generated_refreshes_restarts_and_failures() {
    let cases: u64 = std::env::var("RUNNERD_INVARIANT_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(24);
    let sets: [Vec<String>; 3] = [
        vec![pin("actions/checkout", CHECKOUT)],
        vec![
            pin("actions/checkout", CHECKOUT_NEXT),
            pin("jdx/mise-action", MISE),
        ],
        vec![
            pin("tailscale/github-action/oauth", OAUTH),
            pin("actions/setup-node", NODE),
        ],
    ];
    for seed in 0..cases {
        let dir = TestDir::new("archives");
        let github = FakeGithub::new("h0", &[&pin("actions/checkout", CHECKOUT)]);
        let mut archives = open(&dir.0);
        let mut now = T0;
        let mut pushes = 0;
        let mut current = sets[0].clone();
        let sequence = steps(seed);
        for (at, step) in sequence.iter().enumerate() {
            let case = format!("seed {seed}, step {at} of {sequence:?}");
            match *step {
                Step::Refresh => {
                    archives.refresh(&github, &settings(), now).await;
                    check_invariants(&dir.0, &case);
                    let down = github.0.lock().unwrap().down;
                    if !down {
                        // What is pinned now and not failing is there.
                        let held = on_disk(&dir.0);
                        for label in &current {
                            let (repository, sha) = label.split_once('@').unwrap();
                            let repository =
                                repository.split('/').take(2).collect::<Vec<_>>().join("/");
                            let canonical = github.0.lock().unwrap().repositories
                                [&repository.to_ascii_lowercase()]
                                .full_name
                                .clone();
                            assert!(
                                held.contains(&file_of(&canonical, sha)),
                                "{case}: {label} is pinned and GitHub answers, but it is not kept"
                            );
                        }
                    }
                }
                Step::Restart => archives = open(&dir.0),
                Step::Pins(set) => {
                    pushes += 1;
                    current = sets[set].clone();
                    let pins: Vec<&str> = current.iter().map(String::as_str).collect();
                    github.push(&format!("h{pushes}"), &pins);
                }
                Step::Down(down) => github.0.lock().unwrap().down = down,
                Step::Damage => {
                    if let Some(file) = on_disk(&dir.0).into_iter().next() {
                        let path = dir.0.join("archives").join(file);
                        let mut bytes = fs::read(&path).unwrap();
                        let last = bytes.len() - 1;
                        bytes[last] ^= 0xff;
                        fs::write(path, bytes).unwrap();
                    }
                }
                Step::Killed => {
                    fs::write(dir.0.join("staging/half"), b"\x1f\x8b").unwrap();
                    let stray: PathBuf = dir.0.join("archives/actions_setup-node");
                    fs::create_dir_all(&stray).unwrap();
                    fs::write(stray.join(format!("{NODE}.tar.gz.partial")), b"\x1f\x8b").unwrap();
                    archives = open(&dir.0);
                }
                Step::Days(days) => now += days * DAY,
            }
        }
    }
}

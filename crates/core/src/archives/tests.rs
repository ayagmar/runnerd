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

use super::pins::MAX_DEPTH;
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

/// An archive of `sha` holding 3000 bytes gzip cannot shrink, so it is about that large.
fn large_archive(sha: &str) -> Vec<u8> {
    let mut lcg = Lcg(u64::from_str_radix(&sha[..8], 16).unwrap());
    let noise: String = (0..3000)
        .map(|_| char::from(b'!' + u8::try_from(lcg.next(90)).unwrap()))
        .collect();
    let top = format!("o-r-{}/", &sha[..7]);
    let file = format!("{top}action.yml");
    let blob = format!("{top}blob.txt");
    gzip(&tar_of(
        sha,
        &[
            Item::Dir(&top),
            Item::File(&file, "runs: {}\n"),
            Item::File(&blob, &noise),
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
    /// Requests to GitHub's API answered.
    requests: u32,
    /// What GitHub's rate limit has left, as its last answer said.
    spare: Option<u64>,
    /// Bytes the downloads wrote, failed ones included.
    written: u64,
    /// Downloads that fail after writing this many bytes, by `owner/repo@sha`.
    partial: BTreeMap<String, usize>,
    /// Repositories that turn private as their archive is asked for, between the lookup and the download.
    private_at_download: BTreeSet<String>,
}

/// A fake GitHub; clones share it.
#[derive(Debug, Clone, Default)]
struct FakeGithub(Arc<Mutex<Fake>>);

fn unreachable() -> Error {
    Error::Download("GitHub did not answer".to_owned())
}

fn not_found(path: String) -> Error {
    Error::Status {
        method: reqwest::Method::GET,
        path,
        status: reqwest::StatusCode::NOT_FOUND,
        message: "Not Found".to_owned(),
    }
}

impl Source for FakeGithub {
    async fn action_repository(&self, repository: &str) -> Result<ActionRepository, Error> {
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
        if fake.down {
            return Err(unreachable());
        }
        fake.repositories
            .get(&repository.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| not_found(format!("/repos/{repository}")))
    }

    async fn branch_head(&self, _: &str, _: &str) -> Result<String, Error> {
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
        if fake.down {
            return Err(unreachable());
        }
        Ok(fake.head.clone())
    }

    async fn tree(&self, _: &str, commit: &str) -> Result<Tree, Error> {
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
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
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
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
            let lower = repository.to_ascii_lowercase();
            if fake.private_at_download.remove(&lower)
                && let Some(found) = fake.repositories.get_mut(&lower)
            {
                found.public = false;
            }
            // Anonymous: a repository that is not public is not there.
            if !fake
                .repositories
                .get(&lower)
                .is_some_and(|found| found.public)
            {
                return Err(not_found(format!("/{repository}/tar.gz/{commit}")));
            }
            if let Some(&cut) = fake.partial.get(&label) {
                fs::write(to, vec![0_u8; cut]).unwrap();
                fake.written += cut as u64;
                return Err(unreachable());
            }
            if fake.down || fake.failing.contains(&label) {
                return Err(unreachable());
            }
            let bytes = fake.tarballs.get(&label).cloned().unwrap();
            fake.written += bytes.len() as u64;
            bytes
        };
        assert!(bytes.len() as u64 <= limit);
        fs::write(to, &bytes).unwrap();
        Ok(bytes.len() as u64)
    }

    fn spare_requests(&self) -> Option<u64> {
        self.0.lock().unwrap().spare
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
    Archives::new(dir.to_owned())
}

/// The archives under `archives/`, as `<dir>/<file>`.
fn on_disk(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(dirs) = fs::read_dir(root.join("archives")) else {
        return found;
    };
    for dir in dirs {
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
    drop(ArchiveStore::open(&dir.0).unwrap());
    // Killed mid-download, and killed after a rename the index never heard of.
    fs::write(dir.0.join("staging/1f2e3d"), b"\x1f\x8b half").unwrap();
    let renamed = dir
        .0
        .join("archives")
        .join(file_of("actions/checkout", CHECKOUT));
    fs::create_dir_all(renamed.parent().unwrap()).unwrap();
    fs::write(&renamed, &archive(CHECKOUT, "runs: {}\n")[..40]).unwrap();
    let mut archives = open(&dir.0);
    assert!(
        archives.check(T0).await,
        "the start's check leaves only what the index names"
    );
    assert_eq!(
        count(&dir.0, "staging"),
        0,
        "the half download is gone at start"
    );
    assert_eq!(
        count(&dir.0, "quarantine"),
        1,
        "the file the index never named"
    );
    archives.update(&github, &settings(), T0).await;
    assert_eq!(github.downloads().len(), 1);
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)])
    );
    assert!(verify::verify(&renamed, CHECKOUT, verify::BUDGET).is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_store_opens_where_its_parents_are_not_there_yet() {
    let dir = TestDir::new("archives");
    let root = dir.0.join("cache/runnerd/actions");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&root);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(
        on_disk(&root),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)])
    );
    let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "every runner reads through it");
}

#[test]
fn a_second_owner_of_one_store_is_refused_until_the_first_lets_go() {
    let dir = TestDir::new("archives");
    let first = ArchiveStore::open(&dir.0).unwrap();
    let second = ArchiveStore::open(&dir.0).map(drop).unwrap_err();
    assert_eq!(second.kind(), std::io::ErrorKind::WouldBlock);
    drop(first);
    assert!(
        ArchiveStore::open(&dir.0).is_ok(),
        "the store opens once its owner is gone"
    );
}

#[tokio::test(start_paused = true)]
async fn a_store_that_cannot_open_says_so_and_opens_once_it_can() {
    let dir = TestDir::new("archives");
    let root = dir.0.join("actions");
    fs::write(&root, b"not a directory").unwrap();
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&root);
    assert!(!archives.check(T0).await, "nothing is good to read");
    archives.update(&github, &settings(), T0).await;
    let failures = archives.status().failures;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].kind, ArchiveFailureKind::Store);
    fs::remove_file(&root).unwrap();
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(archives.status().failures, Vec::new());
    assert_eq!(archives.status().entries, 1);
}

#[tokio::test(start_paused = true)]
async fn a_check_goes_past_what_it_cannot_read_and_quarantines_whatever_sits_at_an_archive_s_place()
{
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
    // A directory where checkout's archive was, and setup-node's bytes changed after it.
    let checkout = dir
        .0
        .join("archives")
        .join(file_of("actions/checkout", CHECKOUT));
    fs::remove_file(&checkout).unwrap();
    fs::create_dir(&checkout).unwrap();
    fs::write(checkout.join("inside"), b"x").unwrap();
    let node = dir
        .0
        .join("archives")
        .join(file_of("actions/setup-node", NODE));
    let mut bytes = fs::read(&node).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&node, bytes).unwrap();
    assert!(archives.check(T0 + 600).await, "both were moved aside");
    assert_eq!(count(&dir.0, "quarantine"), 2);
    archives.update(&github, &settings(), T0 + 600).await;
    assert_eq!(github.downloads().len(), 4, "both fetched again");
    check_invariants(&dir.0, "after the check");
    // A file it cannot read leaves the store not good, and the rest checked.
    if !rustix::process::geteuid().is_root() {
        fs::set_permissions(&node, fs::Permissions::from_mode(0o000)).unwrap();
        let stray = dir.0.join("archives/actions_checkout/stray.tar.gz");
        fs::write(&stray, b"x").unwrap();
        assert!(
            !archives.check(T0 + 1200).await,
            "one archive cannot be read"
        );
        assert!(!stray.exists(), "the stranger was still moved aside");
        fs::set_permissions(&node, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn an_index_that_names_a_path_outside_archives_is_never_followed() {
    let dir = TestDir::new("archives");
    let root = dir.0.join("actions");
    fs::create_dir_all(&root).unwrap();
    let outside = dir.0.join("precious.tar.gz");
    fs::write(&outside, b"keep me").unwrap();
    fs::write(
        root.join("index.json"),
        r#"{"entries":[{"repository":"o/r","sha":"../../../precious","sha256":"00","size":1,"fetched_at":0,"pinned_at":0}]}"#,
    )
    .unwrap();
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&root);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(fs::read(&outside).unwrap(), b"keep me");
    assert_eq!(archives.status().entries, 1, "only the archive fetched");
}

#[tokio::test(start_paused = true)]
async fn an_archive_whose_repository_turns_private_is_withdrawn_and_never_fetched_again() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
            &pin("jdx/mise-action", MISE),
        ],
    );
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(on_disk(&dir.0).len(), 3);
    let private = |name: &str| {
        github
            .0
            .lock()
            .unwrap()
            .repositories
            .get_mut(name)
            .unwrap()
            .public = false;
    };
    // Its default branch has not moved: the pins are as they were.
    private("actions/checkout");
    // Changed on disk as well, so a refresh would fetch it again.
    private("actions/setup-node");
    let node = dir
        .0
        .join("archives")
        .join(file_of("actions/setup-node", NODE));
    fs::write(&node, b"changed").unwrap();
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("jdx/mise-action", MISE)]),
        "what is no longer public is gone, whatever pins it"
    );
    // Public when looked up, private by the time its archive is asked for.
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("jdx/mise-action", MISE)]);
    github
        .0
        .lock()
        .unwrap()
        .private_at_download
        .insert("jdx/mise-action".to_owned());
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert!(
        on_disk(&dir.0).is_empty(),
        "an anonymous download of it finds nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_downloads_no_more_than_its_bytes_and_leaves_the_rest_to_the_next() {
    let limits = super::Limits {
        archive: 4096,
        bytes: 2 * 4096,
        requests: 1000,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
            &pin("jdx/mise-action", MISE),
            &pin("actions/checkout", CHECKOUT_NEXT),
        ],
    );
    for (repository, sha) in [
        ("actions/checkout", CHECKOUT),
        ("actions/setup-node", NODE),
        ("jdx/mise-action", MISE),
        ("actions/checkout", CHECKOUT_NEXT),
    ] {
        github.set_tarball(&pin(repository, sha), large_archive(sha));
    }
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(
        github.downloads().len(),
        2,
        "two downloads' worth of bytes, no more"
    );
    assert_eq!(
        archives.status().failures,
        Vec::new(),
        "what waits is no failure"
    );
    archives.refresh(&github, &settings(), T0 + 600).await;
    assert_eq!(
        on_disk(&dir.0).len(),
        4,
        "the next refresh fetched the rest"
    );
    // A download that fails midway, each attempt, counts what it wrote.
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    github
        .0
        .lock()
        .unwrap()
        .partial
        .insert(pin("actions/checkout", CHECKOUT), 3000);
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    let written = github.0.lock().unwrap().written;
    assert!(
        written <= limits.bytes,
        "{written} bytes written past the refresh's {}",
        limits.bytes
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_asks_github_no_more_than_its_requests_and_nothing_once_the_limit_is_low() {
    let limits = super::Limits {
        archive: 256 << 20,
        bytes: 2 << 30,
        requests: 3,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
        ],
    );
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    assert!(
        github.0.lock().unwrap().requests <= 3,
        "within the refresh's requests"
    );
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    github.0.lock().unwrap().spare = Some(10);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(
        github.0.lock().unwrap().requests,
        0,
        "the rest of the limit is the runners'"
    );
    assert_eq!(github.downloads(), Vec::<String>::new());
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
async fn an_outage_across_a_restart_removes_nothing_the_last_whole_inventory_reached() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("tailscale/github-action/oauth", OAUTH)]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    let reached = BTreeSet::from([
        file_of("Tailscale/github-action", OAUTH),
        file_of("actions/checkout", CHECKOUT_NEXT),
    ]);
    assert_eq!(on_disk(&dir.0), reached);
    drop(archives);
    github.0.lock().unwrap().down = true;
    let mut restarted = open(&dir.0);
    restarted.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        reached,
        "what o/app pinned, nested pins included, when it was last read whole"
    );
    // A repository no longer served takes its inventory with it.
    let mut none = settings();
    none.repositories.clear();
    restarted.refresh(&github, &none, T0 + 40 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::new(),
        "nothing serves them any more"
    );
}

#[tokio::test(start_paused = true)]
async fn pins_beyond_what_is_read_remove_nothing_while_they_stay_unread() {
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
    github.push("c2", &[&pin("actions/setup-node", NODE)]);
    archives.refresh(&github, &settings(), T0 + DAY).await;
    // c3 has more workflows than are read; the last of them pins checkout again.
    github.push("c3", &[&pin("actions/setup-node", NODE)]);
    {
        let mut fake = github.0.lock().unwrap();
        let files = &mut fake.trees.get_mut("c3").unwrap().files;
        for n in 0..300 {
            files.insert(
                format!(".github/workflows/w{n:03}.yml"),
                "on: push\n".to_owned(),
            );
        }
        files.insert(
            ".github/workflows/z.yml".to_owned(),
            format!(
                "jobs:\n  test:\n    steps:\n      - uses: {}\n",
                pin("actions/checkout", CHECKOUT)
            ),
        );
    }
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT)),
        "a file left unread may pin it"
    );
    let failures = archives.status().failures;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].kind, ArchiveFailureKind::Pins);
    // A workflow that is not YAML a runner reads is no more read as pinning nothing.
    github.push("c4", &[&pin("actions/setup-node", NODE)]);
    github
        .0
        .lock()
        .unwrap()
        .trees
        .get_mut("c4")
        .unwrap()
        .files
        .insert(
            ".github/workflows/bad.yml".to_owned(),
            "jobs: [\n".to_owned(),
        );
    archives.refresh(&github, &settings(), T0 + 21 * DAY).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT)),
        "a file that does not parse may pin it"
    );
    github.push("c5", &[&pin("actions/setup-node", NODE)]);
    archives.refresh(&github, &settings(), T0 + 22 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/setup-node", NODE)]),
        "read whole again, what no pin kept for 14 days goes"
    );
}

#[tokio::test(start_paused = true)]
async fn an_archive_whose_action_file_is_unread_or_missing_removes_nothing() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("jdx/mise-action", MISE),
        ],
    );
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    // c2 drops checkout and pins mise at a commit whose action file is not YAML: what it pins is not known.
    github.set_tarball(
        &format!("jdx/mise-action@{NODE}"),
        archive(NODE, "runs: [\n"),
    );
    github.push("c2", &[&pin("jdx/mise-action", NODE)]);
    archives.refresh(&github, &settings(), T0 + DAY).await;
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT)),
        "the unread action file may pin it"
    );
    // c3 pins one whose download keeps failing: what it pins is not known either.
    github.push("c3", &[&pin("actions/setup-node", NODE)]);
    github
        .0
        .lock()
        .unwrap()
        .failing
        .insert(pin("actions/setup-node", NODE));
    archives.refresh(&github, &settings(), T0 + 40 * DAY).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT)),
        "an archive not held may pin it"
    );
    github.0.lock().unwrap().failing.clear();
    archives.refresh(&github, &settings(), T0 + 41 * DAY).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT)),
        "what the last whole inventory reached was pinned until it was replaced"
    );
    archives.refresh(&github, &settings(), T0 + 56 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/setup-node", NODE)]),
        "known whole again, what no pin kept for 14 days went"
    );
}

#[tokio::test(start_paused = true)]
async fn pins_are_followed_as_deep_as_a_runner_nests_and_no_deeper() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[]);
    // Level n is n/chain at a commit of n's, whose action pins level n + 1; level 0 is the workflow's.
    let sha = |level: u32| format!("{level:0>40}");
    {
        let mut fake = github.0.lock().unwrap();
        for level in 0..=MAX_DEPTH + 1 {
            let name = format!("n{level}/chain");
            fake.repositories.insert(
                name.clone(),
                ActionRepository {
                    full_name: name.clone(),
                    public: true,
                    default_branch: "main".to_owned(),
                },
            );
            let action = format!(
                "runs:\n  using: composite\n  steps:\n    - uses: n{}/chain@{}\n",
                level + 1,
                sha(level + 1)
            );
            fake.tarballs
                .insert(pin(&name, &sha(level)), archive(&sha(level), &action));
        }
    }
    github.push("c1", &[&pin("n0/chain", &sha(0))]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    let expected: BTreeSet<String> = (0..=MAX_DEPTH)
        .map(|level| file_of(&format!("n{level}/chain"), &sha(level)))
        .collect();
    assert_eq!(
        on_disk(&dir.0),
        expected,
        "levels 0 to {MAX_DEPTH}; the runner never runs level {}",
        MAX_DEPTH + 1
    );
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert_eq!(on_disk(&dir.0), expected, "a whole chain keeps every level");
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
                    fs::create_dir_all(dir.0.join("staging")).unwrap();
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

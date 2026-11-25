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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use tar::{EntryType, Header};

use super::pins::MAX_DEPTH;
use super::store::ArchiveStore;
use super::verify::{self, Rejected};
use super::{ActionRepository, Archives, Budget, Settings, Source, Tree, TreeFile};
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
    let mut header = Header::new_ustar();
    header.set_entry_type(kind);
    let bytes = name.as_bytes();
    header.as_ustar_mut().unwrap().name[..bytes.len()].copy_from_slice(bytes);
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
                entry.as_ustar_mut().unwrap().linkname[..bytes.len()].copy_from_slice(bytes);
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
    let dist = format!("{top}dist/");
    let main = format!("{top}dist/main.js");
    let readme = format!("{top}README.md");
    gzip(&tar_of(
        sha,
        &[
            Item::Dir(&top),
            Item::File(&file, action),
            Item::File(&readme, "an action"),
            Item::Dir(&dist),
            Item::File(&main, "console.log(1)"),
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
    /// Requests sent to GitHub's API, each send of one counted.
    sent: u64,
    /// What GitHub's rate limit has left, as its last answer said.
    spare: Option<u64>,
    /// Bytes the downloads wrote, failed ones included.
    written: u64,
    /// Downloads that fail after writing this many bytes, by `owner/repo@sha`.
    partial: BTreeMap<String, usize>,
    /// Repositories that turn private as their archive is asked for, between the lookup and the download.
    private_at_download: BTreeSet<String>,
    /// Whether GitHub refuses each request's first token, so each is sent twice, as `Github` does after a 401.
    refusing: bool,
    /// Downloads that fail after receiving this many bytes and writing none, as one whose last chunk passed the cap,
    /// by `owner/repo@sha`.
    unwritten: BTreeMap<String, usize>,
    /// Repositories, lower case, whose lookup GitHub does not answer.
    unanswered: BTreeSet<String>,
    /// Downloads that send their bytes only once this file is there, as one that finishes after another archive is
    /// published, by `owner/repo@sha`. The wait blocks the runtime's thread, so no other download moves meanwhile.
    after: BTreeMap<String, PathBuf>,
}

impl Fake {
    /// Counts a request sent to GitHub's API, as `Github` counts it in `github::REQUESTS`.
    fn send(&mut self) {
        let sends = if self.refusing { 2 } else { 1 };
        self.sent += sends;
        let _ = crate::github::REQUESTS.try_with(|sent| sent.set(sent.get() + sends));
    }
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
    async fn served_repository(&self, repository: &str) -> Result<ActionRepository, Error> {
        self.action_repository(repository).await
    }

    async fn action_repository(&self, repository: &str) -> Result<ActionRepository, Error> {
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
        fake.send();
        if fake.down || fake.unanswered.contains(&repository.to_ascii_lowercase()) {
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
        fake.send();
        if fake.down {
            return Err(unreachable());
        }
        Ok(fake.head.clone())
    }

    async fn tree(&self, _: &str, commit: &str) -> Result<Tree, Error> {
        let mut fake = self.0.lock().unwrap();
        fake.requests += 1;
        fake.send();
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
        fake.send();
        let (commit, path) = blob.split_once(':').unwrap();
        Ok(fake.trees[commit].files[path].clone().into_bytes())
    }

    async fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
        received: &AtomicU64,
    ) -> Result<u64, Error> {
        let label = format!("{repository}@{commit}");
        let after = self.0.lock().unwrap().after.get(&label).cloned();
        if let Some(path) = after {
            let start = std::time::Instant::now();
            while !path.exists() {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "{label} waited for {} in vain",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
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
                received.fetch_add(cut as u64, Ordering::Relaxed);
                return Err(unreachable());
            }
            if let Some(&cut) = fake.unwritten.get(&label) {
                fake.written += cut as u64;
                received.fetch_add(cut as u64, Ordering::Relaxed);
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
        received.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes.len() as u64)
    }

    fn spare_requests(&self, _: Budget) -> Option<u64> {
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
        archives.check(T0).await.good(),
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
    archives.refill(&github, &settings(), T0).await;
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
    assert!(!archives.check(T0).await.good(), "nothing is good to read");
    archives.refill(&github, &settings(), T0).await;
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
    assert!(
        archives.check(T0 + 600).await.good(),
        "both were moved aside"
    );
    assert_eq!(count(&dir.0, "quarantine"), 2);
    archives.refill(&github, &settings(), T0 + 600).await;
    assert_eq!(github.downloads().len(), 4, "both fetched again");
    check_invariants(&dir.0, "after the check");
    // A file it cannot read leaves the store not good, and the rest checked.
    if !rustix::process::geteuid().is_root() {
        fs::set_permissions(&node, fs::Permissions::from_mode(0o000)).unwrap();
        let stray = dir.0.join("archives/actions_checkout/stray.tar.gz");
        fs::write(&stray, b"x").unwrap();
        assert!(
            !archives.check(T0 + 1200).await.good(),
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
        store: 8 << 30,
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
        store: 8 << 30,
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
    let failures: Vec<(String, ArchiveFailureKind)> = archives
        .status()
        .failures
        .into_iter()
        .map(|failure| (failure.subject, failure.kind))
        .collect();
    assert_eq!(
        failures,
        vec![
            (SERVED.to_owned(), ArchiveFailureKind::Pins),
            (SERVED.to_owned(), ArchiveFailureKind::Held),
        ],
        "its pins could not be read, and the archives it last reached are kept"
    );
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

/// The subjects and kinds of `archives`' failures.
fn failures_of(archives: &Archives) -> Vec<(String, ArchiveFailureKind)> {
    archives
        .status()
        .failures
        .into_iter()
        .map(|failure| (failure.subject, failure.kind))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_repository_whose_pins_are_not_all_read_keeps_what_it_last_reached_and_retention_goes_on()
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
    github.push("c2", &[&pin("actions/setup-node", NODE)]);
    archives.refresh(&github, &settings(), T0 + DAY).await;
    // c3 has more workflows than are read.
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
    }
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/setup-node", NODE)]),
        "what o/app last reached stays; what nothing reached for 14 days still goes"
    );
    assert_eq!(
        failures_of(&archives),
        vec![
            (SERVED.to_owned(), ArchiveFailureKind::Pins),
            (SERVED.to_owned(), ArchiveFailureKind::Held),
        ]
    );
    // A workflow that is not YAML a runner reads is no more read as pinning nothing.
    github.push("c4", &[]);
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
    archives.refresh(&github, &settings(), T0 + 40 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/setup-node", NODE)]),
        "what o/app last reached stays while a file of it does not parse"
    );
    github.push("c5", &[]);
    archives.refresh(&github, &settings(), T0 + 41 * DAY).await;
    assert_eq!(failures_of(&archives), Vec::new(), "read whole again");
    archives.refresh(&github, &settings(), T0 + 56 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::new(),
        "known whole, what nothing pinned for 14 days since went"
    );
}

#[tokio::test(start_paused = true)]
async fn an_action_runnerd_does_not_support_is_reported_and_holds_nothing() {
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
    // mise at a commit whose action file is not YAML, and checkout's archive at a path with no action file.
    github.set_tarball(
        &format!("jdx/mise-action@{NODE}"),
        archive(NODE, "runs: [\n"),
    );
    github.push(
        "c2",
        &[
            &pin("jdx/mise-action", NODE),
            &pin("actions/checkout/sub", CHECKOUT_NEXT),
        ],
    );
    archives.refresh(&github, &settings(), T0 + DAY).await;
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    let supported = BTreeSet::from([
        file_of("jdx/mise-action", NODE),
        file_of("actions/checkout", CHECKOUT_NEXT),
    ]);
    assert_eq!(
        on_disk(&dir.0),
        supported,
        "neither holds retention: the c1 archives went after 14 days"
    );
    assert_eq!(
        failures_of(&archives),
        vec![
            (
                format!("actions/checkout/sub@{CHECKOUT_NEXT}"),
                ArchiveFailureKind::NotAction
            ),
            (
                format!("jdx/mise-action@{NODE}"),
                ArchiveFailureKind::Unreadable
            ),
        ],
        "each is reported for what it is, not read as pinning nothing"
    );
    // c3 pins one whose download keeps failing: o/app's pins are not all known for now.
    github.push("c3", &[&pin("actions/setup-node", NODE)]);
    github
        .0
        .lock()
        .unwrap()
        .failing
        .insert(pin("actions/setup-node", NODE));
    archives.refresh(&github, &settings(), T0 + 40 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        supported,
        "what o/app reached when its pins were last all known stays"
    );
    assert!(
        failures_of(&archives).contains(&(SERVED.to_owned(), ArchiveFailureKind::Held)),
        "the held retention is reported: {:?}",
        failures_of(&archives)
    );
}

#[tokio::test(start_paused = true)]
async fn a_pin_whose_repository_turns_private_is_never_asked_for_at_the_same_head_and_comes_back_once_public()
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
    let visibility = |public: bool| {
        github
            .0
            .lock()
            .unwrap()
            .repositories
            .get_mut("actions/setup-node")
            .unwrap()
            .public = public;
    };
    visibility(false);
    for refresh in 1..=3 {
        archives
            .refresh(&github, &settings(), T0 + refresh * 600)
            .await;
    }
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)]),
        "withdrawn"
    );
    assert_eq!(github.downloads().len(), 2, "never asked for again");
    assert_eq!(
        failures_of(&archives),
        Vec::new(),
        "a private action is no failure, and holds no retention"
    );
    assert_eq!(
        github.0.lock().unwrap().trees_read,
        1,
        "the head did not move"
    );
    visibility(true);
    archives.refresh(&github, &settings(), T0 + 4 * 600).await;
    assert!(
        on_disk(&dir.0).contains(&file_of("actions/setup-node", NODE)),
        "public again at the same head, it is kept again"
    );
}

#[tokio::test(start_paused = true)]
async fn an_archive_that_fails_its_check_is_downloaded_once_and_its_head_read_once() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new("c1", &[&pin("actions/checkout", CHECKOUT)]);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    let top = format!("o-r-{}/", &NODE[..7]);
    let file = format!("{top}action.yml");
    let link = format!("{top}link");
    github.set_tarball(
        &pin("actions/setup-node", NODE),
        gzip(&tar_of(
            NODE,
            &[
                Item::Dir(&top),
                Item::File(&file, "runs: {}\n"),
                Item::Symlink(&link, "action.yml"),
            ],
        )),
    );
    github.push(
        "c2",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
        ],
    );
    for refresh in 1..=4 {
        archives
            .refresh(&github, &settings(), T0 + refresh * 600)
            .await;
    }
    let node = pin("actions/setup-node", NODE);
    assert_eq!(
        github
            .downloads()
            .iter()
            .filter(|label| **label == node)
            .count(),
        1,
        "the same commit's archive fails its check again"
    );
    assert_eq!(
        github.0.lock().unwrap().trees_read,
        2,
        "each head's tree is read once"
    );
    assert_eq!(
        failures_of(&archives),
        vec![(node, ArchiveFailureKind::Rejected)],
        "reported, and no retention held for it"
    );
}

#[tokio::test(start_paused = true)]
async fn the_store_never_grows_past_its_cap_and_says_when_it_is_full() {
    let limits = super::Limits {
        archive: 4096,
        bytes: 1 << 30,
        requests: 1000,
        store: 2 * 4096,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/checkout", CHECKOUT_NEXT),
            &pin("actions/setup-node", NODE),
        ],
    );
    for (repository, sha) in [
        ("actions/checkout", CHECKOUT),
        ("actions/checkout", CHECKOUT_NEXT),
        ("actions/setup-node", NODE),
    ] {
        github.set_tarball(&pin(repository, sha), large_archive(sha));
    }
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    let status = archives.status();
    assert!(
        status.bytes <= limits.store,
        "{} bytes, past the cap of {}",
        status.bytes,
        limits.store
    );
    assert_eq!(
        status.entries, 2,
        "three downloads at once still fit the cap"
    );
    assert!(
        failures_of(&archives)
            .iter()
            .any(|(_, kind)| *kind == ArchiveFailureKind::Full),
        "{:?}",
        failures_of(&archives)
    );
}

/// An archive published while other downloads are still starting counts once against the cap: in the store's size,
/// no longer in flight.
#[tokio::test(start_paused = true)]
async fn an_archive_published_as_others_start_is_counted_once_against_the_cap() {
    let limits = super::Limits {
        archive: 4096,
        bytes: 1 << 30,
        requests: 1000,
        store: 3 * 4096,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/checkout", CHECKOUT_NEXT),
            &pin("actions/setup-node", NODE),
        ],
    );
    for (repository, sha) in [
        ("actions/checkout", CHECKOUT),
        ("actions/checkout", CHECKOUT_NEXT),
        ("actions/setup-node", NODE),
    ] {
        github.set_tarball(&pin(repository, sha), large_archive(sha));
    }
    // The downloads start in this order; the second sends its bytes once the first's archive is published, so the
    // third reserves its place with the first in the store.
    github.0.lock().unwrap().after.insert(
        pin("actions/checkout", CHECKOUT_NEXT),
        dir.0
            .join("archives")
            .join("actions_checkout")
            .join(format!("{CHECKOUT}.tar.gz")),
    );
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    assert_eq!(
        archives.status().entries,
        3,
        "three archives fit the cap: {:?}",
        failures_of(&archives)
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_counts_the_bytes_it_received_though_it_wrote_none() {
    let limits = super::Limits {
        archive: 4096,
        bytes: 2 * 4096,
        requests: 1000,
        store: 1 << 30,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/checkout", CHECKOUT_NEXT),
            &pin("actions/setup-node", NODE),
        ],
    );
    {
        let mut fake = github.0.lock().unwrap();
        for label in [
            pin("actions/checkout", CHECKOUT),
            pin("actions/checkout", CHECKOUT_NEXT),
            pin("actions/setup-node", NODE),
        ] {
            fake.unwritten.insert(label, 4096);
        }
    }
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    let received = github.0.lock().unwrap().written;
    assert!(
        received <= limits.bytes,
        "{received} bytes received, past the refresh's {}",
        limits.bytes
    );
}

/// A pin whose repository GitHub does not answer for this refresh is not known, so what the served repository last
/// reached is kept past `keep` and the hold is reported; once it answers, retention goes on.
#[tokio::test(start_paused = true)]
async fn a_pin_not_read_this_refresh_keeps_what_its_repository_last_reached() {
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
    let both = BTreeSet::from([
        file_of("actions/checkout", CHECKOUT),
        file_of("actions/setup-node", NODE),
    ]);
    assert_eq!(on_disk(&dir.0), both);
    github
        .0
        .lock()
        .unwrap()
        .unanswered
        .insert("actions/checkout".to_owned());
    let ready = archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert!(
        ready,
        "read with the token, a lookup left unanswered keeps the archives mounted, as before"
    );
    assert_eq!(
        on_disk(&dir.0),
        both,
        "checkout's lookup failed, so what o/app last reached stays past 14 days"
    );
    assert!(
        failures_of(&archives).contains(&(SERVED.to_owned(), ArchiveFailureKind::Held)),
        "the hold is reported: {:?}",
        failures_of(&archives)
    );
    github.0.lock().unwrap().unanswered.clear();
    github.push("c2", &[&pin("actions/setup-node", NODE)]);
    archives.refresh(&github, &settings(), T0 + 35 * DAY).await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/setup-node", NODE)]),
        "known whole again, what nothing pinned for 14 days went"
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_sent_again_after_a_401_counts_against_the_refresh() {
    let limits = super::Limits {
        archive: 256 << 20,
        bytes: 2 << 30,
        // Odd: a request sent twice must not start when only one is left.
        requests: 9,
        store: 8 << 30,
    };
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("actions/setup-node", NODE),
            &pin("jdx/mise-action", MISE),
            &pin("tailscale/github-action/oauth", OAUTH),
        ],
    );
    github.0.lock().unwrap().refusing = true;
    let mut archives = open(&dir.0).with_limits(limits);
    archives.refresh(&github, &settings(), T0).await;
    let sent = github.0.lock().unwrap().sent;
    assert!(
        sent <= u64::from(limits.requests),
        "{sent} requests sent, past the refresh's {}",
        limits.requests
    );
}

#[tokio::test(start_paused = true)]
async fn a_removal_that_fails_shows_in_the_caches_health() {
    if rustix::process::geteuid().is_root() {
        return;
    }
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
    let store = dir.0.display().to_string();
    let locked = |name: &str, mode: u32| {
        fs::set_permissions(
            dir.0.join("archives").join(name),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
    };
    // Retention: checkout is no longer pinned, and cannot be removed.
    github.push("c2", &[&pin("actions/setup-node", NODE)]);
    locked("actions_checkout", 0o555);
    archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    locked("actions_checkout", 0o755);
    assert!(
        failures_of(&archives).contains(&(store.clone(), ArchiveFailureKind::Store)),
        "a retention that failed: {:?}",
        failures_of(&archives)
    );
    // Withdrawal: setup-node turns private, and cannot be removed.
    archives.refresh(&github, &settings(), T0 + 21 * DAY).await;
    github
        .0
        .lock()
        .unwrap()
        .repositories
        .get_mut("actions/setup-node")
        .unwrap()
        .public = false;
    locked("actions_setup-node", 0o555);
    archives
        .refresh(&github, &settings(), T0 + 21 * DAY + 600)
        .await;
    locked("actions_setup-node", 0o755);
    assert!(
        failures_of(&archives).contains(&(store, ArchiveFailureKind::Store)),
        "a withdrawal that failed: {:?}",
        failures_of(&archives)
    );
}

#[tokio::test(start_paused = true)]
async fn a_repository_a_job_resolved_as_a_package_is_withdrawn_never_kept_again_and_reported() {
    let dir = TestDir::new("archives");
    let github = FakeGithub::new(
        "c1",
        &[
            &pin("actions/checkout", CHECKOUT),
            &pin("tailscale/github-action/oauth", OAUTH),
        ],
    );
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;
    assert!(on_disk(&dir.0).contains(&file_of("actions/checkout", CHECKOUT_NEXT)));
    archives
        .packages(&BTreeSet::from(["actions/checkout".to_owned()]), T0 + 60)
        .await;
    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("Tailscale/github-action", OAUTH)]),
        "every archive of it goes, the nested pin's too"
    );
    assert_eq!(
        failures_of(&archives),
        vec![("actions/checkout".to_owned(), ArchiveFailureKind::Package)],
        "reported"
    );
    let downloads = github.downloads().len();
    for refresh in 1..=2 {
        archives
            .refresh(&github, &settings(), T0 + refresh * 600)
            .await;
    }
    assert_eq!(github.downloads().len(), downloads, "never kept again");
    drop(archives);
    let mut restarted = open(&dir.0);
    restarted.refresh(&github, &settings(), T0 + 1800).await;
    assert_eq!(
        github.downloads().len(),
        downloads,
        "not after a restart either"
    );
    assert_eq!(
        failures_of(&restarted),
        vec![("actions/checkout".to_owned(), ArchiveFailureKind::Package)],
        "and still reported"
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
fn a_git_archive_shaped_tarball_passes_within_its_budget_and_lists_its_action_files() {
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

// The real client on a loopback GitHub, its pinned actions read anonymously (ADR 0181): a served repository is read
// with the token, a pinned action's repository and archive without it; a used-up anonymous limit is no disappearance,
// and only GitHub's 404 removes a kept archive.

/// What the loopback GitHub answers for actions/checkout's anonymous lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Public,
    Limited,
    Gone,
}

/// A request the loopback GitHub received: its path and whether it carried a credential.
#[derive(Debug, Clone)]
struct Asked {
    uri: String,
    authorized: bool,
}

const LIVE_TOKEN: &str = "ghs_liveSentinelToken0123456789";

/// Standard base64 with padding, as GitHub sends a blob.
fn base64_of(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let bits = chunk.iter().enumerate().fold(0_u32, |bits, (nth, byte)| {
            bits | u32::from(*byte) << (16 - 8 * nth)
        });
        for nth in 0..4 {
            if nth <= chunk.len() {
                out.push(char::from(ALPHABET[(bits >> (18 - 6 * nth) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Serves `o/app`, private, whose workflow pins actions/checkout and o/secret-action, to the token alone; the pinned
/// repositories and their archives to anyone, as `lookup` says for actions/checkout. o/secret-action is private, so not
/// there without the token.
async fn loopback_github(lookup: Arc<Mutex<Lookup>>) -> (String, Arc<Mutex<Vec<Asked>>>) {
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse as _;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let workflow = format!(
        "jobs:\n  test:\n    steps:\n      - uses: {}\n      - uses: {}\n",
        pin("actions/checkout", CHECKOUT),
        pin("o/secret-action", SECRET)
    );
    let plain = "runs:\n  using: node20\n  main: dist/main.js\n";
    let tarball = archive(CHECKOUT, plain);
    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let authorized = headers.get("authorization").is_some_and(|value| {
            value.as_bytes() == format!("Bearer {LIVE_TOKEN}").as_bytes()
        });
        let uri = uri.to_string();
        log.lock().unwrap().push(Asked {
            uri: uri.clone(),
            authorized: headers.contains_key("authorization"),
        });
        let json = |body: String| ([("content-type", "application/json")], body).into_response();
        let response = match (uri.as_str(), authorized) {
            ("/repos/o/app", true) => json(
                r#"{"full_name":"o/app","visibility":"private","default_branch":"main"}"#.to_owned(),
            ),
            ("/repos/o/app/git/ref/heads/main", true) => {
                json(r#"{"object":{"sha":"c1"}}"#.to_owned())
            }
            ("/repos/o/app/git/trees/c1?recursive=1", true) => json(format!(
                r#"{{"truncated":false,"tree":[{{"path":".github/workflows/ci.yml","type":"blob","sha":"b1","size":{}}}]}}"#,
                workflow.len()
            )),
            ("/repos/o/app/git/blobs/b1", true) => json(format!(
                r#"{{"content":"{}","encoding":"base64","size":{}}}"#,
                base64_of(workflow.as_bytes()),
                workflow.len()
            )),
            ("/repos/actions/checkout", false) => match *lookup.lock().unwrap() {
                Lookup::Public => json(
                    r#"{"full_name":"actions/checkout","visibility":"public","default_branch":"main"}"#
                        .to_owned(),
                ),
                Lookup::Limited => (
                    StatusCode::FORBIDDEN,
                    [
                        ("x-ratelimit-limit", "60"),
                        ("x-ratelimit-remaining", "0"),
                        ("x-ratelimit-reset", "4000000000"),
                    ],
                )
                    .into_response(),
                Lookup::Gone => StatusCode::NOT_FOUND.into_response(),
            },
            (path, false) if path == format!("/actions/checkout/tar.gz/{CHECKOUT}") => {
                tarball.clone().into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        };
        async move { response }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, asked)
}

fn anonymous_github(url: &str) -> crate::github::Github {
    crate::github::Github::with_credentials(
        url,
        crate::github::Credentials::Command(Box::new(|| {
            Ok(crate::github::Token::from_test(LIVE_TOKEN))
        })),
        crate::github::PublicReads::Anonymous,
    )
    .unwrap()
    .with_codeload(url)
}

#[tokio::test]
async fn anonymously_a_private_served_repository_is_scanned_with_the_token_and_its_public_pins_without_it()
 {
    let dir = TestDir::new("archives");
    let lookup = Arc::new(Mutex::new(Lookup::Public));
    let (url, asked) = loopback_github(lookup.clone()).await;
    let github = anonymous_github(&url);
    let mut archives = open(&dir.0);
    archives.refresh(&github, &settings(), T0).await;

    assert_eq!(
        on_disk(&dir.0),
        BTreeSet::from([file_of("actions/checkout", CHECKOUT)]),
        "the private action is never fetched"
    );
    assert!(
        failures_of(&archives)
            .iter()
            .all(|(subject, _)| subject != SERVED),
        "o/app's pins are known whole: {:?}",
        failures_of(&archives)
    );
    let asked = asked.lock().unwrap();
    for request in asked.iter() {
        assert_eq!(
            request.authorized,
            request.uri.starts_with("/repos/o/app"),
            "only the served repository's reads carry the token: {request:?}"
        );
    }
    assert!(
        asked
            .iter()
            .all(|request| !request.uri.contains("secret-action/tar.gz")),
        "a private action's archive is never asked for: {asked:?}"
    );
    assert!(
        asked
            .iter()
            .any(|request| request.uri == "/repos/o/secret-action"),
        "o/secret-action was looked up, anonymously, and is not there"
    );
}

#[tokio::test]
async fn a_used_up_anonymous_limit_keeps_every_archive_but_withdraws_the_view_and_only_a_404_removes_one()
 {
    let dir = TestDir::new("archives");
    let lookup = Arc::new(Mutex::new(Lookup::Public));
    let (url, _) = loopback_github(lookup.clone()).await;
    let mut archives = open(&dir.0);
    let cold = archives
        .refresh(&anonymous_github(&url), &settings(), T0)
        .await;
    assert!(cold, "a cold store has nothing to vet");
    let kept = BTreeSet::from([file_of("actions/checkout", CHECKOUT)]);
    assert_eq!(on_disk(&dir.0), kept);
    let warm = archives
        .refresh(&anonymous_github(&url), &settings(), T0 + 600)
        .await;
    assert!(
        warm,
        "every kept archive's repository answered: runners get them"
    );

    // The anonymous limit is used up for good: every lookup of this client fails.
    *lookup.lock().unwrap() = Lookup::Limited;
    let github = anonymous_github(&url);
    let ready = archives.refresh(&github, &settings(), T0 + 20 * DAY).await;
    assert!(
        !ready,
        "a kept archive whose repository did not answer withdraws the runners' view"
    );
    assert_eq!(
        on_disk(&dir.0),
        kept,
        "a lookup the limit held back is no disappearance, and keeps what o/app reached past 14 days"
    );
    assert!(
        failures_of(&archives).contains(&(SERVED.to_owned(), ArchiveFailureKind::Held)),
        "the verification left incomplete is reported: {:?}",
        failures_of(&archives)
    );
    assert_eq!(
        github.blocked_until(),
        None,
        "the token's reads are not held back"
    );
    let limit = github.requests().rate_limit();
    assert!(
        limit.is_none(),
        "no anonymous reading became the token's: {limit:?}"
    );

    // GitHub says it is not there to anyone: removed.
    *lookup.lock().unwrap() = Lookup::Gone;
    let ready = archives
        .refresh(&anonymous_github(&url), &settings(), T0 + 21 * DAY)
        .await;
    assert_eq!(on_disk(&dir.0), BTreeSet::new());
    assert!(ready, "answered, and removed: the view is whole again");
}

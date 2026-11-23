//! The Grype database's store and task on fakes of Anchore and of Grype (ADR 0160): what runners get, what a failed
//! step keeps, what a restart checks, which generations stay, and a generated sequence of all of them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests: a failed step fails the test"
)]

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::store::{DB_FILE, IMPORT_FILE, MODEL_DIR, WORK_DB, hex};
use super::task::Keeper;
use super::*;
use crate::api::{GRYPE_MAX_AGE_SECS, GrypeFailureKind};
use crate::testdir::TestDir;

/// A day after the first database was built: the clock the tests start at.
const START: u64 = 1_795_415_312;

/// What the fakes do, as each test sets it.
#[derive(Debug, Default)]
struct World {
    /// The database Anchore lists, by when it was built; `None` while it answers no listing.
    listed: Option<u64>,
    /// What goes wrong after the listing.
    fault: Fault,
    /// Docker cannot list the containers; else the generations their labels name.
    mounted: Option<BTreeSet<String>>,
    /// How many listings, downloads and imports were asked for.
    listings: u32,
    downloads: u32,
    imports: u32,
}

/// What goes wrong after the listing, if anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    /// The download fails.
    Download,
    /// Anchore sends other bytes than the listing's checksum.
    Corrupt,
    /// Grype's import fails.
    Import,
    /// Grype says it imported a database built at another time than the listing's.
    OtherDatabase,
}

#[derive(Debug, Clone, Default)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn set(&self, change: impl FnOnce(&mut World)) {
        change(&mut self.0.lock().unwrap());
    }

    fn get<T>(&self, read: impl FnOnce(&World) -> T) -> T {
        read(&self.0.lock().unwrap())
    }
}

/// The archive of the database built at `built`.
fn archive(built: u64) -> Vec<u8> {
    format!("archive of the database built at {built}").into_bytes()
}

/// The database Grype imports from it.
fn database(built: u64) -> Vec<u8> {
    format!("SQLite database built at {built}").into_bytes()
}

fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn path(built: u64) -> String {
    format!("vulnerability-db_v6.1.10_{built}.tar.zst")
}

impl Source for Fake {
    async fn listing(&self) -> Result<Vec<u8>, String> {
        let built = {
            let mut world = self.0.lock().unwrap();
            world.listings += 1;
            world.listed.ok_or("Anchore answered 503")?
        };
        Ok(serde_json::json!({
            "status": "active",
            "schemaVersion": "v6.1.10",
            "built": crate::time::rfc3339(built * 1000),
            "path": path(built),
            "checksum": format!("sha256:{}", sha256(&archive(built))),
        })
        .to_string()
        .into_bytes())
    }

    async fn archive(&self, path: &str, to: &Path, _limit: u64) -> Result<Downloaded, String> {
        let fault = {
            let mut world = self.0.lock().unwrap();
            world.downloads += 1;
            world.fault
        };
        if fault == Fault::Download {
            return Err("connection reset".to_owned());
        }
        let built: u64 = path
            .trim_start_matches("vulnerability-db_v6.1.10_")
            .trim_end_matches(".tar.zst")
            .parse()
            .unwrap();
        let mut bytes = archive(built);
        if fault == Fault::Corrupt {
            bytes.push(b'!');
        }
        std::fs::write(to, &bytes).unwrap();
        Ok(Downloaded {
            sha256: sha256(&bytes),
            bytes: bytes.len() as u64,
        })
    }
}

impl Grype for Fake {
    /// Writes what `grype db import` does: `db/6/vulnerability.db` and `import.json`, in a temporary directory first.
    async fn import(&self, work: &Path, archive_name: &str) -> Result<Imported, String> {
        let fault = {
            let mut world = self.0.lock().unwrap();
            world.imports += 1;
            world.fault
        };
        let bytes = std::fs::read(work.join(archive_name)).map_err(|error| error.to_string())?;
        let built: u64 = String::from_utf8(bytes)
            .unwrap()
            .trim_start_matches("archive of the database built at ")
            .parse()
            .unwrap();
        if fault == Fault::Import {
            // What a failed import leaves: a temporary directory.
            std::fs::create_dir_all(work.join(WORK_DB).join("tmp-v6-import1")).unwrap();
            return Err("unable to hydrate database".to_owned());
        }
        let dir = work.join(WORK_DB).join(MODEL_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(DB_FILE), database(built)).unwrap();
        std::fs::write(dir.join(IMPORT_FILE), r#"{"digest":"xxh64:0"}"#).unwrap();
        // Grype makes its directories 0700, which a runner's user could not read.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Ok(Imported {
            built_at: if fault == Fault::OtherDatabase {
                built - 1
            } else {
                built
            },
            schema: "v6.1.10".to_owned(),
        })
    }

    async fn mounted(&self) -> Result<BTreeSet<String>, String> {
        self.get(|world| world.mounted.clone())
            .ok_or_else(|| "Docker is not answering".to_owned())
    }
}

/// A refresh as the task makes one: prepare, then update.
async fn refresh(keeper: &mut Keeper, fake: &Fake, now: u64) {
    keeper.prepare(now).await;
    keeper.update(fake, fake, now).await;
}

fn generations(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir.join("dbs"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn staging(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("staging"))
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default()
}

/// The database a runner mounts, as bytes: what a scan reads.
fn mounted_database(runner: &RunnerGrype) -> Vec<u8> {
    let source = PathBuf::from(runner.mount.source.as_deref().unwrap());
    std::fs::read(source.join(MODEL_DIR).join(DB_FILE)).unwrap()
}

#[tokio::test]
async fn a_new_database_is_downloaded_checked_imported_and_mounted_read_only() {
    let dir = TestDir::new("grype-new");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut keeper = Keeper::new(dir.0.clone());
    refresh(&mut keeper, &fake, START + 60).await;

    let runner = keeper
        .runner(START + 60)
        .expect("runners mount the database");
    assert_eq!(runner.mount.target.as_deref(), Some(MOUNT));
    assert_eq!(runner.mount.read_only, Some(true), "no job writes it");
    assert_eq!(
        runner.env,
        [
            "GRYPE_DB_CACHE_DIR=/opt/runnerd/grype",
            "GRYPE_DB_AUTO_UPDATE=false",
            "GRYPE_DB_VALIDATE_AGE=true",
        ]
    );
    assert_eq!(mounted_database(&runner), database(START));
    // A runner's user, which is not the daemon's, reads every file.
    let source = PathBuf::from(runner.mount.source.as_deref().unwrap());
    for path in [source.clone(), source.join(MODEL_DIR)] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "{}", path.display());
    }
    let mode = std::fs::metadata(source.join(MODEL_DIR).join(DB_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
    assert!(
        !source.join(path(START)).exists() && staging(&dir.0).is_empty(),
        "the archive is not kept"
    );
    let status = keeper.status(START + 60);
    assert_eq!(
        status,
        api::GrypeDatabase {
            held: Some(api::HeldGrypeDatabase {
                built_at: START,
                schema: "v6.1.10".to_owned(),
                bytes: database(START).len() as u64 + r#"{"digest":"xxh64:0"}"#.len() as u64,
            }),
            mounted: true,
            checked_at: Some(START + 60),
            failure: None,
        }
    );

    // The same listing again downloads nothing.
    refresh(&mut keeper, &fake, START + 3600).await;
    assert_eq!(fake.get(|world| (world.downloads, world.imports)), (1, 1));
}

/// A step that breaks one thing, how it breaks it, and the failure it shows.
type Broken = (&'static str, fn(&mut World), GrypeFailureKind);

/// The bug each step catches: a failure that withdraws or replaces the database runners mount, or leaves a partial
/// one behind.
#[tokio::test]
async fn a_failed_step_keeps_the_database_held_and_says_what_failed() {
    let steps: [Broken; 5] = [
        ("listing", |w| w.listed = None, GrypeFailureKind::Listing),
        (
            "download",
            |w| w.fault = Fault::Download,
            GrypeFailureKind::Download,
        ),
        (
            "checksum",
            |w| w.fault = Fault::Corrupt,
            GrypeFailureKind::Checksum,
        ),
        (
            "import",
            |w| w.fault = Fault::Import,
            GrypeFailureKind::Import,
        ),
        (
            "other database",
            |w| w.fault = Fault::OtherDatabase,
            GrypeFailureKind::Import,
        ),
    ];
    for (step, break_it, kind) in steps {
        let dir = TestDir::new("grype-failed");
        let fake = Fake::default();
        fake.set(|world| {
            world.listed = Some(START);
            world.mounted = Some(BTreeSet::new());
        });
        let mut keeper = Keeper::new(dir.0.clone());
        refresh(&mut keeper, &fake, START + 60).await;
        let held = keeper.runner(START + 60).unwrap();

        fake.set(|world| {
            world.listed = Some(START + 86_400);
            break_it(world);
        });
        let now = START + 86_400 + 60;
        keeper.prepare(now).await;
        let wait = keeper.update(&fake, &fake, now).await;
        assert_eq!(wait, RETRY, "{step}: tried again soon");
        assert_eq!(
            keeper.runner(now),
            Some(held.clone()),
            "{step}: runners keep the database held"
        );
        assert_eq!(mounted_database(&held), database(START), "{step}");
        let failure = keeper.status(now).failure.expect(step);
        assert_eq!(failure.kind, kind, "{step}");
        assert_eq!(failure.since, now, "{step}");
        assert!(staging(&dir.0).is_empty(), "{step}: nothing partial stays");
        assert_eq!(generations(&dir.0).len(), 1, "{step}");

        // Fixed, the next refresh takes the new one and the failure clears.
        fake.set(|world| {
            *world = World {
                listed: Some(START + 86_400),
                mounted: Some(BTreeSet::new()),
                ..World::default()
            };
        });
        let later = now + 1800;
        refresh(&mut keeper, &fake, later).await;
        assert_eq!(
            mounted_database(&keeper.runner(later).unwrap()),
            database(START + 86_400),
            "{step}"
        );
        assert_eq!(keeper.status(later).failure, None, "{step}");
        assert_eq!(
            generations(&dir.0).len(),
            1,
            "{step}: the old one went, as no container mounts it"
        );
    }
}

#[tokio::test]
async fn a_restart_mounts_the_database_held_once_checked_even_while_anchore_is_down() {
    let dir = TestDir::new("grype-restart");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut keeper = Keeper::new(dir.0.clone());
    refresh(&mut keeper, &fake, START + 60).await;
    let held = keeper.runner(START + 60).unwrap();
    drop(keeper);

    fake.set(|world| world.listed = None);
    let mut keeper = Keeper::new(dir.0.clone());
    keeper.prepare(START + 120).await;
    assert_eq!(
        keeper.runner(START + 120),
        Some(held),
        "served before the listing is read"
    );
    keeper.update(&fake, &fake, START + 120).await;
    assert_eq!(
        keeper
            .status(START + 120)
            .failure
            .map(|failure| failure.kind),
        Some(GrypeFailureKind::Listing)
    );
    assert!(keeper.runner(START + 120).is_some(), "still served");
}

/// The bug it catches: a database changed on disk while no daemon ran mounted in runners unchecked.
#[tokio::test]
async fn a_restart_drops_a_database_that_changed_on_disk_and_downloads_it_again() {
    let dir = TestDir::new("grype-changed");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut keeper = Keeper::new(dir.0.clone());
    refresh(&mut keeper, &fake, START + 60).await;
    let source = PathBuf::from(keeper.runner(START + 60).unwrap().mount.source.unwrap());
    drop(keeper);
    std::fs::write(source.join(MODEL_DIR).join(DB_FILE), b"altered").unwrap();

    fake.set(|world| world.listed = None);
    let mut keeper = Keeper::new(dir.0.clone());
    keeper.prepare(START + 120).await;
    assert_eq!(keeper.runner(START + 120), None, "no runner mounts it");
    assert_eq!(
        keeper
            .status(START + 120)
            .failure
            .map(|failure| failure.kind),
        Some(GrypeFailureKind::Store)
    );
    assert!(generations(&dir.0).is_empty(), "it is gone");

    fake.set(|world| world.listed = Some(START));
    keeper.update(&fake, &fake, START + 1800).await;
    assert_eq!(
        mounted_database(&keeper.runner(START + 1800).unwrap()),
        database(START)
    );
}

/// The bug it catches: removing a database a running job's container still mounts, or keeping old ones forever.
#[tokio::test]
async fn an_old_database_stays_while_a_container_names_it_and_goes_after() {
    let dir = TestDir::new("grype-prune");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut keeper = Keeper::new(dir.0.clone());
    refresh(&mut keeper, &fake, START + 60).await;
    let first = keeper.runner(START + 60).unwrap().generation;

    fake.set(|world| {
        world.listed = Some(START + 86_400);
        world.mounted = Some(BTreeSet::from([first.clone()]));
    });
    refresh(&mut keeper, &fake, START + 86_400 + 60).await;
    let second = keeper.runner(START + 86_400 + 60).unwrap().generation;
    assert_eq!(
        generations(&dir.0),
        BTreeSet::from([first.clone(), second.clone()])
    );

    fake.set(|world| world.mounted = None);
    refresh(&mut keeper, &fake, START + 86_400 + 3600).await;
    assert!(
        generations(&dir.0).contains(&first),
        "kept while Docker cannot say"
    );

    fake.set(|world| world.mounted = Some(BTreeSet::new()));
    refresh(&mut keeper, &fake, START + 86_400 + 7200).await;
    assert_eq!(generations(&dir.0), BTreeSet::from([second]));
}

#[tokio::test]
async fn a_database_older_than_grype_accepts_is_not_mounted() {
    let dir = TestDir::new("grype-stale");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut keeper = Keeper::new(dir.0.clone());
    refresh(&mut keeper, &fake, START + 60).await;
    assert_eq!(
        keeper.until_stale(START + 60),
        Some(Duration::from_secs(GRYPE_MAX_AGE_SECS - 60)),
        "the task wakes when it goes stale"
    );

    let late = START + GRYPE_MAX_AGE_SECS;
    fake.set(|world| world.listed = None);
    keeper.update(&fake, &fake, late).await;
    assert_eq!(
        keeper.runner(late),
        None,
        "a scan downloads its own instead"
    );
    assert_eq!(
        keeper.until_stale(late),
        None,
        "no wake-up at once, again and again"
    );
    let status = keeper.status(late);
    assert!(!status.mounted, "not mounted");
    assert_eq!(
        status.failure.map(|failure| failure.kind),
        Some(GrypeFailureKind::Listing),
        "what kept it from updating comes first"
    );
    fake.set(|world| world.listed = Some(START));
    keeper.update(&fake, &fake, late).await;
    assert_eq!(
        keeper.status(late).failure.map(|failure| failure.kind),
        Some(GrypeFailureKind::Stale),
        "Anchore lists nothing newer"
    );
}

#[tokio::test]
async fn a_second_runnerd_on_the_same_directory_keeps_out() {
    let dir = TestDir::new("grype-two");
    let fake = Fake::default();
    fake.set(|world| {
        world.listed = Some(START);
        world.mounted = Some(BTreeSet::new());
    });
    let mut first = Keeper::new(dir.0.clone());
    refresh(&mut first, &fake, START + 60).await;
    let mut second = Keeper::new(dir.0.clone());
    refresh(&mut second, &fake, START + 60).await;
    assert_eq!(second.runner(START + 60), None);
    assert_eq!(
        second
            .status(START + 60)
            .failure
            .map(|failure| failure.kind),
        Some(GrypeFailureKind::Store)
    );
    assert!(first.runner(START + 60).is_some(), "the first keeps it");
}

/// One step of the generated sequence.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Anchore lists a database built a day later.
    Newer,
    Refresh,
    ListingFails,
    DownloadFails,
    Corrupt,
    ImportFails,
    /// The daemon restarts: a new task on the same directory.
    Restart,
    /// The held database changes on disk while no daemon runs, then it restarts.
    Damage,
    /// A container mounts the generation runners get now, or the last one goes.
    Mount,
    Unmount,
    /// Six hours pass.
    Wait,
}

const STEPS: [Step; 11] = [
    Step::Newer,
    Step::Refresh,
    Step::ListingFails,
    Step::DownloadFails,
    Step::Corrupt,
    Step::ImportFails,
    Step::Restart,
    Step::Damage,
    Step::Mount,
    Step::Unmount,
    Step::Wait,
];

/// Over generated sequences of listings, failures, restarts, damage and mounts: a runner only ever gets a database
/// as Grype imported it from an archive with the listing's checksum, young enough, never one older than it got before
/// unless a check dropped the newer; every generation a container names stays; nothing partial stays in `staging/`.
#[tokio::test]
async fn generated_sequences_never_give_a_runner_an_unchecked_stale_or_vanished_database() {
    for seed in 0..40_u64 {
        let dir = TestDir::new("grype-sequence");
        let fake = Fake::default();
        fake.set(|world| world.mounted = Some(BTreeSet::new()));
        let mut keeper = Keeper::new(dir.0.clone());
        let mut now = START;
        let mut listed = START - 86_400;
        let mut newest_served = 0;
        let mut mounts: Vec<String> = Vec::new();
        let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let mut trace = Vec::new();
        for _ in 0..30 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let step = STEPS[usize::try_from(state >> 33).unwrap() % STEPS.len()];
            trace.push(step);
            let mut dropped = false;
            fake.set(|world| {
                *world = World {
                    listed: Some(listed),
                    mounted: Some(mounts.iter().cloned().collect()),
                    ..World::default()
                }
            });
            match step {
                Step::Newer => {
                    listed += 86_400;
                    fake.set(|world| world.listed = Some(listed));
                }
                Step::Refresh => {}
                Step::ListingFails => fake.set(|world| world.listed = None),
                Step::DownloadFails => fake.set(|world| world.fault = Fault::Download),
                Step::Corrupt => fake.set(|world| world.fault = Fault::Corrupt),
                Step::ImportFails => fake.set(|world| world.fault = Fault::Import),
                Step::Restart => keeper = Keeper::new(dir.0.clone()),
                Step::Damage => {
                    if let Some(runner) = keeper.runner(now) {
                        drop(keeper);
                        let source = PathBuf::from(runner.mount.source.unwrap());
                        std::fs::write(source.join(MODEL_DIR).join(DB_FILE), b"damaged").unwrap();
                        dropped = true;
                    } else {
                        drop(keeper);
                    }
                    keeper = Keeper::new(dir.0.clone());
                }
                Step::Mount => mounts.extend(keeper.runner(now).map(|runner| runner.generation)),
                Step::Unmount => {
                    mounts.pop();
                }
                Step::Wait => now += 6 * 3600,
            }
            fake.set(|world| world.mounted = Some(mounts.iter().cloned().collect()));
            if dropped {
                newest_served = 0;
            }
            keeper.prepare(now).await;
            keeper.update(&fake, &fake, now).await;
            let context = format!("seed {seed}: {trace:?}");
            newest_served = holds(
                &keeper,
                &dir.0,
                &mounts,
                now,
                listed,
                newest_served,
                &context,
            );
        }
    }
}

const CONFIG: &str = r#"
[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "app"
cpus = 4
memory = "4G"
slots = 1

[[repositories]]
name = "owner/app"
profile = "app"
labels = ["self-hosted", "app"]
"#;

/// Asserts the sequence's invariants after a refresh at `now`, `listed` being the newest database listed so far and
/// `newest_served` the newest a runner got; returns the newest a runner gets now.
fn holds(
    keeper: &Keeper,
    dir: &Path,
    mounts: &[String],
    now: u64,
    listed: u64,
    newest_served: u64,
    context: &str,
) -> u64 {
    let on_disk = generations(dir);
    for mounted in mounts {
        assert!(
            on_disk.contains(mounted),
            "{context}: {mounted} went while mounted"
        );
    }
    assert!(
        staging(dir).is_empty(),
        "{context}: staging holds {:?}",
        staging(dir)
    );
    let Some(runner) = keeper.runner(now) else {
        let status = keeper.status(now);
        assert!(!status.mounted, "{context}");
        assert!(
            status
                .held
                .is_none_or(|held| now >= held.built_at + GRYPE_MAX_AGE_SECS),
            "{context}: a fresh database held but not served"
        );
        return newest_served;
    };
    let built: u64 = runner
        .generation
        .split('-')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(mounted_database(&runner), database(built), "{context}");
    assert!(now < built + GRYPE_MAX_AGE_SECS, "{context}: stale");
    assert!(
        built >= newest_served,
        "{context}: went back to an older one"
    );
    assert!(built <= listed, "{context}: one never listed");
    built
}

#[test]
fn turned_off_by_a_reload_runners_get_no_database_and_clients_see_none() {
    let dir = TestDir::new("grype-link");
    let on = Config::parse(CONFIG).unwrap();
    let link = Link::new(Some(dir.0.clone()), &on);
    link.serve(
        &dir.0.join("dbs/1795415312-3d24f57cd358"),
        "1795415312-3d24f57cd358",
    );
    assert!(link.runner().is_some(), "served while on");
    let off = Config::parse(&CONFIG.replace(
        "[profiles.app]",
        "grype = { enabled = false }\n\n[profiles.app]",
    ))
    .unwrap();
    link.reload(&off);
    assert_eq!(link.runner(), None, "withdrawn at once");
    assert_eq!(link.status(), None);
    link.reload(&on);
    assert_eq!(link.runner(), None, "not before the task checks it again");
}

/// Serves `routes` (path to status and body) on a loopback port in place of Anchore's CDN.
async fn fake_anchore(routes: Vec<(&'static str, u16, Vec<u8>)>) -> String {
    use axum::response::IntoResponse;
    let routes = Arc::new(routes);
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let routes = routes.clone();
        async move {
            match routes.iter().find(|(path, ..)| *path == uri.path()) {
                Some((_, status, body)) => (
                    axum::http::StatusCode::from_u16(*status).unwrap(),
                    body.clone(),
                )
                    .into_response(),
                None => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/databases", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// The bug it catches: an archive hashed other than as it arrived, one past the cap kept, or an error page taken for a
/// listing.
#[tokio::test]
async fn anchore_s_listing_and_archive_are_read_beside_each_other_and_capped() {
    let url = fake_anchore(vec![
        (
            "/databases/v6/latest.json",
            200,
            b"{\"status\":\"active\"}".to_vec(),
        ),
        ("/databases/v6/db.tar.zst", 200, archive(START)),
        ("/databases/v6/gone.tar.zst", 404, b"not here".to_vec()),
    ])
    .await;
    let anchore = Anchore::new(&url).unwrap();
    assert_eq!(anchore.listing().await.unwrap(), b"{\"status\":\"active\"}");
    let dir = TestDir::new("grype-anchore");
    let to = dir.0.join("db.tar.zst");
    assert_eq!(
        anchore.archive("db.tar.zst", &to, 1 << 20).await.unwrap(),
        Downloaded {
            sha256: sha256(&archive(START)),
            bytes: archive(START).len() as u64,
        }
    );
    assert_eq!(std::fs::read(&to).unwrap(), archive(START));
    let error = anchore
        .archive("db.tar.zst", &dir.0.join("small.tar.zst"), 8)
        .await
        .unwrap_err();
    assert!(error.contains("larger than 8 bytes"), "{error}");
    let error = anchore
        .archive("gone.tar.zst", &dir.0.join("gone.tar.zst"), 1 << 20)
        .await
        .unwrap_err();
    assert!(error.contains("404"), "{error}");

    let down = fake_anchore(vec![("/databases/v6/latest.json", 503, Vec::new())]).await;
    let error = Anchore::new(&down).unwrap().listing().await.unwrap_err();
    assert!(error.contains("503"), "{error}");
}

/// The bug it catches: an import that could reach the network, write outside its work directory, or run as root
/// and leave files the daemon cannot publish.
#[test]
fn grype_imports_without_a_network_as_the_daemon_s_user_into_its_work_directory_only() {
    let body = sources::import_body(
        Path::new("/cache/runnerd/grype/staging/work-0"),
        &["db", "import", "/work/db.tar.zst"],
        "1000:1000",
        false,
    );
    assert_eq!(body.image.as_deref(), Some(IMAGE));
    assert_eq!(body.user.as_deref(), Some("1000:1000"));
    let env = body.env.unwrap();
    for setting in [
        "GRYPE_DB_CACHE_DIR=/work/db",
        "GRYPE_DB_AUTO_UPDATE=false",
        "GRYPE_CHECK_FOR_APP_UPDATE=false",
        "TMPDIR=/work/tmp",
    ] {
        assert!(env.iter().any(|set| set == setting), "{setting}: {env:?}");
    }
    let host = body.host_config.unwrap();
    assert_eq!(host.network_mode.as_deref(), Some("none"));
    assert_eq!(host.readonly_rootfs, Some(true));
    assert_eq!(host.cap_drop, Some(vec!["ALL".to_owned()]));
    let mounts = host.mounts.unwrap();
    assert_eq!(mounts.len(), 1, "{mounts:?}");
    assert_eq!(
        mounts[0].source.as_deref(),
        Some("/cache/runnerd/grype/staging/work-0")
    );
    assert_eq!(mounts[0].target.as_deref(), Some("/work"));
}

#[test]
fn grype_s_status_is_read_from_its_output_whatever_it_logs_around_it() {
    // `grype db status -o json` of grype 0.122.1, with a warning before it.
    let output = "[0000]  WARN something about the config\n{\n \"schemaVersion\": \"v6.1.10\",\n \"from\": \"manual import\",\n \"built\": \"2026-11-23T06:28:32Z\",\n \"path\": \"/work/db/6/vulnerability.db\",\n \"valid\": true\n}\n";
    let json = sources::json_object(output).unwrap();
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(value["built"], "2026-11-23T06:28:32Z");
    assert_eq!(sources::json_object("no json here"), None);
}

//! What the Grype database task knows besides its store (ADR 0160), and the steps of a refresh: open and check the
//! store, prune, read the listing, download, check the SHA-256, import and publish. Only `Keeper`'s methods write its
//! fields, which are independent: whether the store is open, what last failed, when the listing was last read, and
//! which archive is held back after its fetch failed.
//!
//! Nothing here survives a restart but the store's own files: the next task opens and checks the store again and
//! reads the listing at once. A reload that turns the database off leaves it as it is.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{info, warn};

use super::listing::{self, Listing};
use super::store::{Checked, GrypeStore, Held, Publishing};
use super::{ARCHIVE_LIMIT, Grype, REFRESH, RETRY, RunnerGrype, Source, mountable, stale_at};
use crate::api::{self, GrypeFailure, GrypeFailureKind};
use crate::sync::lock;

/// The task's state.
#[derive(Debug)]
pub(super) struct Keeper {
    directory: PathBuf,
    /// Shared with the blocking work that hashes and writes; `None` until it opens, which each refresh tries again.
    store: Option<Arc<Mutex<GrypeStore>>>,
    /// What failed and has not succeeded since.
    failure: Option<GrypeFailure>,
    /// When the listing was last read.
    checked_at: Option<u64>,
    /// The archive whose fetch failed after it was downloaded, not fetched again until a day passes or the listing
    /// names another.
    held_back: Option<HeldBack>,
}

/// An archive whose fetch failed after it cost a download.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldBack {
    /// The archive's SHA-256, as the listing named it.
    sha256: String,
    /// When it may be fetched again.
    until: u64,
}

/// How long an archive whose fetch failed after its download is held back: a day, four refreshes.
const HOLD_BACK: Duration = Duration::from_hours(24);

/// Why an update stopped, and in what words.
struct Failed {
    kind: GrypeFailureKind,
    detail: String,
}

impl Failed {
    fn new(kind: GrypeFailureKind, detail: impl Into<String>) -> Failed {
        Failed {
            kind,
            detail: detail.into(),
        }
    }
}

impl Keeper {
    /// The task's state for a store at `directory`, not yet open.
    pub(super) fn new(directory: PathBuf) -> Keeper {
        Keeper {
            directory,
            store: None,
            failure: None,
            checked_at: None,
            held_back: None,
        }
    }

    /// Opens the store if it is not open, and then checks the database it holds, once: a store that cannot open is
    /// a `Store` failure until a later refresh opens it. Before it opens, the containers an import of an earlier
    /// daemon left are removed, so none writes into `staging/` as it is emptied.
    pub(super) async fn prepare<G: Grype>(&mut self, grype: &G, now: u64) {
        if self.store.is_some() {
            return;
        }
        if let Err(error) = grype.clean().await {
            warn!("cannot remove what an earlier Grype import left: {error}");
        }
        let directory = self.directory.clone();
        let opened = tokio::task::spawn_blocking(move || {
            let mut store = GrypeStore::open(&directory)?;
            let checked = store.check()?;
            Ok::<_, std::io::Error>((store, checked))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|opened| opened);
        match opened {
            Ok((store, checked)) => {
                if let Checked::Dropped(problem) = &checked {
                    warn!("the Grype database held was dropped: {problem}");
                    self.fail(Failed::new(GrypeFailureKind::Store, problem.clone()), now);
                } else if self
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.kind == GrypeFailureKind::Store)
                {
                    self.failure = None;
                }
                self.store = Some(Arc::new(Mutex::new(store)));
            }
            Err(error) => {
                warn!(
                    "cannot open the Grype database at {}: {error}",
                    self.directory.display()
                );
                self.fail(Failed::new(GrypeFailureKind::Store, error.to_string()), now);
            }
        }
    }

    /// Removes the generations neither held nor named by a container, then reads the listing and, when it names a
    /// database built after the held one, downloads, checks, imports and publishes it. A generation is so removed a
    /// refresh after it was replaced, once every start that read it has its container. Returns how long until the
    /// next refresh. Whatever fails keeps the held database; an archive that failed after its download is held back
    /// a day, unless the listing names another.
    pub(super) async fn update<S: Source, G: Grype>(
        &mut self,
        source: &S,
        grype: &G,
        now: u64,
    ) -> Duration {
        let Some(store) = self.store.clone() else {
            return RETRY;
        };
        if let Err(error) = prune(&store, grype).await {
            warn!("old Grype databases stay: {error}");
        }
        let listing = match source.listing().await {
            Ok(bytes) => listing::parse(&bytes)
                .map_err(|error| Failed::new(GrypeFailureKind::Listing, error)),
            Err(error) => Err(Failed::new(GrypeFailureKind::Listing, error)),
        };
        let listing = match listing {
            Ok(listing) => listing,
            Err(failed) => {
                self.fail(failed, now);
                return RETRY;
            }
        };
        self.checked_at = Some(now);
        let held = lock(&store).held().cloned();
        if held
            .as_ref()
            .is_none_or(|held| listing.built_at > held.built_at)
        {
            if self
                .held_back
                .as_ref()
                .is_some_and(|back| back.sha256 == listing.sha256 && now < back.until)
            {
                return REFRESH;
            }
            // Nothing is downloaded that Grype could not import.
            if let Err(error) = grype.ready().await {
                self.fail(Failed::new(GrypeFailureKind::Import, error), now);
                return RETRY;
            }
            match fetch(&store, source, grype, &listing).await {
                Ok(held) => info!(
                    "Grype database {} built at {} is the one runners mount",
                    held.schema, held.built_at
                ),
                Err(failed) => {
                    warn!(
                        "the Grype database built at {} is not kept: {}",
                        listing.built_at, failed.detail
                    );
                    if failed.kind != GrypeFailureKind::Download {
                        self.held_back = Some(HeldBack {
                            sha256: listing.sha256.clone(),
                            until: now + HOLD_BACK.as_secs(),
                        });
                    }
                    self.fail(failed, now);
                    return RETRY;
                }
            }
        }
        self.failure = None;
        self.held_back = None;
        REFRESH
    }

    /// What a runner gets now: the held database while it is younger than Grype accepts.
    pub(super) fn runner(&self, now: u64) -> Option<RunnerGrype> {
        let store = self.store.as_ref()?;
        let store = lock(store);
        let held = store.held()?;
        fresh(held, now).then(|| {
            RunnerGrype::of(
                &store.generation_dir(&held.generation),
                &held.generation,
                held.built_at,
            )
        })
    }

    /// How long until the held database grows older than Grype accepts; `None` when none is held or it already has.
    pub(super) fn until_stale(&self, now: u64) -> Option<Duration> {
        let store = lock(self.store.as_ref()?);
        let stale_at = stale_at(store.held()?.built_at);
        (stale_at > now).then(|| Duration::from_secs(stale_at - now))
    }

    /// What clients show.
    pub(super) fn status(&self, now: u64) -> api::GrypeDatabase {
        let held = self
            .store
            .as_ref()
            .and_then(|store| lock(store).held().cloned());
        let stale = held
            .as_ref()
            .filter(|held| !fresh(held, now))
            .map(|held| GrypeFailure {
                kind: GrypeFailureKind::Stale,
                detail: String::new(),
                since: stale_at(held.built_at),
            });
        api::GrypeDatabase {
            mounted: held.as_ref().is_some_and(|held| fresh(held, now)),
            held: held.map(|held| api::HeldGrypeDatabase {
                built_at: held.built_at,
                schema: held.schema,
                bytes: held.bytes,
            }),
            checked_at: self.checked_at,
            failure: self.failure.clone().or(stale),
        }
    }

    /// Notes `failed`, keeping when a failure of its kind first began.
    fn fail(&mut self, failed: Failed, now: u64) {
        let since = self
            .failure
            .as_ref()
            .filter(|failure| failure.kind == failed.kind)
            .map_or(now, |failure| failure.since);
        self.failure = Some(GrypeFailure {
            kind: failed.kind,
            detail: failed.detail,
            since,
        });
    }
}

/// Whether `held` is given to a starting runner at `now`.
fn fresh(held: &Held, now: u64) -> bool {
    mountable(held.built_at, now)
}

/// Downloads the archive `listing` names into a new work directory, checks its SHA-256, has Grype import and check
/// it, and publishes it. The work directory goes either way.
async fn fetch<S: Source, G: Grype>(
    store: &Arc<Mutex<GrypeStore>>,
    source: &S,
    grype: &G,
    listing: &Listing,
) -> Result<Held, Failed> {
    let store_error =
        |error: std::io::Error| Failed::new(GrypeFailureKind::Store, error.to_string());
    let work = blocking(store, |store| store.work())
        .await
        .map_err(store_error)?;
    let fetched = fetch_into(store, source, grype, listing, &work).await;
    let discarded = blocking(store, {
        let work = work.clone();
        move |store| store.discard(&work)
    })
    .await;
    let held = fetched?;
    discarded.map_err(store_error)?;
    Ok(held)
}

async fn fetch_into<S: Source, G: Grype>(
    store: &Arc<Mutex<GrypeStore>>,
    source: &S,
    grype: &G,
    listing: &Listing,
    work: &Path,
) -> Result<Held, Failed> {
    let downloaded = source
        .archive(&listing.path, &work.join(&listing.path), ARCHIVE_LIMIT)
        .await
        .map_err(|error| Failed::new(GrypeFailureKind::Download, error))?;
    if downloaded.sha256 != listing.sha256 {
        return Err(Failed::new(
            GrypeFailureKind::Checksum,
            format!(
                "sha256:{} where the listing says sha256:{}",
                downloaded.sha256, listing.sha256
            ),
        ));
    }
    let imported = grype
        .import(work, &listing.path)
        .await
        .map_err(|error| Failed::new(GrypeFailureKind::Import, error))?;
    // What Grype imported is the database the listing named.
    if imported.built_at != listing.built_at || imported.schema != listing.schema {
        return Err(Failed::new(
            GrypeFailureKind::Import,
            format!(
                "Grype says it holds {} built at {}, the listing {} built at {}",
                imported.schema, imported.built_at, listing.schema, listing.built_at
            ),
        ));
    }
    let publishing = Publishing {
        built_at: imported.built_at,
        schema: imported.schema,
        archive: listing.path.clone(),
        archive_sha256: downloaded.sha256,
    };
    let work = work.to_owned();
    blocking(store, move |store| store.publish(&work, &publishing))
        .await
        .map_err(|error| Failed::new(GrypeFailureKind::Store, error.to_string()))
}

/// Removes the generations neither held nor named by a container's label. Nothing when Docker cannot say which
/// containers mount one.
async fn prune<G: Grype>(store: &Arc<Mutex<GrypeStore>>, grype: &G) -> Result<(), String> {
    let mounted = grype.mounted().await?;
    let removed = blocking(store, move |store| store.prune(&mounted))
        .await
        .map_err(|error| error.to_string())?;
    if !removed.is_empty() {
        info!(
            "removed Grype databases no runner mounts: {}",
            removed.join(", ")
        );
    }
    Ok(())
}

/// Runs `work` on the store on a blocking thread.
async fn blocking<T: Send + 'static>(
    store: &Arc<Mutex<GrypeStore>>,
    work: impl FnOnce(&mut GrypeStore) -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || work(&mut lock(&store)))
        .await
        .map_err(std::io::Error::other)?
}

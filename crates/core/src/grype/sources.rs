//! What the Grype database task reads and runs (ADR 0160): `Anchore`, which downloads the listing and the archive it
//! names from Anchore's CDN, without a credential, hashing the archive as it arrives; and `Importer`, which runs
//! Grype's own `db import` and `db status` on a work directory in a container of the pinned Grype image, without a
//! network, and asks Docker which generations runner containers mount. Neither decides anything.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use super::store::{WORK_DB, WORK_TMP, hex};
use super::{Downloaded, Grype, IMAGE, Imported, LABEL, Source};
use crate::docker::{DockerApi, is_not_found};

/// Anchore's databases, as Grype's `distribution.DefaultConfig` names them.
pub const ANCHORE: &str = "https://grype.anchore.io/databases";

/// How long the listing's request may take, as Grype's `update-available-timeout`.
const LISTING_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the archive's download may take: Grype's `update-download-timeout` is 5 minutes, for a 200 MB archive.
const ARCHIVE_TIMEOUT: Duration = Duration::from_mins(15);
/// The largest listing read: Anchore's is a few hundred bytes.
const LISTING_LIMIT: usize = 64 << 10;

/// Anchore's CDN, over HTTPS, anonymously.
#[derive(Debug, Clone)]
pub struct Anchore {
    listing: reqwest::Client,
    archive: reqwest::Client,
    base: String,
}

impl Anchore {
    /// The client of Anchore's databases at `base` (`ANCHORE`, or a test's server).
    pub fn new(base: &str) -> Result<Anchore, String> {
        crate::github::install_crypto_provider();
        let client = |timeout| {
            reqwest::Client::builder()
                .user_agent(concat!("runnerd/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::limited(3))
                .timeout(timeout)
                .build()
                .map_err(|error| error.to_string())
        };
        Ok(Anchore {
            listing: client(LISTING_TIMEOUT)?,
            archive: client(ARCHIVE_TIMEOUT)?,
            base: base.trim_end_matches('/').to_owned(),
        })
    }
}

impl Source for Anchore {
    async fn listing(&self) -> Result<Vec<u8>, String> {
        let url = format!("{}/{}/latest.json", self.base, super::listing::MODEL);
        let mut response = self
            .listing
            .get(&url)
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("Anchore answered {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url().to_string())?
        {
            body.extend_from_slice(&chunk);
            if body.len() > LISTING_LIMIT {
                return Err(format!("the listing is larger than {LISTING_LIMIT} bytes"));
            }
        }
        Ok(body)
    }

    async fn archive(&self, path: &str, to: &Path, limit: u64) -> Result<Downloaded, String> {
        let url = format!("{}/{}/{path}", self.base, super::listing::MODEL);
        let mut response = self
            .archive
            .get(&url)
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        if !response.status().is_success() {
            return Err(format!("Anchore answered {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(format!("the archive is larger than {limit} bytes"));
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)
            .map_err(|error| error.to_string())?;
        let mut file = tokio::fs::File::from_std(file);
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        let mut bytes: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| error.without_url().to_string())?
        {
            bytes += chunk.len() as u64;
            if bytes > limit {
                return Err(format!("the archive is larger than {limit} bytes"));
            }
            context.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        file.flush().await.map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        Ok(Downloaded {
            sha256: hex(context.finish().as_ref()),
            bytes,
        })
    }
}

/// Where the import container sees its work directory.
const WORK: &str = "/work";
/// How long Grype's import of a database may take, and its check.
const IMPORT_DEADLINE: Duration = Duration::from_mins(20);
const STATUS_DEADLINE: Duration = Duration::from_mins(5);
/// The import's memory: SQLite builds the database's indexes in it.
const IMPORT_MEMORY: i64 = 3 << 30;
/// The import container's name, and the label every container the importer makes carries.
const IMPORT_NAME: &str = "runnerd-grype-import";
const STATUS_NAME: &str = "runnerd-grype-status";
const IMPORT_LABEL: &str = "runnerd.grype-import";

/// Grype in a container of the pinned image, on Docker.
#[derive(Debug)]
pub struct Importer<D> {
    docker: Arc<D>,
    /// `uid:gid` the import runs as: the daemon's, so it owns what Grype writes.
    user: String,
}

impl<D> Importer<D> {
    /// Runs Grype on `docker` as the daemon's user.
    pub fn new(docker: Arc<D>) -> Importer<D> {
        let user = format!(
            "{}:{}",
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw()
        );
        Importer { docker, user }
    }
}

/// `grype db status -o json`, as `vulnerability.ProviderStatus` writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    schema_version: String,
    #[serde(default)]
    built: String,
    #[serde(default)]
    valid: bool,
    #[serde(default)]
    error: String,
}

impl<D: DockerApi> Importer<D> {
    /// Runs Grype with `args` on `work`, mounted at `/work`, under the name `name`, within `deadline`: its exit code
    /// and output. Removes its container afterwards, and one an earlier daemon left.
    async fn grype(
        &self,
        name: &str,
        work: &Path,
        args: &[&str],
        read_only: bool,
        deadline: Duration,
    ) -> Result<(i64, String), String> {
        let docker = &*self.docker;
        let remove = async || match docker.remove(name).await {
            Err(error) if !is_not_found(&error) => Err(format!("cannot remove {name}: {error}")),
            _ => Ok(()),
        };
        remove().await?;
        if !docker
            .image_present(IMAGE)
            .await
            .map_err(|error| error.to_string())?
        {
            docker
                .pull(IMAGE)
                .await
                .map_err(|error| format!("cannot pull Grype's image: {error}"))?;
        }
        let ran = tokio::time::timeout(deadline, async {
            docker
                .run(name, import_body(work, args, &self.user, read_only))
                .await?;
            let code = docker.wait(name).await?;
            let output = docker.logs(name).await?;
            Ok::<_, crate::docker::Error>((code, output))
        })
        .await;
        // Also when the deadline cut it off: no import outlives its refresh.
        remove().await?;
        match ran {
            Err(_) => Err(format!(
                "Grype did not finish within {}",
                crate::api::duration(deadline.as_secs())
            )),
            Ok(Err(error)) => Err(error.to_string()),
            Ok(Ok(done)) => Ok(done),
        }
    }
}

impl<D: DockerApi + 'static> Grype for Importer<D> {
    async fn import(&self, work: &Path, archive: &str) -> Result<Imported, String> {
        let archive = format!("{WORK}/{archive}");
        let (code, output) = self
            .grype(
                IMPORT_NAME,
                work,
                &["db", "import", &archive],
                false,
                IMPORT_DEADLINE,
            )
            .await?;
        if code != 0 {
            return Err(format!(
                "grype db import exited {code}: {}",
                last_line(&output)
            ));
        }
        // Grype's own check of what it imported: its schema, its age and its digest.
        let (code, output) = self
            .grype(
                STATUS_NAME,
                work,
                &["db", "status", "-o", "json"],
                true,
                STATUS_DEADLINE,
            )
            .await?;
        let status = json_object(&output)
            .and_then(|json| serde_json::from_str::<Status>(json).ok())
            .ok_or_else(|| format!("grype db status exited {code}: {}", last_line(&output)))?;
        if code != 0 || !status.valid || !status.error.is_empty() {
            return Err(format!(
                "grype db status says it is invalid: {}",
                if status.error.is_empty() {
                    last_line(&output)
                } else {
                    &status.error
                }
            ));
        }
        let built_at = crate::time::parse_ms(&status.built)
            .ok_or_else(|| format!("grype db status gave no build time: {}", status.built))?
            / 1000;
        Ok(Imported {
            built_at,
            schema: status.schema_version,
        })
    }

    async fn mounted(&self) -> Result<BTreeSet<String>, String> {
        let containers = self
            .docker
            .list(LABEL)
            .await
            .map_err(|error| error.to_string())?;
        Ok(containers
            .into_iter()
            .filter_map(|container| container.labels.get(LABEL).cloned())
            .collect())
    }
}

/// The container that runs Grype with `args` on the work directory `work`: as `user`, without a network,
/// capabilities or new privileges, on a read-only root, with Grype's cache and temporary directories in `work`.
pub(crate) fn import_body(
    work: &Path,
    args: &[&str],
    user: &str,
    read_only: bool,
) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(IMAGE.to_owned()),
        cmd: Some(args.iter().map(|arg| (*arg).to_owned()).collect()),
        env: Some(vec![
            format!("GRYPE_DB_CACHE_DIR={WORK}/{WORK_DB}"),
            "GRYPE_DB_AUTO_UPDATE=false".to_owned(),
            "GRYPE_CHECK_FOR_APP_UPDATE=false".to_owned(),
            "GRYPE_LOG_LEVEL=error".to_owned(),
            format!("TMPDIR={WORK}/{WORK_TMP}"),
            format!("HOME={WORK}/{WORK_TMP}"),
        ]),
        user: Some(user.to_owned()),
        labels: Some(HashMap::from([(IMPORT_LABEL.to_owned(), String::new())])),
        host_config: Some(HostConfig {
            memory: Some(IMPORT_MEMORY),
            memory_swap: Some(IMPORT_MEMORY),
            nano_cpus: Some(2_000_000_000),
            network_mode: Some("none".to_owned()),
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".to_owned()]),
            security_opt: Some(vec!["no-new-privileges".to_owned()]),
            mounts: Some(vec![Mount {
                target: Some(WORK.to_owned()),
                source: Some(work.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(read_only),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The JSON object `output` holds: from its first line that opens one to its last that closes one, as Grype indents
/// what `-o json` writes, whatever it logs around it.
pub(crate) fn json_object(output: &str) -> Option<&str> {
    let start = output
        .match_indices('{')
        .map(|(at, _)| at)
        .find(|&at| at == 0 || output[..at].ends_with('\n'))?;
    let end = output.rfind("\n}").map(|at| at + 2)?;
    (end > start).then(|| &output[start..end])
}

/// The last line of `output` that says something, or `no output`.
fn last_line(output: &str) -> &str {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
}

//! The Docker Hub mirror's storage: finding the links in it whose blob is gone or empty, removing them before the
//! mirror starts, and the doctor's check of a running mirror (docs/adr/0144). It reaches the storage only through a
//! container of the mirror's image, never the host's view of the volume; it writes only while no mirror runs, and
//! only reads a running one's.

use std::collections::HashMap;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig};
use futures_util::TryStreamExt as _;

use super::{MIRROR_IMAGE, Names, volume};
use crate::api::{self, Check, CheckStatus};
use crate::docker::{DockerApi, Error, is_not_found};

/// What reads the mirror's links, in the image's busybox `sh`: `repair` removes those whose blob is gone, `check` only
/// lists them.
const LINKS_SCRIPT: &str = include_str!("mirror_links.sh");

/// The registry's setting for its blob descriptor cache. runnerd sets it empty, which turns the cache off: the
/// image's config keeps it in memory, where it goes on saying a blob is stored after the proxy deleted it.
pub(super) const DESCRIPTOR_CACHE_ENV: &str = "REGISTRY_STORAGE_CACHE_BLOBDESCRIPTOR";

/// How long the repair before a start may take: a few thousand links take well under a second.
const REPAIR_DEADLINE: Duration = Duration::from_secs(60);

/// The memory the repair container may use: it only reads small files and removes some.
const REPAIR_MEMORY: i64 = 64 << 20;

/// The mirror's storage as its container sees it.
const V2: &str = "/var/lib/registry/docker/registry/v2";

/// How many of the links removed or found the log line and the doctor name.
const NAMED: usize = 3;

/// What is wrong with a link in the mirror's storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Its blob's data is missing: the registry fetches the blob again from Docker Hub, unless its blob descriptor
    /// cache still holds it, and then answers with no data.
    Gone,
    /// Its blob's data is empty: the registry answers with no data, and keeps the empty file when it fetches the
    /// blob again. The data goes with the link.
    Empty,
    /// It holds no digest.
    Garbled,
}

impl Fault {
    /// Whether the mirror answers a pull of it with no data: always for an empty blob, and for a missing one while
    /// the mirror keeps the blob descriptor cache.
    fn serves_nothing(self, cached: bool) -> bool {
        match self {
            Fault::Empty => true,
            Fault::Gone => cached,
            Fault::Garbled => false,
        }
    }
}

/// A link in the mirror's storage whose blob is gone or empty, or that holds no digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangling {
    /// What is wrong with it.
    pub fault: Fault,
    /// The repository the link is in, such as `library/caddy`.
    pub repository: String,
    /// The digest the link holds, such as `sha256:881f…`; `None` when it holds none.
    pub digest: Option<String>,
    /// The link's path under the storage's `docker/registry/v2`.
    pub link: String,
}

impl Dangling {
    /// What a repair removes for it, as paths in the mirror's container: the link, and an empty blob's data.
    fn removes(&self) -> Vec<String> {
        let mut paths = vec![format!("{V2}/{}", self.link)];
        if let (Fault::Empty, Some((algorithm, hex))) = (
            self.fault,
            self.digest
                .as_deref()
                .and_then(|digest| digest.split_once(':')),
        ) && let Some(prefix) = hex.get(..2)
        {
            paths.push(format!("{V2}/blobs/{algorithm}/{prefix}/{hex}/data"));
        }
        paths
    }
}

impl std::fmt::Display for Dangling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.digest {
            Some(digest) => write!(f, "{}@{digest}", self.repository)?,
            None => write!(f, "{} ({})", self.repository, self.link)?,
        }
        if self.fault == Fault::Empty {
            f.write_str(" (empty)")?;
        }
        Ok(())
    }
}

/// What a read of the mirror's links found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Links {
    /// How many links it read.
    pub count: usize,
    /// Those whose blob is gone or empty, in the order it read them.
    pub dangling: Vec<Dangling>,
}

impl Links {
    /// What `mirror_links.sh` printed; `None` when it was cut off before its last line.
    fn parse(output: &str) -> Option<Links> {
        let mut dangling = Vec::new();
        let mut count = None;
        for line in output.lines() {
            let fault = match line.split_once(' ') {
                Some(("gone", rest)) => Some((Fault::Gone, rest)),
                Some(("empty", rest)) => Some((Fault::Empty, rest)),
                Some(("garbled", rest)) => Some((Fault::Garbled, rest)),
                _ => None,
            };
            if let Some((fault, found)) = fault {
                let Some((digest, link)) = found.split_once(' ') else {
                    continue;
                };
                dangling.push(Dangling {
                    fault,
                    repository: repository_of(link),
                    digest: (digest != "-").then(|| digest.to_owned()),
                    link: link.to_owned(),
                });
            } else if let Some(read) = line.strip_prefix("links ") {
                count = read.trim().parse().ok();
            }
        }
        Some(Links {
            count: count?,
            dangling,
        })
    }
}

/// The first few of `dangling`, and how many more.
fn named(dangling: &[&Dangling]) -> String {
    let mut named: Vec<String> = dangling
        .iter()
        .take(NAMED)
        .map(ToString::to_string)
        .collect();
    if dangling.len() > NAMED {
        named.push(format!("{} more", dangling.len() - NAMED));
    }
    named.join(", ")
}

/// The repository a link is in: its path between `repositories/` and `/_layers/` or `/_manifests/`.
fn repository_of(link: &str) -> String {
    let path = link.strip_prefix("repositories/").unwrap_or(link);
    ["/_layers/", "/_manifests/"]
        .iter()
        .find_map(|marker| path.find(marker).map(|at| &path[..at]))
        .unwrap_or(path)
        .to_owned()
}

/// What the repair before a start came to. It never keeps the mirror from starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    /// Every link leads to a stored blob, or there are none, as in a new volume.
    Clean,
    /// It removed these links.
    Removed(Links),
    /// It could not read or remove them all, with why; those left are found again at the next start.
    Failed(String),
}

impl Repair {
    /// Logs `line`: a failure as a warning.
    pub fn log(&self) {
        match (self, self.line()) {
            (Repair::Failed(_), Some(line)) => tracing::warn!("{line}"),
            (_, Some(line)) => tracing::info!("{line}"),
            (_, None) => {}
        }
    }

    /// The line the daemon logs for it; none when there was nothing to repair.
    pub fn line(&self) -> Option<String> {
        match self {
            Repair::Clean => None,
            Repair::Removed(links) => Some(format!(
                "removed {} of the mirror's {} links whose blob is gone, before starting it: {}",
                links.dangling.len(),
                links.count,
                named(&links.dangling.iter().collect::<Vec<_>>())
            )),
            Repair::Failed(why) => Some(format!(
                "cannot repair the mirror's storage, starting it as it is: {why}"
            )),
        }
    }
}

/// What `mirror_up` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorUp {
    /// It ran already: its storage is left as it is.
    Running,
    /// It started, after the repair of its storage.
    Started(Repair),
}

/// Removes the mirror's links whose blob is gone or empty, in a container of the mirror's image that mounts its
/// storage. Call only while no mirror runs. A repair container an earlier daemon left, as a restart while it ran
/// leaves it, is removed first; the repair reads every link again, so one cut off is done over.
pub(super) async fn repair(docker: &impl DockerApi, names: &Names, slice: &str) -> Repair {
    let name = names.mirror_repair();
    let repaired = tokio::time::timeout(REPAIR_DEADLINE, async {
        remove_left(docker, &name).await?;
        docker.run(&name, repair_body(names, slice)).await?;
        let code = docker.wait(&name).await?;
        let output = docker.logs(&name).await?;
        Ok::<_, Error>((code, output))
    })
    .await;
    // Also when the deadline cut it off: no repair runs beside the mirror.
    let removed = remove_left(docker, &name).await;
    let (code, output) = match repaired {
        Err(_) => {
            return Repair::Failed(format!(
                "it did not finish within {}",
                api::duration(REPAIR_DEADLINE.as_secs())
            ));
        }
        Ok(Err(error)) => return Repair::Failed(error.to_string()),
        Ok(Ok(done)) => done,
    };
    if let Err(error) = removed {
        return Repair::Failed(format!("cannot remove its container: {error}"));
    }
    match Links::parse(&output) {
        Some(links) if code == 0 && links.dangling.is_empty() => Repair::Clean,
        Some(links) if code == 0 => Repair::Removed(links),
        _ => Repair::Failed(format!("it exited {code}: {}", output.trim())),
    }
}

/// Removes the container `name` if there is one.
async fn remove_left(docker: &impl DockerApi, name: &str) -> Result<(), Error> {
    match docker.remove(name).await {
        Err(error) if !is_not_found(&error) => Err(error),
        _ => Ok(()),
    }
}

/// The repair container: `mirror_links.sh repair` on the mirror's storage, without a network.
pub(super) fn repair_body(names: &Names, slice: &str) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(MIRROR_IMAGE.to_owned()),
        entrypoint: Some(script("repair")),
        labels: Some(HashMap::from([(
            "runnerd.mirror-repair".to_owned(),
            String::new(),
        )])),
        host_config: Some(HostConfig {
            cgroup_parent: Some(slice.to_owned()),
            memory: Some(REPAIR_MEMORY),
            memory_swap: Some(REPAIR_MEMORY),
            nano_cpus: Some(1_000_000_000),
            mounts: Some(vec![volume(names.mirror(), "/var/lib/registry", false)]),
            network_mode: Some("none".to_owned()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `mirror_links.sh` in `mode`, as a command for the image's `sh`.
fn script(mode: &str) -> Vec<String> {
    ["/bin/sh", "-c", LINKS_SCRIPT, "mirror-links", mode]
        .map(str::to_owned)
        .to_vec()
}

/// What the doctor found of the mirror's storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The mirror is not running: runnerd repairs its storage before it starts it.
    NotRunning,
    /// It runs, and its links read so.
    Read {
        /// Its links.
        links: Links,
        /// Whether it keeps the blob descriptor cache, as a mirror an older runnerd started does.
        cached: bool,
    },
    /// It could not be read, with why.
    Unread(String),
}

/// Reads a running mirror's links, without changing them, for the doctor.
pub async fn check(docker: &impl DockerApi, names: &Names) -> Found {
    match docker.inspect(&names.mirror()).await {
        Ok(state) if state.running => {}
        Ok(_) => return Found::NotRunning,
        Err(error) if is_not_found(&error) => return Found::NotRunning,
        Err(error) => return Found::Unread(error.to_string()),
    }
    let output = match docker.exec(&names.mirror(), script("check")).await {
        Ok(output) => output.try_concat().await,
        Err(error) => Err(error),
    };
    let output = match output {
        Ok(output) => String::from_utf8_lossy(&output).into_owned(),
        Err(error) => return Found::Unread(error.to_string()),
    };
    let Some(links) = Links::parse(&output) else {
        return Found::Unread(format!("reading its links printed {:?}", output.trim()));
    };
    // `cache` with nothing after it is runnerd's setting.
    let cached = !output.lines().any(|line| line == "cache ");
    Found::Read { links, cached }
}

/// The doctor's `mirror` check: it fails while a link answers a pull with no data, which an empty blob does, and a
/// missing one while the mirror keeps the blob descriptor cache. A mirror that keeps that cache warns; a missing blob
/// without it is only noted, as the mirror fetches it again.
pub fn mirror_check(names: &Names, found: &Found) -> Check {
    let check = |status, detail: String, fix: Option<String>| Check {
        name: "mirror".to_owned(),
        status,
        detail,
        fix,
    };
    let (links, cached) = match found {
        Found::NotRunning => {
            return check(
                CheckStatus::Info,
                "not running; runnerd removes the links whose blob is gone or empty from its storage before it \
                 starts it"
                    .to_owned(),
                None,
            );
        }
        Found::Unread(why) => {
            return check(
                CheckStatus::Skip,
                format!("cannot read its storage: {why}"),
                None,
            );
        }
        Found::Read { links, cached } => (links, *cached),
    };
    let all: Vec<&Dangling> = links.dangling.iter().collect();
    let broken: Vec<&Dangling> = links
        .dangling
        .iter()
        .filter(|dangling| dangling.fault.serves_nothing(cached))
        .collect();
    let cache = "it keeps the blob descriptor cache, as a mirror an older runnerd started does, so once one \
                 repository's copy of a layer expires, pulls of that layer from another get no data; runnerd starts \
                 it without that cache when its jobs have finished";
    if !broken.is_empty() {
        let removes: Vec<String> = all.iter().flat_map(|dangling| dangling.removes()).collect();
        let restart = if cached {
            format!(
                "\ndocker restart {}   # empties the blob descriptor cache",
                names.mirror()
            )
        } else {
            String::new()
        };
        let fix = format!(
            "remove them, and the mirror fetches each again from Docker Hub:\ndocker exec {} rm -f {}{restart}",
            names.mirror(),
            removes.join(" ")
        );
        return check(
            CheckStatus::Fail,
            format!(
                "{} of its {} links answer a pull with no data: {}; jobs that pull them fail with \
                 \"short read: expected … bytes but got 0\"",
                broken.len(),
                links.count,
                named(&broken)
            ),
            Some(fix),
        );
    }
    if cached {
        return check(CheckStatus::Warn, cache.to_owned(), None);
    }
    if all.is_empty() {
        return check(
            CheckStatus::Pass,
            format!("each of its {} links leads to a stored blob", links.count),
            None,
        );
    }
    check(
        CheckStatus::Info,
        format!(
            "{} of its {} links lead to a blob that is gone: {}; it fetches each again from Docker Hub when pulled, \
             and runnerd removes them before it next starts it",
            all.len(),
            links.count,
            named(&all)
        ),
        None,
    )
}

#[cfg(test)]
mod tests;

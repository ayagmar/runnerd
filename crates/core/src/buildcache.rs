//! The build cache every runner shares (ADR 0052, 0053): the entries of Gradle's HTTP build cache, and of the other
//! tools the runner image points at it, kept per repository in a directory of the daemon's and served to each runner
//! on a Unix socket of its own, which the runner image forwards to `127.0.0.1`. Owns the entries, the size limit, the
//! sockets, and what each runner was served and stored. Which socket a request came in on is who sent it; no job can
//! claim another's. It never decides whether a job may write: the engine tells it (`Writes`), from what GitHub says
//! of the job. It never logs a request's headers, which may carry a tool's token.

mod serve;

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::net::UnixListener;
use tokio::task::{JoinHandle, JoinSet};
use tracing::warn;

use crate::api::BuildCacheCounts;
use crate::config::BuildCacheWriters;
use crate::github::{PullRequest, RunTrigger};
use crate::sync::lock;

/// Where a runner container mounts its socket's directory, read-only (`image/entrypoint.sh`).
pub const MOUNT: &str = "/run/runnerd-build-cache";

/// The socket's name in that directory.
pub const SOCKET: &str = "cache.sock";

/// The directory of the entries, in the build cache's directory: nothing else there is counted or trimmed.
const ENTRIES: &str = "entries";

/// Where uploads are written before they become entries, in the build cache's directory; emptied at start.
const UPLOADS: &str = "uploads";

/// Whether a runner's job may write entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// Not known yet: what it sends is held until it is.
    Undecided,
    /// GitHub says it runs a push, scheduled or manually started run of a branch that may write, or, where the
    /// repository lets them, a pull request of its own branches that people opened.
    Allowed,
    /// Anything else: another pull request, another branch, or a job GitHub does not place on this runner.
    Refused,
}

/// The events whose runs may write: they run code that reached a branch, never a pull request's.
const WRITING_EVENTS: [&str; 3] = ["push", "schedule", "workflow_dispatch"];

/// The event of a pull request's run that `BuildCacheWriters::PullRequests` lets write. Never
/// `pull_request_target`, which runs with the repository's secrets whatever the pull request's head is.
pub const PULL_REQUEST_EVENT: &str = "pull_request";

/// Whether `runner`'s job may write, from what GitHub says: the runner it placed the job on (`job_runner`, which
/// a job cannot forge as it can its runner's log), and its run's trigger. A push, scheduled or manually started run
/// of `repository` itself on one of `branches` may. With `writers` at `PullRequests`, so may a `pull_request` run of
/// one of the repository's own branches that people started, once GitHub also says each of its pull requests was
/// opened by one (`opened_by_user`). `Undecided` while GitHub places the job on no runner.
pub fn writes(
    runner: &str,
    job_runner: Option<&str>,
    repository: &str,
    trigger: &RunTrigger,
    branches: &[String],
    writers: BuildCacheWriters,
) -> Writes {
    match job_runner {
        None => Writes::Undecided,
        Some(placed) if placed != runner => Writes::Refused,
        Some(_) if trigger.head_repository.as_deref() != Some(repository) => Writes::Refused,
        Some(_) => {
            let Some(branch) = &trigger.head_branch else {
                return Writes::Refused;
            };
            let event = trigger.event.as_str();
            let branch_run = WRITING_EVENTS.contains(&event) && branches.contains(branch);
            let pull_request = writers == BuildCacheWriters::PullRequests
                && event == PULL_REQUEST_EVENT
                && trigger.by_user
                && !trigger.pull_requests.is_empty();
            if branch_run || pull_request {
                Writes::Allowed
            } else {
                Writes::Refused
            }
        }
    }
}

/// Whether a pull request `trigger` runs lets it write: a person opened it, from `trigger`'s head branch in
/// `repository` itself. A bot's, such as Renovate's or Dependabot's, only reads.
pub fn opened_by_user(pull: &PullRequest, repository: &str, trigger: &RunTrigger) -> bool {
    pull.by_user
        && pull.head_repository.as_deref() == Some(repository)
        && trigger.head_branch.as_ref() == Some(&pull.head_branch)
}

/// The build cache's size now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    /// The entries' bytes.
    pub bytes: u64,
    /// How many entries there are.
    pub entries: u64,
}

/// What `trim` removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trimmed {
    /// The size before.
    pub before: Size,
    /// The size after.
    pub after: Size,
}

/// The shared build cache. Cloning shares it.
#[derive(Debug, Clone)]
pub struct BuildCache {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// The entries, under `<owner>/<name>/<tool>/<key>`.
    root: PathBuf,
    /// The uploads not yet entries.
    uploads: PathBuf,
    /// One directory per runner, holding its socket.
    sockets: PathBuf,
    // A panic midway leaves at worst an entry counted that is gone, or a file not counted, which `open` counts at
    // the next start.
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    max_size: u64,
    entries: HashMap<PathBuf, Entry>,
    bytes: u64,
    runners: HashMap<String, Runner>,
    next_upload: u64,
    /// The bytes of the uploads not yet entries, being received or held: at most `max_size`, so jobs whose writes
    /// are undecided cannot fill the disk.
    uploading: u64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    bytes: u64,
    /// When it was last written or served, in ms: the least recently used go first.
    used: u64,
}

#[derive(Debug)]
struct Runner {
    repository: String,
    writes: Writes,
    counts: BuildCacheCounts,
    /// Uploads waiting for `Writes` to be decided.
    held: Vec<Upload>,
    /// The bytes of its uploads not yet entries, being received or held: at most a quarter of `max_size` while its
    /// writes are undecided, so one job cannot take the room another's uploads need.
    uploading: u64,
    dir: PathBuf,
    serving: JoinHandle<()>,
}

/// An upload written to a file of its own, not an entry yet.
#[derive(Debug)]
struct Upload {
    file: PathBuf,
    entry: PathBuf,
    bytes: u64,
}

impl BuildCache {
    /// Opens the build cache in `directory`, counting the entries an earlier run left, with each runner's socket
    /// under `sockets`.
    pub fn open(directory: &Path, sockets: &Path, max_size: u64) -> io::Result<BuildCache> {
        std::fs::create_dir_all(directory)?;
        let (root, uploads) = (directory.join(ENTRIES), directory.join(UPLOADS));
        match std::fs::remove_dir_all(&uploads) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        private_dir(&root)?;
        private_dir(&uploads)?;
        private_dir(sockets)?;
        let mut entries = HashMap::new();
        count_entries(&root, &root, &mut entries)?;
        let bytes = entries.values().map(|entry: &Entry| entry.bytes).sum();
        Ok(BuildCache {
            inner: Arc::new(Inner {
                root,
                uploads,
                sockets: sockets.to_owned(),
                state: Mutex::new(State {
                    max_size,
                    entries,
                    bytes,
                    ..State::default()
                }),
            }),
        })
    }

    /// Where the entries are: `entries` in the build cache's directory.
    pub fn directory(&self) -> &Path {
        &self.inner.root
    }

    /// Sets the size `trim` brings it under.
    pub fn set_max_size(&self, max_size: u64) {
        lock(&self.inner.state).max_size = max_size;
    }

    /// Opens `runner`'s socket for a job of `repository` and returns the directory its container mounts at `MOUNT`.
    /// A runner already open, as after a restart, gets its socket back in the directory its container mounts.
    pub fn attach(&self, runner: &str, repository: &str) -> io::Result<PathBuf> {
        if !valid_repository(repository) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{repository} is not a repository name"),
            ));
        }
        let dir = self.inner.sockets.join(socket_dir(runner));
        std::fs::create_dir_all(&dir)?;
        // The container's root, which forwards to it, is not the daemon's user under sysbox.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        let socket = dir.join(SOCKET);
        match std::fs::remove_file(&socket) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
        let serving = tokio::spawn(accept(self.inner.clone(), runner.to_owned(), listener));
        let old = lock(&self.inner.state).runners.insert(
            runner.to_owned(),
            Runner {
                repository: repository.to_owned(),
                writes: Writes::Undecided,
                counts: BuildCacheCounts::default(),
                held: Vec::new(),
                uploading: 0,
                dir: dir.clone(),
                serving,
            },
        );
        if let Some(old) = old {
            old.serving.abort();
            self.inner.remove_uploads(None, &old.held);
        }
        Ok(dir)
    }

    /// Removes the socket directories of runners not in `keep`, which an earlier run left.
    pub fn forget_others(&self, keep: &[&str]) {
        let keep: Vec<String> = keep.iter().map(|runner| socket_dir(runner)).collect();
        let Ok(dirs) = std::fs::read_dir(&self.inner.sockets) else {
            return;
        };
        for dir in dirs.flatten() {
            if !keep
                .iter()
                .any(|name| *name == dir.file_name().to_string_lossy())
            {
                let _ = std::fs::remove_dir_all(dir.path());
            }
        }
    }

    /// Whether `runner` is open and its writes undecided.
    pub fn undecided(&self, runner: &str) -> bool {
        lock(&self.inner.state)
            .runners
            .get(runner)
            .is_some_and(|runner| runner.writes == Writes::Undecided)
    }

    /// Decides whether `runner`'s job may write, once: what it sent meanwhile becomes entries or is dropped.
    pub fn decide(&self, runner: &str, writes: Writes) {
        let held = {
            let mut state = lock(&self.inner.state);
            let Some(open) = state.runners.get_mut(runner) else {
                return;
            };
            if open.writes != Writes::Undecided || writes == Writes::Undecided {
                return;
            }
            open.writes = writes;
            std::mem::take(&mut open.held)
        };
        match writes {
            Writes::Allowed => {
                for upload in held {
                    self.inner.store(runner, upload);
                }
            }
            Writes::Refused | Writes::Undecided => {
                if let Some(open) = lock(&self.inner.state).runners.get_mut(runner) {
                    open.counts.refused += held.len() as u64;
                }
                self.inner.remove_uploads(Some(runner), &held);
            }
        }
    }

    /// Closes `runner`'s socket and returns what it was served and stored, and whether it could write. What it sent
    /// while undecided is dropped.
    pub fn detach(&self, runner: &str) -> Option<BuildCacheCounts> {
        let mut open = lock(&self.inner.state).runners.remove(runner)?;
        open.serving.abort();
        self.inner.remove_uploads(None, &open.held);
        open.counts.refused += open.held.len() as u64;
        open.counts.writer = open.writes == Writes::Allowed;
        if let Err(error) = std::fs::remove_dir_all(&open.dir)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!("cannot remove {}: {error}", open.dir.display());
        }
        Some(open.counts)
    }

    /// The size of the entries now.
    pub fn size(&self) -> Size {
        let state = lock(&self.inner.state);
        Size {
            bytes: state.bytes,
            entries: state.entries.len() as u64,
        }
    }

    /// Removes the entries used least recently until the rest fit in the size limit. Blocks on the file system: call
    /// it off the engine's loop, and only while no runner runs, so no job finds an entry gone midway.
    pub fn trim(&self) -> Trimmed {
        let limit = lock(&self.inner.state).max_size;
        self.trim_to(limit)
    }

    /// Removes every entry; call it only while no runner runs, as `trim`.
    pub fn clear(&self) -> Trimmed {
        self.trim_to(0)
    }

    fn trim_to(&self, limit: u64) -> Trimmed {
        let (before, victims) = {
            let mut state = lock(&self.inner.state);
            let before = Size {
                bytes: state.bytes,
                entries: state.entries.len() as u64,
            };
            let mut by_use: Vec<(PathBuf, Entry)> = state
                .entries
                .iter()
                .map(|(path, entry)| (path.clone(), *entry))
                .collect();
            by_use.sort_by_key(|(path, entry)| (entry.used, path.clone()));
            let mut victims = Vec::new();
            for (path, entry) in by_use {
                if state.bytes <= limit && (limit > 0 || state.entries.is_empty()) {
                    break;
                }
                state.entries.remove(&path);
                state.bytes -= entry.bytes;
                victims.push(path);
            }
            (before, victims)
        };
        for path in victims {
            if let Err(error) = std::fs::remove_file(self.inner.root.join(&path))
                && error.kind() != io::ErrorKind::NotFound
            {
                warn!(
                    "cannot remove build cache entry {}: {error}",
                    path.display()
                );
            }
        }
        Trimmed {
            before,
            after: self.size(),
        }
    }
}

impl Inner {
    /// Makes an upload of `runner`'s, whose job may write, an entry; refuses it when the entries already take twice
    /// the size limit, which only a busy hour between two trims reaches.
    fn store(&self, runner: &str, upload: Upload) {
        let fits = {
            let state = lock(&self.state);
            let replaced = state
                .entries
                .get(&upload.entry)
                .map_or(0, |entry| entry.bytes);
            state.bytes - replaced + upload.bytes <= state.max_size.saturating_mul(2)
        };
        let path = self.root.join(&upload.entry);
        let stored = fits
            && path
                .parent()
                .is_none_or(|parent| std::fs::create_dir_all(parent).is_ok())
            && std::fs::rename(&upload.file, &path).is_ok();
        let mut state = lock(&self.state);
        Self::give_back(&mut state, Some(runner), upload.bytes);
        if stored {
            let entry = Entry {
                bytes: upload.bytes,
                used: crate::time::now_ms(),
            };
            if let Some(old) = state.entries.insert(upload.entry, entry) {
                state.bytes -= old.bytes;
            }
            state.bytes += upload.bytes;
        } else {
            let _ = std::fs::remove_file(&upload.file);
        }
        if let Some(open) = state.runners.get_mut(runner) {
            if stored {
                open.counts.written_bytes += upload.bytes;
            } else {
                open.counts.refused += 1;
            }
        }
    }

    /// Takes `bytes` more of the uploads' room for `runner`, unless they would take more than `max_size`, or, while
    /// its writes are undecided, its own more than a quarter of it.
    fn reserve(&self, runner: &str, bytes: u64) -> bool {
        let mut state = lock(&self.state);
        let (total, max_size) = (state.uploading, state.max_size);
        let Some(open) = state.runners.get_mut(runner) else {
            return false;
        };
        let fits = total + bytes <= max_size
            && (open.writes != Writes::Undecided || open.uploading + bytes <= max_size / 4);
        if fits {
            open.uploading += bytes;
            state.uploading += bytes;
        }
        fits
    }

    /// Gives back what `reserve` took for an upload of `runner`'s, if it is still open, that became an entry or was
    /// removed.
    fn release(&self, runner: Option<&str>, bytes: u64) {
        Self::give_back(&mut lock(&self.state), runner, bytes);
    }

    fn give_back(state: &mut State, runner: Option<&str>, bytes: u64) {
        state.uploading = state.uploading.saturating_sub(bytes);
        if let Some(open) = runner.and_then(|runner| state.runners.get_mut(runner)) {
            open.uploading = open.uploading.saturating_sub(bytes);
        }
    }

    /// Removes uploads of `runner`'s, or of a runner no longer open, and gives back their room.
    fn remove_uploads(&self, runner: Option<&str>, uploads: &[Upload]) {
        for upload in uploads {
            let _ = std::fs::remove_file(&upload.file);
            self.release(runner, upload.bytes);
        }
    }

    /// Marks an entry used now, in the index and, so the order survives a restart, in its file's time.
    fn touch(&self, entry: &Path) {
        if let Some(used) = lock(&self.state).entries.get_mut(entry) {
            used.used = crate::time::now_ms();
        }
        let path = self.root.join(entry);
        tokio::task::spawn_blocking(move || {
            if let Ok(file) = std::fs::File::open(&path) {
                let _ = file.set_modified(std::time::SystemTime::now());
            }
        });
    }
}

/// Serves `runner`'s connections until it is detached, which aborts this task and with it every connection.
async fn accept(inner: Arc<Inner>, runner: String, listener: UnixListener) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve::connection(inner.clone(), runner.clone(), stream));
                }
                Err(error) => {
                    warn!("{runner}: the build cache's socket failed: {error}");
                    return;
                }
            },
            Some(_) = connections.join_next() => {}
        }
    }
}

/// The directory of a runner's socket: a hash of its name, as a socket's path must stay under 108 bytes.
fn socket_dir(runner: &str) -> String {
    // FNV-1a, as `HostIdentity` hashes.
    let hash = runner
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Whether `name` is a path segment of an entry: letters, digits, `.`, `_` and `-`, not `.` or `..`.
fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Whether `repository` is `owner/name` with no other segment.
fn valid_repository(repository: &str) -> bool {
    repository.split_once('/').is_some_and(|(owner, name)| {
        valid_segment(owner) && valid_segment(name) && !owner.starts_with('.')
    })
}

fn private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Counts the entries under `dir`, by their path under `root`, last used when last modified.
fn count_entries(root: &Path, dir: &Path, entries: &mut HashMap<PathBuf, Entry>) -> io::Result<()> {
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        let metadata = item.metadata()?;
        if metadata.is_dir() {
            count_entries(root, &path, entries)?;
        } else if metadata.is_file()
            && let Ok(relative) = path.strip_prefix(root)
        {
            entries.insert(
                relative.to_owned(),
                Entry {
                    bytes: metadata.len(),
                    used: metadata.modified().map_or(0, crate::time::ms),
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Sends one request to a runner's socket in `dir`, as a tool in its container would through the forwarder, and
/// returns the response's status and body.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "a test helper: a socket that does not answer fails the test"
)]
pub(crate) async fn request(dir: &Path, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(dir.join(SOCKET))
        .await
        .unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Basic dG9rZW46c2VjcmV0\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let status = String::from_utf8_lossy(&response[9..12]).parse().unwrap();
    (status, response[split + 4..].to_vec())
}

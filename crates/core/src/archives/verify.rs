//! Whether a downloaded file is an action archive a runner may unpack: a whole gzip stream holding one whole tar, its
//! PAX global header's `comment` the commit it was asked for, as `git archive` writes it, and every entry inside one
//! top directory. It reads the file once, within budgets, and hashes it; it extracts nothing and never writes.
//!
//! What it proves and what it does not (ADR 0147): the archive is well formed, cannot place a file outside its
//! directory, and says it is the commit asked for. It does not prove its files are that commit's: GitHub's answer to
//! an authenticated request for that commit is trusted for that.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read};
use std::path::{Component, Path};

use flate2::bufread::GzDecoder;
use ring::digest;
use tar::EntryType;

use super::pins::is_action_file;

/// What an archive may hold, so a hostile one cannot exhaust the daemon's memory or time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// The most bytes it may expand to.
    pub(crate) expanded: u64,
    /// The most entries it may have.
    pub(crate) entries: u64,
    /// The largest action file whose pins are read; a larger one is checked but not read.
    pub(crate) action_file: u64,
}

/// The budgets runnerd checks archives with: far above any action's (actions/checkout's is 1.5M expanded).
pub(crate) const BUDGET: Budget = Budget {
    expanded: 1 << 30,
    entries: 200_000,
    action_file: 1 << 20,
};

/// A verified archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    /// The SHA-256 of the file, in lowercase hex.
    pub(crate) sha256: String,
    /// Its bytes.
    pub(crate) size: u64,
    /// The action files it holds, by the directory they are in (`""` for the top directory): what their pins are
    /// read from.
    pub(crate) actions: BTreeMap<String, String>,
}

/// Why an archive is not kept. Each says what was wrong, never with the archive's content.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Rejected {
    /// The file could not be opened.
    #[error("cannot be read: {0}")]
    Unreadable(io::Error),
    /// The gzip stream or the tar inside it is cut short or malformed.
    #[error("not a whole gzip stream of a whole tar: {0}")]
    Tar(io::Error),
    /// It expands past the budget, or has more entries than it allows.
    #[error("past its budget of {0}")]
    Budget(&'static str),
    /// It does not open with a PAX global header whose comment is the commit asked for.
    #[error("its PAX comment is not the commit asked for")]
    Comment,
    /// An entry's name leaves the top directory, or the entries have more than one.
    #[error("entry {0:?} is outside its one top directory")]
    Outside(String),
    /// A hard link, a device, a FIFO or another kind a runner should never unpack.
    #[error("entry {0:?} is of a kind an action archive does not hold")]
    Kind(String),
    /// A symbolic link whose target leaves the top directory.
    #[error("symbolic link {0:?} points outside its top directory")]
    Link(String),
    /// Two entries of one name.
    #[error("entry {0:?} is in it twice")]
    Duplicate(String),
    /// Bytes follow the gzip stream.
    #[error("data follows its gzip stream")]
    Trailing,
}

/// Reads `path` whole and says whether it is the archive of commit `sha`, within `budget`.
pub(crate) fn verify(path: &Path, sha: &str, budget: Budget) -> Result<Verified, Rejected> {
    let file = File::open(path).map_err(Rejected::Unreadable)?;
    let mut hashed = Hashed::new(BufReader::new(file));
    let verified = read_archive(&mut hashed, sha, budget)?;
    let (sha256, size) = hashed.finish();
    Ok(Verified {
        sha256,
        size,
        actions: verified,
    })
}

/// The SHA-256 and size of the file at `path`, as the store checks a kept archive against its index.
pub(crate) fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut hashed = Hashed::new(BufReader::new(File::open(path)?));
    io::copy(&mut hashed, &mut io::sink())?;
    Ok(hashed.finish())
}

fn read_archive<R: Read>(
    hashed: &mut Hashed<R>,
    sha: &str,
    budget: Budget,
) -> Result<BTreeMap<String, String>, Rejected> {
    // A reader of our own under the decoder, so what it reads past the gzip stream is still there to look at.
    let mut gzip = GzDecoder::new(BufReader::new(hashed));
    let capped = Capped {
        inner: &mut gzip,
        left: budget.expanded,
    };
    let mut archive = tar::Archive::new(capped);
    let mut checker = Checker {
        sha,
        budget,
        top: None,
        seen: BTreeSet::new(),
        entries: 0,
        commented: false,
        actions: BTreeMap::new(),
    };
    for entry in archive.entries().map_err(tar_error)? {
        let entry = entry.map_err(tar_error)?;
        checker.check(entry)?;
    }
    if !checker.commented || checker.top.is_none() {
        return Err(Rejected::Comment);
    }
    // The rest of the tar stream (its end blocks and padding) and the gzip trailer, whose CRC and length the decoder
    // checks once it reaches them.
    let mut rest = archive.into_inner();
    io::copy(&mut rest, &mut io::sink()).map_err(tar_error)?;
    // Nothing may follow the gzip stream: a second member would be read by `tar -xzf` and not checked here.
    let mut buffered = gzip.into_inner();
    match buffered.fill_buf() {
        Ok([]) => Ok(checker.actions),
        Ok(_) => Err(Rejected::Trailing),
        Err(error) => Err(Rejected::Tar(error)),
    }
}

/// A read that failed: past the budget, or a gzip or tar stream that is cut short or malformed, which the decoder
/// and the tar reader do not tell apart.
fn tar_error(error: io::Error) -> Rejected {
    if error.kind() == io::ErrorKind::FileTooLarge {
        Rejected::Budget("expanded bytes")
    } else {
        Rejected::Tar(error)
    }
}

/// What has been seen of an archive so far.
struct Checker<'sha> {
    sha: &'sha str,
    budget: Budget,
    /// The one top directory every entry is in.
    top: Option<String>,
    seen: BTreeSet<String>,
    entries: u64,
    /// Whether its global header named the commit.
    commented: bool,
    actions: BTreeMap<String, String>,
}

impl Checker<'_> {
    fn check<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<(), Rejected> {
        self.entries += 1;
        if self.entries > self.budget.entries {
            return Err(Rejected::Budget("entries"));
        }
        let kind = entry.header().entry_type();
        if kind == EntryType::XGlobalHeader {
            // Only the first entry, as `git archive` writes it.
            if self.entries != 1 {
                return Err(Rejected::Kind("a second PAX global header".to_owned()));
            }
            let comment = entry
                .pax_extensions()
                .map_err(tar_error)?
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .find(|extension| extension.key().ok() == Some("comment"))
                .and_then(|extension| extension.value().ok().map(str::to_owned));
            if comment.as_deref() != Some(self.sha) {
                return Err(Rejected::Comment);
            }
            self.commented = true;
            return Ok(());
        }
        if !self.commented {
            return Err(Rejected::Comment);
        }
        let name = entry
            .path()
            .map_err(tar_error)?
            .to_string_lossy()
            .into_owned();
        let parts = components(&name).ok_or_else(|| Rejected::Outside(name.clone()))?;
        let Some((first, inside)) = parts.split_first() else {
            return Err(Rejected::Outside(name));
        };
        match &self.top {
            Some(top) if top != first => return Err(Rejected::Outside(name)),
            Some(_) => {}
            None => self.top = Some((*first).to_owned()),
        }
        let normal = parts.join("/");
        if !self.seen.insert(normal) {
            return Err(Rejected::Duplicate(name));
        }
        match kind {
            EntryType::Directory => Ok(()),
            // A file directly at the top would be a second top-level entry.
            _ if inside.is_empty() => Err(Rejected::Outside(name)),
            EntryType::Regular | EntryType::Continuous => {
                let relative = inside.join("/");
                if is_action_file(&relative) && entry.size() <= self.budget.action_file {
                    let mut text = String::new();
                    // An action file that is not UTF-8 holds no pins a runner reads.
                    if entry.read_to_string(&mut text).is_ok() {
                        let dir = super::pins::action_dir(&relative).to_owned();
                        self.actions.entry(dir).or_insert(text);
                    }
                }
                Ok(())
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(tar_error)?
                    .map(|target| target.to_string_lossy().into_owned())
                    .ok_or_else(|| Rejected::Link(name.clone()))?;
                if confined(&inside[..inside.len() - 1], &target) {
                    Ok(())
                } else {
                    Err(Rejected::Link(name))
                }
            }
            _ => Err(Rejected::Kind(name)),
        }
    }
}

/// The normal parts of an entry's name, `None` if it is absolute or climbs with `..`. `.` parts are dropped.
fn components(name: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts)
}

/// Whether a symbolic link in the directory `dir` (parts below the top directory) pointing at `target` stays inside
/// the top directory, once its `..` are followed.
fn confined(dir: &[&str], target: &str) -> bool {
    if target.starts_with('/') || target.is_empty() {
        return false;
    }
    let mut depth = dir.len();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let Some(up) = depth.checked_sub(1) else {
                    return false;
                };
                depth = up;
            }
            _ => depth += 1,
        }
    }
    true
}

/// A reader that hashes and counts what passes through it.
struct Hashed<R> {
    inner: R,
    context: digest::Context,
    size: u64,
}

impl<R: Read> Hashed<R> {
    fn new(inner: R) -> Hashed<R> {
        Hashed {
            inner,
            context: digest::Context::new(&digest::SHA256),
            size: 0,
        }
    }

    /// The SHA-256 in lowercase hex and the size of what was read.
    fn finish(self) -> (String, u64) {
        (hex(self.context.finish().as_ref()), self.size)
    }
}

/// `bytes` in lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.context.update(&buf[..read]);
        self.size += read as u64;
        Ok(read)
    }
}

/// A reader that fails once more than `left` bytes were read through it.
struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.left = self
            .left
            .checked_sub(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::FileTooLarge, "past the budget"))?;
        Ok(read)
    }
}

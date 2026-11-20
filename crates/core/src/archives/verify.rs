//! Whether a downloaded file is an action archive a runner may unpack: a whole gzip stream holding one whole tar in
//! the narrow shape `git archive` writes, its PAX global header's `comment` the commit it was asked for, and every
//! entry inside one top directory. It reads the headers itself, so what it checks is what GNU `tar -xzf` unpacks: only
//! the one global `comment`, extended headers of one `path` each, ustar headers with valid checksums, directories and
//! regular files each inside a directory listed before it, and two zero blocks then zeros to the end. Links, GNU long
//! names and any other metadata are refused, and the runner downloads that action itself. It reads the file once,
//! within budgets, and hashes it; it extracts nothing and never writes.
//!
//! What it proves and what it does not (ADR 0147): the archive is well formed, cannot place a file outside its
//! directory, and says it is the commit asked for. It does not prove its files are that commit's: GitHub's answer for
//! that commit is trusted for that.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read};
use std::path::Path;

use flate2::bufread::GzDecoder;
use ring::digest;

use super::pins::is_action_file;

#[cfg(test)]
mod tests;

/// A tar block.
const BLOCK: usize = 512;

/// The most bytes of PAX records a header may carry: far above one path's.
const RECORDS: u64 = 64 << 10;

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
    pub(crate) actions: BTreeMap<String, ActionFile>,
}

/// An action's metadata file in an archive: the one the runner loads, `action.yml` before `action.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionFile {
    /// Whether it is `action.yml`.
    pub(crate) yml: bool,
    /// Its bytes; `None` when it is larger than the budget lets be read, so what it pins is not known.
    pub(crate) bytes: Option<Vec<u8>>,
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
    /// It does not open with a PAX global header whose one record is the comment of the commit asked for.
    #[error("its PAX comment is not the commit asked for")]
    Comment,
    /// A header `git archive` does not write: a bad checksum or magic, a size that is not octal, extended records
    /// other than one `path`, or a mode beyond the permission bits.
    #[error("a header is not one git archive writes: {0}")]
    Header(&'static str),
    /// An entry's name leaves the top directory, the entries have more than one, or its directory was not listed
    /// before it.
    #[error("entry {0:?} is outside a directory listed before it")]
    Outside(String),
    /// A link, a device, a FIFO or another kind it does not hold.
    #[error("entry {0:?} is of a kind it does not hold")]
    Kind(String),
    /// Two entries of one name.
    #[error("entry {0:?} is in it twice")]
    Duplicate(String),
    /// The tar does not end in two zero blocks and zeros after them.
    #[error("its tar does not end as one does")]
    End,
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

/// The names `path` holds as `tar -t` lists them, directories with a trailing `/`, sorted; for the tests that compare
/// it with GNU tar's.
#[cfg(test)]
pub(crate) fn listing(path: &Path, sha: &str) -> Result<Vec<String>, Rejected> {
    let file = File::open(path).map_err(Rejected::Unreadable)?;
    let mut hashed = Hashed::new(BufReader::new(file));
    let checker = read(&mut hashed, sha, BUDGET)?;
    let mut names: Vec<String> = checker
        .seen
        .iter()
        .map(|name| {
            if checker.dirs.contains(name) {
                format!("{name}/")
            } else {
                name.clone()
            }
        })
        .collect();
    names.sort();
    Ok(names)
}

fn read_archive<R: Read>(
    hashed: &mut Hashed<R>,
    sha: &str,
    budget: Budget,
) -> Result<BTreeMap<String, ActionFile>, Rejected> {
    read(hashed, sha, budget).map(|checker| checker.actions)
}

fn read<'sha, R: Read>(
    hashed: &mut Hashed<R>,
    sha: &'sha str,
    budget: Budget,
) -> Result<Checker<'sha>, Rejected> {
    // A reader of our own under the decoder, so what it reads past the gzip stream is still there to look at.
    let mut gzip = GzDecoder::new(BufReader::new(hashed));
    let mut tar = Capped {
        inner: &mut gzip,
        left: budget.expanded,
    };
    let mut checker = Checker {
        sha,
        budget,
        top: None,
        seen: BTreeSet::new(),
        dirs: BTreeSet::new(),
        entries: 0,
        commented: false,
        path: None,
        actions: BTreeMap::new(),
    };
    loop {
        let block = read_block(&mut tar)?;
        if block.iter().all(|&byte| byte == 0) {
            break;
        }
        checker.check(&block, &mut tar)?;
    }
    if !checker.commented || checker.top.is_none() || checker.path.is_some() {
        return Err(if checker.commented {
            Rejected::End
        } else {
            Rejected::Comment
        });
    }
    // The second zero block, then zeros to the end of the stream, which also has the decoder check the gzip
    // trailer's CRC and length.
    if read_block(&mut tar)?.iter().any(|&byte| byte != 0) {
        return Err(Rejected::End);
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let read = tar.read(&mut buffer).map_err(tar_error)?;
        if read == 0 {
            break;
        }
        if buffer[..read].iter().any(|&byte| byte != 0) {
            return Err(Rejected::End);
        }
    }
    // Nothing may follow the gzip stream: a second member would be read by `tar -xzf` and not checked here.
    let mut buffered = gzip.into_inner();
    match buffered.fill_buf() {
        Ok([]) => Ok(checker),
        Ok(_) => Err(Rejected::Trailing),
        Err(error) => Err(Rejected::Tar(error)),
    }
}

/// The next block; a stream that ends inside one, or before it, is cut short.
fn read_block<R: Read>(reader: &mut R) -> Result<[u8; BLOCK], Rejected> {
    let mut block = [0_u8; BLOCK];
    reader.read_exact(&mut block).map_err(tar_error)?;
    Ok(block)
}

/// Reads `size` bytes of an entry's data and its padding to the next block: the data, when `keep`.
fn read_data<R: Read>(reader: &mut R, size: u64, keep: bool) -> Result<Option<Vec<u8>>, Rejected> {
    let padded = size.next_multiple_of(BLOCK as u64);
    let data = if keep {
        let mut data = Vec::new();
        reader
            .by_ref()
            .take(size)
            .read_to_end(&mut data)
            .map_err(tar_error)?;
        if data.len() as u64 != size {
            return Err(Rejected::Tar(io::ErrorKind::UnexpectedEof.into()));
        }
        Some(data)
    } else {
        skip(reader, size)?;
        None
    };
    skip(reader, padded - size)?;
    Ok(data)
}

/// Reads past `count` bytes, all of which must be there.
fn skip<R: Read>(reader: &mut R, count: u64) -> Result<(), Rejected> {
    let skipped = io::copy(&mut reader.by_ref().take(count), &mut io::sink()).map_err(tar_error)?;
    if skipped == count {
        Ok(())
    } else {
        Err(Rejected::Tar(io::ErrorKind::UnexpectedEof.into()))
    }
}

/// A read that failed: past the budget, or a gzip or tar stream that is cut short or malformed, which the decoder
/// does not tell apart.
fn tar_error(error: io::Error) -> Rejected {
    if error.kind() == io::ErrorKind::FileTooLarge {
        Rejected::Budget("expanded bytes")
    } else {
        Rejected::Tar(error)
    }
}

/// A ustar header as `git archive` writes it.
struct Header<'block> {
    /// Its name: the prefix, a `/`, and the name, or the name alone.
    name: String,
    kind: u8,
    size: u64,
    block: &'block [u8; BLOCK],
}

impl Header<'_> {
    /// Reads `block` as a POSIX ustar header with a valid checksum, an octal size and a mode of permission bits only.
    fn parse(block: &[u8; BLOCK]) -> Result<Header<'_>, Rejected> {
        if &block[257..263] != b"ustar\0" || &block[263..265] != b"00" {
            return Err(Rejected::Header("its magic is not POSIX ustar's"));
        }
        let stored =
            octal(&block[148..156]).ok_or(Rejected::Header("its checksum is not octal"))?;
        let sum: u64 = block
            .iter()
            .enumerate()
            .map(|(at, &byte)| {
                if (148..156).contains(&at) {
                    u64::from(b' ')
                } else {
                    u64::from(byte)
                }
            })
            .sum();
        if stored != sum {
            return Err(Rejected::Header("its checksum does not match"));
        }
        let size = octal(&block[124..136]).ok_or(Rejected::Header("its size is not octal"))?;
        let mode = octal(&block[100..108]).ok_or(Rejected::Header("its mode is not octal"))?;
        if mode & !0o777 != 0 {
            return Err(Rejected::Header("its mode has more than permission bits"));
        }
        let name = field(&block[..100])?;
        let prefix = field(&block[345..500])?;
        let name = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        Ok(Header {
            name,
            kind: block[156],
            size,
            block,
        })
    }

    /// Whether its link name is empty, as it is for anything but a link.
    fn unlinked(&self) -> bool {
        self.block[157..257].iter().all(|&byte| byte == 0)
    }
}

/// A NUL-terminated header field, UTF-8.
fn field(bytes: &[u8]) -> Result<&str, Rejected> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).map_err(|_| Rejected::Header("a name is not UTF-8"))
}

/// An octal number field as `git archive` writes it: digits, ended by a NUL or a space; never base-256.
fn octal(bytes: &[u8]) -> Option<u64> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0 || byte == b' ')
        .unwrap_or(bytes.len());
    let digits = &bytes[..end];
    if digits.is_empty() || bytes[end..].iter().any(|&byte| byte != 0 && byte != b' ') {
        return None;
    }
    digits.iter().try_fold(0_u64, |value, &digit| {
        (b'0'..=b'7')
            .contains(&digit)
            .then(|| value.checked_mul(8)?.checked_add(u64::from(digit - b'0')))
            .flatten()
    })
}

/// PAX records as `<length> <key>=<value>\n`, each length its record's bytes, filling `data` exactly.
fn records(data: &[u8]) -> Result<Vec<(&str, &str)>, Rejected> {
    let malformed = Rejected::Header("its PAX records are malformed");
    let mut found = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest
            .iter()
            .position(|&byte| byte == b' ')
            .ok_or(Rejected::Header("its PAX records are malformed"))?;
        let length: usize = std::str::from_utf8(&rest[..space])
            .ok()
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .ok_or(Rejected::Header("its PAX records are malformed"))?;
        if length <= space + 1 || length > rest.len() || rest[length - 1] != b'\n' {
            return Err(malformed);
        }
        let record = &rest[space + 1..length - 1];
        // GNU tar ends a path at a NUL, so a name with one would unpack as another; no control character belongs in
        // a name `git archive` writes.
        if record.iter().any(|&byte| byte.is_ascii_control()) {
            return Err(Rejected::Header("a PAX record holds a control character"));
        }
        let record = std::str::from_utf8(record)
            .map_err(|_| Rejected::Header("its PAX records are malformed"))?;
        let (key, value) = record
            .split_once('=')
            .ok_or(Rejected::Header("its PAX records are malformed"))?;
        found.push((key, value));
        rest = &rest[length..];
    }
    Ok(found)
}

/// What has been seen of an archive so far.
struct Checker<'sha> {
    sha: &'sha str,
    budget: Budget,
    /// The one top directory every entry is in.
    top: Option<String>,
    /// Every entry's name, without a trailing `/`.
    seen: BTreeSet<String>,
    /// Those of directories.
    dirs: BTreeSet<String>,
    entries: u64,
    /// Whether its global header named the commit.
    commented: bool,
    /// The `path` of an extended header, for the entry after it.
    path: Option<String>,
    actions: BTreeMap<String, ActionFile>,
}

impl Checker<'_> {
    /// Checks the entry whose header is `block`, and reads its data from `tar`.
    fn check<R: Read>(&mut self, block: &[u8; BLOCK], tar: &mut R) -> Result<(), Rejected> {
        let header = Header::parse(block)?;
        if !header.unlinked() {
            return Err(Rejected::Kind(header.name));
        }
        match header.kind {
            b'g' => return self.global(&header, tar),
            b'x' => return self.extended(&header, tar),
            _ => {}
        }
        if !self.commented {
            return Err(Rejected::Comment);
        }
        self.entries += 1;
        if self.entries > self.budget.entries {
            return Err(Rejected::Budget("entries"));
        }
        let name = self.path.take().unwrap_or(header.name);
        match header.kind {
            b'5' => {
                let path = name.strip_suffix('/').unwrap_or(&name);
                self.place(path, &name)?;
                if header.size != 0 {
                    return Err(Rejected::Header("a directory has data"));
                }
                self.dirs.insert(path.to_owned());
                Ok(())
            }
            b'0' => {
                self.place(&name, &name)?;
                let Some((_, inside)) = name.split_once('/') else {
                    return Err(Rejected::Outside(name));
                };
                if !is_action_file(inside) {
                    read_data(tar, header.size, false)?;
                    return Ok(());
                }
                let keep = header.size <= self.budget.action_file;
                let bytes = read_data(tar, header.size, keep)?;
                let file = ActionFile {
                    yml: inside.rsplit('/').next() == Some("action.yml"),
                    bytes,
                };
                let dir = super::pins::action_dir(inside).to_owned();
                // The runner loads `action.yml` when both are there.
                match self.actions.get(&dir) {
                    Some(kept) if kept.yml => {}
                    _ => {
                        self.actions.insert(dir, file);
                    }
                }
                Ok(())
            }
            _ => Err(Rejected::Kind(name)),
        }
    }

    /// The global header: the first entry, one record, the commit's `comment`.
    fn global<R: Read>(&mut self, header: &Header<'_>, tar: &mut R) -> Result<(), Rejected> {
        if self.commented || self.entries != 0 || header.size > RECORDS {
            return Err(Rejected::Comment);
        }
        let data = read_data(tar, header.size, true)?.unwrap_or_default();
        match records(&data)?.as_slice() {
            [("comment", comment)] if *comment == self.sha => {
                self.commented = true;
                Ok(())
            }
            _ => Err(Rejected::Comment),
        }
    }

    /// An extended header: one `path` record, for an entry that follows.
    fn extended<R: Read>(&mut self, header: &Header<'_>, tar: &mut R) -> Result<(), Rejected> {
        if !self.commented {
            return Err(Rejected::Comment);
        }
        if self.path.is_some() || header.size > RECORDS {
            return Err(Rejected::Header("an extended header is not one path"));
        }
        let data = read_data(tar, header.size, true)?.unwrap_or_default();
        match records(&data)?.as_slice() {
            [("path", path)] if !path.is_empty() => {
                self.path = Some((*path).to_owned());
                Ok(())
            }
            _ => Err(Rejected::Header("an extended header is not one path")),
        }
    }

    /// Notes `path` (`name` as written), which must be the top directory, or new and in a directory listed before it.
    fn place(&mut self, path: &str, name: &str) -> Result<(), Rejected> {
        let parts: Vec<&str> = path.split('/').collect();
        if parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
        {
            return Err(Rejected::Outside(name.to_owned()));
        }
        match parts.split_last() {
            Some((_, [])) if self.top.is_none() && name.ends_with('/') => {
                self.top = Some(path.to_owned());
            }
            Some((_, parents)) if !parents.is_empty() => {
                if !self.dirs.contains(&parents.join("/")) {
                    return Err(Rejected::Outside(name.to_owned()));
                }
            }
            _ => return Err(Rejected::Outside(name.to_owned())),
        }
        if !self.seen.insert(path.to_owned()) {
            return Err(Rejected::Duplicate(name.to_owned()));
        }
        Ok(())
    }
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

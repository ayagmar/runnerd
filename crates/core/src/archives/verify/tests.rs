//! The verifier against what GNU tar does with the same bytes (ADR 0147): `git archive`'s own output passes and lists
//! as `tar -tzf` lists it, and every shape GNU tar reads differently from a plain reading of the headers is refused:
//! global PAX fields besides the comment, extended headers beyond one `path`, GNU long names, links, entries outside
//! a directory seen before them, and a tar that does not end in two zero blocks and zero padding.

#![allow(
    clippy::unwrap_used,
    reason = "tests: a fixture that cannot be made fails the test"
)]

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;

use super::{BUDGET, Rejected, listing, verify};
use crate::testdir::TestDir;

const SHA: &str = "08eba0b27e820071cde6df949e0beb9ba4906955";
const TOP: &str = "o-r-08eba0b/";

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// A ustar header as `git archive` writes it: `name` split into prefix and name when it is longer than 100 bytes.
fn header(name: &str, kind: u8, size: usize, mode: u32) -> [u8; 512] {
    let mut block = [0_u8; 512];
    let (prefix, name) = if name.len() > 100 {
        let split = name[..=155.min(name.len() - 1)].rfind('/').unwrap();
        (&name[..split], &name[split + 1..])
    } else {
        ("", name)
    };
    block[..name.len()].copy_from_slice(name.as_bytes());
    block[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    block[108..116].copy_from_slice(b"0000000\0");
    block[116..124].copy_from_slice(b"0000000\0");
    block[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    block[136..148].copy_from_slice(b"14531234567\0");
    block[156] = kind;
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    checksum(&mut block);
    block
}

fn checksum(block: &mut [u8; 512]) {
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|&byte| u32::from(byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
}

/// PAX records, each `<length> <key>=<value>\n`.
fn records(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in pairs {
        let body = format!(" {key}={value}\n");
        // The length counts its own digits.
        let mut length = body.len() + 1;
        while body.len() + length.to_string().len() != length {
            length = body.len() + length.to_string().len();
        }
        out.extend(format!("{length}{body}").into_bytes());
    }
    out
}

/// One entry: a header and its data, padded.
fn entry(out: &mut Vec<u8>, name: &str, kind: u8, data: &[u8], mode: u32) {
    out.extend(header(name, kind, data.len(), mode));
    out.extend(data);
    out.resize(out.len().next_multiple_of(512), 0);
}

/// The global header `git archive` opens with.
fn global(out: &mut Vec<u8>, pairs: &[(&str, &str)]) {
    entry(out, "pax_global_header", b'g', &records(pairs), 0o666);
}

/// Two zero blocks, then zeros to a 10240-byte record, as `git archive` ends.
fn end(out: &mut Vec<u8>) {
    out.extend([0_u8; 1024]);
    out.resize(out.len().next_multiple_of(10240), 0);
}

/// A well-formed archive of `SHA` around `body`, which writes its entries after the top directory.
fn archive(body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    global(&mut out, &[("comment", SHA)]);
    entry(&mut out, TOP, b'5', b"", 0o775);
    body(&mut out);
    end(&mut out);
    out
}

/// An archive of only its top directory, after a global header of `pairs`.
fn opened(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    global(&mut out, pairs);
    entry(&mut out, TOP, b'5', b"", 0o775);
    end(&mut out);
    out
}

fn check(dir: &TestDir, tar: &[u8]) -> Result<super::Verified, Rejected> {
    let path = dir.0.join("a.tar.gz");
    fs::write(&path, gzip(tar)).unwrap();
    verify(&path, SHA, BUDGET)
}

/// A git command in `repo`, isolated from the host's configuration.
fn git(repo: &Path, home: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    output.stdout
}

/// A repository with `files` committed, and its `git archive` as GitHub serves it: gzip, under one top directory.
fn git_archive(
    dir: &TestDir,
    files: &[(&str, &str)],
    links: &[(&str, &str)],
) -> (String, std::path::PathBuf) {
    let repo = dir.0.join("repo");
    let home = dir.0.join("home");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&home).unwrap();
    git(&repo, &home, &["init", "-q", "-b", "main"]);
    for (name, text) in files {
        let path = repo.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    for (name, target) in links {
        std::os::unix::fs::symlink(target, repo.join(name)).unwrap();
    }
    git(&repo, &home, &["add", "-A"]);
    git(&repo, &home, &["commit", "-q", "-m", "c"]);
    let sha = String::from_utf8(git(&repo, &home, &["rev-parse", "HEAD"])).unwrap();
    let sha = sha.trim().to_owned();
    let path = dir.0.join("archive.tar.gz");
    let prefix = format!("--prefix=o-r-{}/", &sha[..7]);
    let bytes = git(
        &repo,
        &home,
        &["archive", "--format=tar.gz", &prefix, "HEAD"],
    );
    fs::write(&path, bytes).unwrap();
    (sha, path)
}

#[test]
fn what_git_archive_writes_passes_and_lists_as_gnu_tar_lists_it() {
    let dir = TestDir::new("verify");
    let deep = format!("{}/file.txt", ["nested-directory"; 8].join("/"));
    let long = format!("dir/{}.txt", "n".repeat(120));
    let (sha, path) = git_archive(
        &dir,
        &[
            ("action.yml", "runs:\n  using: node24\n  main: index.js\n"),
            ("sub/action.yaml", "runs: {}\n"),
            ("sub/action.yml", "runs:\n  using: composite\n  steps: []\n"),
            ("index.js", "console.log(1)\n"),
            (deep.as_str(), "deep"),
            (long.as_str(), "long"),
        ],
        &[],
    );
    let verified = verify(&path, &sha, BUDGET).unwrap();
    let sub = &verified.actions["sub"];
    assert!(sub.yml, "the runner loads action.yml before action.yaml");
    assert_eq!(
        sub.bytes.as_deref(),
        Some(&b"runs:\n  using: composite\n  steps: []\n"[..])
    );
    let tar = Command::new("tar").arg("-tzf").arg(&path).output().unwrap();
    assert!(tar.status.success(), "{tar:?}");
    let mut gnu: Vec<String> = String::from_utf8(tar.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    gnu.sort();
    assert_eq!(
        listing(&path, &sha).unwrap(),
        gnu,
        "what tar unpacks is what was checked"
    );
}

#[test]
fn an_archive_with_a_link_is_refused_and_the_runner_downloads_it_itself() {
    let dir = TestDir::new("verify");
    let (sha, path) = git_archive(
        &dir,
        &[("action.yml", "runs: {}\n")],
        &[("link", "action.yml")],
    );
    assert!(
        matches!(verify(&path, &sha, BUDGET), Err(Rejected::Kind(_))),
        "a link stays out until links are checked as the runner's copy follows them"
    );
}

#[test]
fn pax_records_gnu_tar_reads_otherwise_are_refused() {
    let dir = TestDir::new("verify");
    let file = |out: &mut Vec<u8>| entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "a global path",
            opened(&[("comment", SHA), ("path", "../../evil")]),
        ),
        ("a global size", opened(&[("size", "0"), ("comment", SHA)])),
        (
            "two comments",
            opened(&[("comment", SHA), ("comment", SHA)]),
        ),
        ("a global header after an entry", {
            let mut out = Vec::new();
            global(&mut out, &[("comment", SHA)]);
            entry(&mut out, TOP, b'5', b"", 0o775);
            global(&mut out, &[("comment", SHA)]);
            end(&mut out);
            out
        }),
        (
            "an extended linkpath",
            archive(|out| {
                entry(
                    out,
                    "x",
                    b'x',
                    &records(&[("linkpath", "/etc/passwd")]),
                    0o644,
                );
                file(out);
            }),
        ),
        (
            "two extended paths",
            archive(|out| {
                let path = format!("{TOP}b");
                entry(
                    out,
                    "x",
                    b'x',
                    &records(&[("path", &path), ("path", "../evil")]),
                    0o644,
                );
                file(out);
            }),
        ),
        (
            "an extended mtime",
            archive(|out| {
                entry(out, "x", b'x', &records(&[("mtime", "1")]), 0o644);
                file(out);
            }),
        ),
        (
            "an extended header before the end",
            archive(|out| {
                let path = format!("{TOP}b");
                entry(out, "x", b'x', &records(&[("path", &path)]), 0o644);
            }),
        ),
        (
            "a malformed record",
            archive(|out| {
                entry(out, "x", b'x', b"99 path=o-r-08eba0b/b\n", 0o644);
                file(out);
            }),
        ),
        (
            "a GNU long name",
            archive(|out| {
                entry(
                    out,
                    "././@LongLink",
                    b'L',
                    format!("{TOP}b\0").as_bytes(),
                    0o644,
                );
                file(out);
            }),
        ),
    ];
    for (case, tar) in cases {
        let result = check(&dir, &tar);
        assert!(result.is_err(), "{case} is refused: {result:?}");
    }
    assert!(
        check(&dir, &archive(file)).is_ok(),
        "the same archive without them passes"
    );
}

#[test]
fn a_header_git_archive_does_not_write_is_refused() {
    let dir = TestDir::new("verify");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "a bad checksum",
            archive(|out| {
                let mut block = header(&format!("{TOP}a"), b'0', 0, 0o664);
                block[148] = b'7';
                out.extend(block);
            }),
        ),
        (
            "GNU's magic",
            archive(|out| {
                let mut block = header(&format!("{TOP}a"), b'0', 0, 0o664);
                block[257..265].copy_from_slice(b"ustar  \0");
                checksum(&mut block);
                out.extend(block);
            }),
        ),
        (
            "a base-256 size",
            archive(|out| {
                let mut block = header(&format!("{TOP}a"), b'0', 0, 0o664);
                block[124..136].copy_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
                checksum(&mut block);
                out.extend(block);
            }),
        ),
        (
            "a setuid file",
            archive(|out| entry(out, &format!("{TOP}a"), b'0', b"a", 0o4755)),
        ),
        (
            "a hard link",
            archive(|out| entry(out, &format!("{TOP}a"), b'1', b"", 0o644)),
        ),
        (
            "a symbolic link",
            archive(|out| entry(out, &format!("{TOP}a"), b'2', b"", 0o777)),
        ),
    ];
    for (case, tar) in cases {
        let result = check(&dir, &tar);
        assert!(result.is_err(), "{case} is refused: {result:?}");
    }
    let file = |out: &mut Vec<u8>| entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
    assert!(
        check(&dir, &archive(file)).is_ok(),
        "the same archive without them passes"
    );
}

#[test]
fn every_entry_sits_in_a_directory_seen_before_it() {
    let dir = TestDir::new("verify");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "a file used as a directory",
            archive(|out| {
                entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
                entry(out, &format!("{TOP}a/b"), b'0', b"b", 0o664);
            }),
        ),
        (
            "a directory never listed",
            archive(|out| entry(out, &format!("{TOP}a/b"), b'0', b"b", 0o664)),
        ),
        (
            "a file and a directory of one name",
            archive(|out| {
                entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
                entry(out, &format!("{TOP}a/"), b'5', b"", 0o775);
            }),
        ),
        ("a file before the top directory", {
            let mut out = Vec::new();
            global(&mut out, &[("comment", SHA)]);
            entry(&mut out, &format!("{TOP}a"), b'0', b"a", 0o664);
            entry(&mut out, TOP, b'5', b"", 0o775);
            end(&mut out);
            out
        }),
        (
            "an extended path outside the top directory",
            archive(|out| {
                entry(out, "x", b'x', &records(&[("path", "../evil")]), 0o644);
                entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
            }),
        ),
    ];
    for (case, tar) in cases {
        let result = check(&dir, &tar);
        assert!(result.is_err(), "{case} is refused: {result:?}");
    }
}

#[test]
fn a_tar_ends_in_two_zero_blocks_and_nothing_but_zeros_after() {
    let dir = TestDir::new("verify");
    let body = |out: &mut Vec<u8>| {
        global(out, &[("comment", SHA)]);
        entry(out, TOP, b'5', b"", 0o775);
        entry(out, &format!("{TOP}a"), b'0', b"a", 0o664);
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("no end", {
            let mut out = Vec::new();
            body(&mut out);
            out
        }),
        ("one zero block", {
            let mut out = Vec::new();
            body(&mut out);
            out.extend([0_u8; 512]);
            out
        }),
        ("an entry after one zero block", {
            let mut out = Vec::new();
            body(&mut out);
            out.extend([0_u8; 512]);
            entry(&mut out, &format!("{TOP}hidden"), b'0', b"h", 0o664);
            end(&mut out);
            out
        }),
        ("bytes in the padding", {
            let mut out = Vec::new();
            body(&mut out);
            end(&mut out);
            let last = out.len() - 1;
            out[last] = 1;
            out
        }),
    ];
    for (case, tar) in cases {
        let result = check(&dir, &tar);
        assert!(result.is_err(), "{case} is refused: {result:?}");
    }
}

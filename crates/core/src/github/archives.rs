//! What the action archives read from GitHub (`archives::Source`, ADR 0147): a repository's canonical name and
//! visibility, its default branch's head, its tree and files, with the token through the API, and an action's archive,
//! downloaded from codeload.github.com without it. An archive's download never carries the token, is never asked again
//! with it, and follows no redirect: what a private repository holds cannot reach the store, whatever its visibility
//! was when it was looked up.

use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::Method;
use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;

use super::{Error, Github, GithubApi as _};
use crate::archives::{ActionRepository, Source, Tree, TreeFile};

/// Where action archives are downloaded from, anonymously.
pub(super) const CODELOAD: &str = "https://codeload.github.com";

/// How long one archive's download may take, its redirect included.
pub(super) const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize)]
struct RepositoryBody {
    full_name: String,
    visibility: crate::api::Visibility,
    default_branch: String,
}

#[derive(Deserialize)]
struct RefBody {
    object: RefObject,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Deserialize)]
struct TreeBody {
    #[serde(default)]
    truncated: bool,
    tree: Vec<TreeEntry>,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct BlobBody {
    content: String,
    encoding: String,
    size: u64,
}

/// A path segment as a URL carries it: a branch name may hold `#`, `%` or `?`.
fn escaped(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A failed download, with what failed and never the address.
fn download_error(path: &str, what: impl std::fmt::Display) -> Error {
    Error::Download(format!("{path}: {what}"))
}

impl Source for Github {
    async fn action_repository(&self, repository: &str) -> Result<ActionRepository, Error> {
        let body: RepositoryBody = self.get(&format!("/repos/{repository}")).await?;
        Ok(ActionRepository {
            full_name: body.full_name,
            public: body.visibility == crate::api::Visibility::Public,
            default_branch: body.default_branch,
        })
    }

    async fn branch_head(&self, repository: &str, branch: &str) -> Result<String, Error> {
        // Kept for a conditional request: an unchanged head is a 304, which costs no rate limit.
        let path = format!("/repos/{repository}/git/ref/heads/{}", escaped(branch));
        let body: RefBody = self.get(&path).await?;
        Ok(body.object.sha)
    }

    async fn tree(&self, repository: &str, commit: &str) -> Result<Tree, Error> {
        let path = format!("/repos/{repository}/git/trees/{commit}?recursive=1");
        // A commit's tree never changes, and a large one would crowd out the answers kept for polls: never kept.
        let body: TreeBody = self.get_once(&path).await?;
        Ok(Tree {
            truncated: body.truncated,
            files: body
                .tree
                .into_iter()
                .filter(|entry| entry.kind == "blob")
                .map(|entry| TreeFile {
                    path: entry.path,
                    blob: entry.sha,
                    size: entry.size,
                })
                .collect(),
        })
    }

    async fn blob(&self, repository: &str, blob: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let path = format!("/repos/{repository}/git/blobs/{blob}");
        let body: BlobBody = self.get_once(&path).await?;
        if body.size > limit {
            return Err(download_error(
                &path,
                format_args!("larger than {limit} bytes"),
            ));
        }
        if body.encoding != "base64" {
            return Err(download_error(&path, "not base64"));
        }
        base64(&body.content).ok_or_else(|| download_error(&path, "not base64"))
    }

    async fn tarball(
        &self,
        repository: &str,
        commit: &str,
        to: &Path,
        limit: u64,
        received: &AtomicU64,
    ) -> Result<u64, Error> {
        // Anonymous, whatever GitHub answers: only what anyone may download is ever kept.
        let path = format!("/{repository}/tar.gz/{commit}");
        let response = self
            .downloads
            .get(format!("{}{path}", self.codeload))
            .send()
            .await
            .map_err(|source| Error::Http {
                method: Method::GET,
                path: path.clone(),
                source: source.without_url(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Status {
                method: Method::GET,
                path,
                status,
                message: "the archive was not sent".to_owned(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(download_error(
                &path,
                format_args!("larger than {limit} bytes"),
            ));
        }
        write_capped(response, &path, to, limit, received).await
    }

    fn spare_requests(&self) -> Option<u64> {
        let rate_limit = self.spend().rate_limit?;
        (rate_limit.reset > std::time::SystemTime::now()).then_some(rate_limit.remaining)
    }
}

/// Streams `response`'s body into a new file `to`, readable by its owner only until it is published, failing once it
/// passes `limit` bytes. Every chunk received counts in `received`, the one that passes `limit` too.
async fn write_capped(
    mut response: reqwest::Response,
    path: &str,
    to: &Path,
    limit: u64,
    received: &AtomicU64,
) -> Result<u64, Error> {
    let io = |error: std::io::Error| download_error(path, error);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .map_err(io)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut written: u64 = 0;
    loop {
        let chunk = response.chunk().await.map_err(|source| Error::Http {
            method: Method::GET,
            path: path.to_owned(),
            source: source.without_url(),
        })?;
        let Some(chunk) = chunk else { break };
        received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        written += chunk.len() as u64;
        if written > limit {
            return Err(download_error(
                path,
                format_args!("larger than {limit} bytes"),
            ));
        }
        file.write_all(&chunk).await.map_err(io)?;
    }
    file.flush().await.map_err(io)?;
    Ok(written)
}

/// Standard base64 with padding, as GitHub's blobs are, ignoring line breaks; `None` if it is not.
fn base64(text: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        Some(u32::from(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        }))
    };
    let clean: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !clean.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for quad in clean.chunks(4) {
        let pad = quad.iter().rev().take_while(|byte| **byte == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut bits: u32 = 0;
        for byte in &quad[..4 - pad] {
            bits = bits << 6 | value(*byte)?;
        }
        bits <<= if pad == 2 {
            12
        } else {
            6 * u32::from(pad == 1)
        };
        let bytes = bits.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_decodes_what_github_sends_with_its_line_breaks() {
        assert_eq!(
            base64("dXNlczog\nYUBi\n").as_deref(),
            Some(&b"uses: a@b"[..])
        );
        assert_eq!(base64("YQ==").as_deref(), Some(&b"a"[..]));
        assert_eq!(base64("YWI=").as_deref(), Some(&b"ab"[..]));
        assert_eq!(base64("YQ="), None);
        assert_eq!(base64("Y!==",), None);
    }
}

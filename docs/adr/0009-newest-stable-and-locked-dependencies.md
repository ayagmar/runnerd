# 0009. Every dependency newest stable and locked

Status: **Accepted**, in part superseded by [0025](0025-published-runner-images-named-by-reference.md).

## Context

Floating versions and tags make builds change without a commit.

## Decision

A dependency is added at its newest stable release. `Cargo.lock` is committed and CI builds with `--locked`. Git dependencies are pinned by commit, images by digest, GitHub Actions by commit SHA, CI tools by version and SHA-256. `cargo deny` checks advisories, licenses and sources.

## Consequences

Builds are reproducible. Updates are explicit commits.

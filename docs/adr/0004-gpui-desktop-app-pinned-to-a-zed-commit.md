# 0004. GPUI desktop app pinned to a Zed commit

Superseded by [0034](0034-desktop-framework.md).

## Context

The crates.io release of `gpui` (0.2.2) was last published in 2025-10 and lags the version Zed itself uses.

## Decision

The desktop app (`crates/desktop`, to come later) depends on `gpui` from `zed-industries/zed` at a pinned commit.

## Consequences

Builds are reproducible but updating GPUI means picking a new Zed commit by hand. `deny.toml` must allow that git source when the crate lands.

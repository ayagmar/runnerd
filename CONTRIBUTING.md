# Contributing

The checks to run before sending a change are in the README, under "Building". Shell scripts also go through
`shellcheck`, and workflow changes through `actionlint` and `zizmor`.

## CI

All of CI is one workflow, `.github/workflows/ci.yml`. Its `changes` job looks at the paths a pull request touches and
starts only the jobs those paths can affect:

| Job | Runs on a pull request that changes |
|---|---|
| `rust`: fmt, clippy, tests | `crates/**`, `examples/**`, any `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `deny.toml` |
| `deny`: cargo deny | any `Cargo.toml`, `Cargo.lock`, `deny.toml` |
| `image`: builds the `trellis` and `glowly` stages, pushes nothing | `image/**` |
| `site-build`: Astro build | `site/**` |
| `lint`: shellcheck, actionlint, zizmor | `.github/**`, any `*.sh` |

A change to `ci.yml` itself runs all of them. Pushes to `main` run every job and then deploy the site. A weekly
schedule runs `deny`, so new advisories show up without a dependency change. Draft pull requests run only `changes`;
marking one ready for review starts a full run. A new push to a pull request cancels its older run; runs on `main` are
never cancelled.

`gate` is the only required check. It passes when every job it needs passed or was skipped, and fails otherwise. It
also writes the run's job timings to the run summary.

Caches:

- Rust: `Swatinem/rust-cache` keeps the cargo registry and `target/`, keyed on `Cargo.lock` and the toolchain. Only
  `main` saves it; pull requests restore `main`'s copy.
- Image: build layers go to the Actions cache, one scope per stage.
- Site: `actions/setup-node` caches the pnpm store.

The repository gets 10 GB of Actions cache. Check `gh api repos/ayagmar/runnerd/actions/cache/usage` before adding a
cache.

### Adding a job

1. Add a filter for its paths to the `changes` job, an output for it, and set it to `true` for pushes in the `pick`
   step.
2. Start the job with `needs: changes` and `if: needs.changes.outputs.<name> == 'true'`, and add it to `gate`'s
   `needs`.
3. Pin every action to a full commit SHA with the version in a comment. Download tools from a pinned release and
   check their SHA-256.
4. Keep `run:` blocks short; actionlint shellchecks them. Longer logic goes in `.github/scripts/<name>.sh`, which the
   `lint` job shellchecks.
5. Cache what the job downloads or compiles, and compare its time on a second run of the same pull request.

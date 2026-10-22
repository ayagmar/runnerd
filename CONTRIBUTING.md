# Contributing

The checks to run before sending a change, and the rules for where code goes, are in [AGENTS.md](AGENTS.md).

## CI

All of CI is one workflow, `.github/workflows/ci.yml`. Its `changes` job looks at the paths a pull request touches and
starts only the jobs those paths can affect:

| Job | Runs on a pull request that changes |
|---|---|
| `rust`: fmt, clippy (also core without `host`, tests included), tests, `cargo doc`, the client boundary check, `cargo-machete` | `crates/**`, `examples/**`, any `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `.cargo/**`, `clippy.toml`, `.clippy.toml`, `deny.toml`, `.github/scripts/check-boundaries.sh` |
| `deny`: cargo deny | any `Cargo.toml`, `Cargo.lock`, `deny.toml` |
| `image`: builds the `jvm` and `node` stages, pushes nothing | `image/**` |
| `site-build`: Astro build | `site/**` |
| `lint`: shellcheck, actionlint, zizmor | `.github/**`, any `*.sh` |
| `text`: typos, `taplo fmt --check` | any file |

A change to `ci.yml` itself runs all of them. Pushes to `main` run every job and then deploy the site. A weekly
schedule runs `deny`, so new advisories show up without a dependency change. Draft pull requests run only `changes`;
marking one ready for review starts a full run. Editing a pull request (its title, body or base branch) also starts a
run, so a new base is compared again. A newer run of a pull request cancels its older one; runs on `main` are never
cancelled.

`gate` is the only required check. It passes when every job it needs passed or was skipped, and fails otherwise. It
always fails on a draft, so a draft's run can't satisfy it once the pull request is marked ready. It also writes the
run's job timings to the run summary.

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

## Releases

`.github/workflows/release.yml` runs when a tag like `v0.0.1` or `v0.0.1-rc.1` is pushed; a suffix makes a pre-release.
It builds `runnerd` and `runnerctl` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with
[dist](https://github.com/axodotdev/cargo-dist), attests every archive plus `install.sh`, `sha256.sum` and the source
tarball, and creates the release with the archives, a `.sha256` per archive, `sha256.sum`, and `install.sh` set to
install that tag. The tag must match the workspace version in `Cargo.toml`, and a tag without a suffix must be on
`main`, or the `plan` job fails.

The workflow started as dist's generated one and was edited (its header lists how), so `Cargo.toml` sets
`allow-dirty = ["ci"]`. It is not a pull request check; `lint` still runs actionlint and zizmor on it.

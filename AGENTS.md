# Working on runnerd

Rules for anyone changing this repository, people and agents alike. [ARCHITECTURE.md](ARCHITECTURE.md) says where
things live; [docs/adr](docs/adr/README.md) says why.

## The gate

Run before every commit; CI runs the same:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p runnerd-core --no-default-features --all-targets --locked -- -D warnings
cargo clippy -p runnerd-core --no-default-features --features client --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
.github/scripts/check-boundaries.sh
.github/scripts/check-wire-compat.sh
cargo build --locked -p runnerd -p runnerd-cli && .github/scripts/check-docs-commands.sh target/debug
cargo machete
typos
taplo fmt --check
cargo deny check
pnpm -C site build   # once: pnpm -C site install --frozen-lockfile
```

Workflow changes also go through `actionlint` and `zizmor`, shell scripts through `shellcheck`.
[CONTRIBUTING.md](CONTRIBUTING.md) lists the CI jobs and what starts them; `.github/workflows/ci.yml` pins the tool
versions.

## Where code goes

- Domain logic and API types: `crates/core`. Types a client reads go in `api` or `config`, which build without the
  `host` feature.
- Anything that talks to Docker or GitHub, or schedules runners: a `host` module of `crates/core`.
- The daemon's commands and its socket API: `crates/daemon`. Terminal output goes through
  `crates/daemon/src/output.rs` only.
- Clients (`crates/cli`, `crates/web`, later `crates/desktop`) talk to the daemon's API. They never depend on
  `bollard`, `reqwest` or core's `host` feature; `check-boundaries.sh` fails if they do.
- Every module starts with a `//!` doc saying what it owns and what it must not do.

When you change a boundary (a crate's dependencies, what a module owns, an invariant), update ARCHITECTURE.md and
add an ADR in the same change.

## Dependencies

- Add each at its newest stable release; check crates.io or the project's releases, not memory.
- Keep `Cargo.lock` committed and build with `--locked`. Pin git dependencies by commit, images by digest, actions by
  commit SHA, CI tools by version and SHA-256.
- `cargo deny check` must pass; `cargo machete` must find nothing unused.

## Lints

`[workspace.lints]` in `Cargo.toml` is strict: no `unwrap`, `expect`, `panic!` or prints outside tests and the
output module, docs on every public item of `runnerd-core`. An `#[allow]` needs `reason = "..."`. Prefer fixing the
code; if a lint fights the code style everywhere, allow it in `Cargo.toml` with a comment saying why.

## Style

These follow [Google's Rust style guide](https://google.github.io/styleguide/rustguide.html) where clippy can't check
it.

- A `bool` parameter whose meaning a literal call site doesn't show becomes a two-variant enum named for what it
  means there: `next_pane(pane, Direction::Forward)`, not `next_pane(pane, true)`.
- No tuples in `pub` or `pub(crate)` signatures, except newtypes: return a struct with named fields, so two fields
  of the same type can't be swapped.
- A test `assert!` on a bare boolean gets a message naming the expected state, such as
  `assert!(!paused, "o/app stays running")`. Fix the ones you touch.
- A struct's lifetime is named for what it borrows, such as `Usage<'app>`; `'a` is fine on functions.

## Tests

- Test through public behaviour, named in domain terms (see `crates/core/src/scheduler/engine/tests.rs`).
- Never read or change the host: no real `$HOME`, config, socket, Docker or GitHub. Use temp directories and the
  fakes for `GithubApi` and `DockerApi`.
- Tests that need Docker and sysbox are `#[ignore]`d; README's "Building" says how to run them.

## Secrets

- The GitHub token never leaves `github::Github`: no log line, `Debug` output, error or test assertion message
  shows it.
- A JIT config reaches a container only as the root-only file at `docker::JIT_CONFIG_PATH`.
- Never commit tokens, `.env` files or real machine IDs.

## Branches and commits

- Branch names: `<type>/<topic>`, such as `feat/scheduler` or `ci/release-workflow`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `refactor:`, `test:`,
  `docs:`, `ci:`, `chore:`. One logical change per commit.

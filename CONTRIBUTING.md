# Contributing

The checks to run before sending a change, and the rules for where code goes, are in [AGENTS.md](AGENTS.md).

## CI

All of CI is one workflow, `.github/workflows/ci.yml`. Its `changes` job looks at the paths a pull request touches and
starts only the jobs those paths can affect:

| Job | Runs on a pull request that changes |
|---|---|
| `rust`: fmt, clippy (also core without `host`, alone and with `client`, tests included), tests, `cargo doc`, the client boundary check, the wire check against the previous release, `cargo-machete` | `crates/**`, `examples/**`, any `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `.cargo/**`, `clippy.toml`, `.clippy.toml`, `deny.toml`, `.github/scripts/check-boundaries.sh`, `.github/scripts/check-wire-compat.sh` |
| `docs-commands`: builds `runnerd` and `runnerctl` and checks every command and option the docs show against their `--help` | `README.md`, `site/src/content/docs/**`, `crates/**`, any `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.github/scripts/check-docs-commands.sh` |
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
  `main`'s `rust` job saves it; pull requests and `docs-commands` restore that copy.
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

## Testing against a real repository

The tests never reach Docker or GitHub; the ignored ones (README, "Building") start containers but serve no
repository. To run jobs end to end, point a daemon at a sandbox repository of your own.

On a host that already runs runnerd, the two daemons collide in two ways. Both name their runners
with the same machine prefix, and at start `Engine::recover` adopts or removes every runner container on the local
Docker, whoever started it, so the two must not run at once. Stopping the service is not enough on its own: cache
volumes are named for the profile, not the daemon (`runnerd-<profile>-base`, `runnerd-<profile>-slot-<n>`,
`runnerd-pnpm`, `runnerd-maven`, `runnerd-mirror`), so a test profile named like one of the owner's runs on the
owner's slot layers and can reseed the owner's snapshot, and smaller shared-cache limits empty the owner's caches.

1. `systemctl --user stop runnerd`, and wait for it to exit; it waits for running jobs.
2. Write a config that serves only the sandbox repository, with its own `[history] database` under a scratch
   directory ([`examples/sandbox.toml`](examples/sandbox.toml) is one, without the history line):
   - name its profiles so that none matches a profile in the owner's config (`sandbox`, not `jvm`);
   - copy the owner's `[caches]` table unchanged, so the shared pnpm, Maven and mirror volumes keep their limits.

   Check it: `runnerd check-config <scratch>/runnerd.toml`.
3. `runnerd serve --config <scratch>/runnerd.toml --socket <scratch>/runnerd.sock --web off`, and in another
   terminal `runnerctl --socket <scratch>/runnerd.sock status` (or `watch`, `tui`).
4. Push to the sandbox repository and watch its jobs run.
5. Stop the test daemon with `runnerd stop --socket <scratch>/runnerd.sock`, check that `docker ps` lists none of
   its runners, then `systemctl --user start runnerd`.

## The wire between releases

`.github/scripts/check-wire-compat.sh` fails when the previous release cannot read what this version writes, or this
version cannot read what the previous release wrote: the daemon's responses and events, the requests clients send,
the events and admissions the history stores, and what `[notify]` webhooks receive. A new field or variant makes it
fail to compile until `crates/wire-compat/tests/wire_compat/samples.rs` has a sample of it. A new document type,
such as a new response, joins `documents!` in `main.rs` under `new:`, which only this version reads; it moves up
with the others when the first release that has it becomes the previous one.

Fix a failure with a serde default or a fallback (`#[serde(other)]`) where one keeps both sides reading. When the
break is the point, write the ADR that says what an older client or daemon does with it, and add one line to
`ACCEPTED` naming the item the failure names and the ADR. A new event type needs neither: clients and the history
skip events they do not know.

The check's own `crates/wire-compat/Cargo.lock` locks runnerd-core's dependencies too. After changing them, run
`cargo check --manifest-path crates/wire-compat/Cargo.toml --tests` and commit that lock as well, or the check fails
under `--locked`.

## Releases

`.github/workflows/release.yml` runs when a tag like `v0.0.1` or `v0.0.1-rc.1` is pushed; a suffix makes a pre-release.
The tag must match the workspace version in `Cargo.toml`, and a tag without a suffix must be on `main`, or the
`plan` job fails. To cut one:

1. On a branch, bump `version` under `[workspace.package]` in `Cargo.toml` and the `version` of the internal
   dependencies under `[workspace.dependencies]` (`runnerd-core`, `runnerd-web`), then run `cargo check` and
   `cargo check --manifest-path crates/wire-compat/Cargo.toml --tests` without `--locked` so both `Cargo.lock`s
   follow.
2. Update every place that names the version. `git grep -nF <old version> -- ':!Cargo.lock' ':!site/pnpm-lock.yaml'`
   finds them; today they are:
   - `README.md`: the installer's `--version` example;
   - `examples/runnerd.toml`: each profile's `image` tag (the doctor tests compare it with the crate version);
   - `crates/core/src/config.rs`: the `image` key's doc comment;
   - the site: `install.md`, `configuration.mdx`, `clients.mdx`, `src/pages/index.astro` and the TUI frames in
     `src/assets/tui/`, which are recaptured rather than edited.

   `crates/wire-compat` names the previous release, not this one; it moves after the tag (step 6).
   Tests that use a version only as an example, such as `store/tests.rs`'s "as runnerd 0.0.5 stored it", stay, and
   so does the site's `upgrading.md`, which names the versions each section is about. Give it a section for the new
   release when an upgrade asks anything of the owner: a migration, a changed image, a new default.
3. Merge the pull request, then tag the merge commit on `main` and push the tag:
   `git tag v0.0.6 && git push origin v0.0.6`.
4. The release run then:
   - builds `runnerd` and `runnerctl` for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with
     [dist](https://github.com/axodotdev/cargo-dist). dist names each archive for its crate, so `runnerctl` ships in
     `runnerd-cli-<target>.tar.xz`; `install.sh` and the crates' `[package.metadata.binstall]` expect those names;
   - builds every stage of `image/Dockerfile` from the tag for linux/amd64 (the `images` job,
     [`images.yml`](.github/workflows/images.yml)), pushes it to `ghcr.io/ayagmar/runnerd-runner:<stage>-<version>`
     labelled `runnerd.version=<version>`, which `runnerd doctor` compares with its own version, and attests it;
   - attests every archive plus `install.sh`, `sha256.sum` and the source tarball, and creates the GitHub release
     with the archives, a `.sha256` per archive, `sha256.sum`, and `install.sh` set to install that tag;
   - publishes `runnerd-core`, `runnerd-web`, `runnerd` and `runnerd-cli` to crates.io (the `publish-crates` job)
     with [trusted publishing](https://crates.io/docs/trusted-publishing): a token that lasts 30 minutes, minted
     from the run's OIDC identity, so no crates.io token is stored.
5. Check the result: the images are public
   (an anonymous pull, with an empty Docker config so the host's login stays:
   `docker --config "$(mktemp -d)" pull ghcr.io/ayagmar/runnerd-runner:base-<version>`), `install.sh` installs the
   new version, and crates.io lists it.
6. Make the new release the one the wire check reads against
   ([ADR 0061](docs/adr/0061-the-previous-release-reads-the-wire.md)). On a branch from the tagged commit, in this
   order:
   - in `crates/wire-compat/Cargo.toml`, set `runnerd-core-previous`'s `rev` to the tag's commit
     (`git rev-parse v0.0.6^{commit}`) and its comment to the tag, and `PREVIOUS` in
     `crates/wire-compat/tests/wire_compat/main.rs` to the tag; run `cargo check --manifest-path
     crates/wire-compat/Cargo.toml --tests` so its `Cargo.lock` follows;
   - add to `documents!` the document types the release added, with their samples, which now compile against the
     previous release;
   - write its fixtures: `cargo test --manifest-path crates/wire-compat/Cargo.toml --target-dir target/wire-compat
     --test wire_compat -- --ignored write_fixtures` replaces `crates/wire-compat/fixtures/` with its samples, one
     file per document (the check fails on a document without one);
   - run `.github/scripts/check-wire-compat.sh`. It names every `ACCEPTED` entry the new release no longer breaks;
     remove them.

A new package on ghcr.io has so far come out public, as the repository is. If one does not, make it public once:
the package's Package settings → Danger Zone → Change visibility → Public. To publish the images of a release that
has none, run the workflow by hand: `gh workflow run images.yml -f tag=v0.0.4`. It builds the tag's `image/`, so
nothing is retagged.

Trusted publishing is set up: each crate had its first version published by hand, has `release.yml` as its trusted
publisher on crates.io, and the repository variable `CRATES_IO_TRUSTED_PUBLISHING` is `true`. A new crate needs the
same before a release can publish it.

The workflow started as dist's generated one and was edited (its header lists how), so `Cargo.toml` sets
`allow-dirty = ["ci"]`. It is not a pull request check; `lint` still runs actionlint and zizmor on it.

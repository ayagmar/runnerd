# runnerd

runnerd runs ephemeral GitHub Actions runners on a Linux host: one fresh container per job, from one pool shared by the repositories you configure, kept inside a memory and CPU budget.

It is in early development. Today it only reads and checks its config; it does not start runners yet.

Documentation: <https://ayagmar.github.io/runnerd/>

## Config

One file, `runnerd.toml`, lists the repositories to serve, the runner profile each one uses, the shared caches and the systemd slice whose limits form the budget. [`examples/runnerd.toml`](examples/runnerd.toml) is a complete example.

```sh
runnerd check-config runnerd.toml
```

This reads the slice's `MemoryMax` from systemd and fails when `slots × memory` over all profiles, plus the Docker Hub mirror, does not fit in it. A slice without a unit file has no `MemoryMax`; the check passes with a warning, since nothing would hold the runners to their caps. To check against a size without systemd, pass it: `--memory-max 33G`.

Sizes take a binary unit (`K`, `M`, `G`, `T`), as Docker and systemd use them.

`isolation = "unsafe-dind"` runs privileged containers, so a job can take over the host. It is refused unless the `[host]` table also sets `allow_unsafe = true`.

## Building

```sh
cargo build --release
```

The toolchain is pinned in `rust-toolchain.toml`. Before sending a change, run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

## License

MIT

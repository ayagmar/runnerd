#!/bin/bash
# Runs as root when the runner exits (entrypoint.sh), before runnerd may keep the stopped container as a failed job's
# record. Deletes what the job added or changed among the runner's dot-files and in the temporary directories, which
# is where the runner keeps its registration and credentials and where tools keep logins and keys (~/.gitconfig,
# ~/.aws, ~/.gnupg, /tmp/deploy.pem), whoever wrote them; then the other files known to hold a job's secrets. The
# Rust toolchains stay, for a shell to re-run a step: ~/.rustup, and ~/.cargo's bin, registry and git, which hold no
# credentials; ~/.cargo's credentials.toml and config.toml, which may, go.
# runnerd refuses to keep a container whose changes still hold any of them (runnerd_core::record::secrets_left).
# The entrypoint touches the marker before the runner starts. $1 is the root to scrub, for runnerd's test; `/` when
# empty.
set -uo pipefail
root=${1:-}
home=$root/home/runner
marker=$root/run/runnerd-since

# Changed since the marker by ctime, which a job cannot set back; without a marker, everything. Mount points (the
# cache layers) are other file systems: they and what is on them stay, as volumes are not part of a record.
since=(-cnewer "$marker")
[ -e "$marker" ] || since=()
delete() {
    local path
    while IFS= read -r -d '' path; do
        mountpoint -q "$path" || rm -rf --one-file-system "$path"
    done
}
# Kept only as directories: a job's file of either name goes.
kept=()
[ -d "$home/.rustup" ] && [ ! -L "$home/.rustup" ] && kept+=(-o -path "$home/.rustup")
if [ -d "$home/.cargo" ] && [ ! -L "$home/.cargo" ]; then
    kept+=(-o -path "$home/.cargo")
    find "$home/.cargo" -xdev -mindepth 1 \( -path "$home/.cargo/bin" -o -path "$home/.cargo/registry" \
        -o -path "$home/.cargo/git" \) -prune -o \( "${since[@]}" -prune -print0 \) 2> /dev/null | delete
fi
find "$home" -xdev -mindepth 1 \( -path "$home/[!.]*" "${kept[@]}" \) -prune -o \( "${since[@]}" -prune -print0 \) \
    2> /dev/null | delete
find "$root/tmp" "$root/var/tmp" -xdev -mindepth 1 "${since[@]}" -prune -print0 2> /dev/null | delete

rm -rf "$root/run/runnerd" "$home/_work/_temp"
# The token actions/checkout puts in a repository's config, if the job ended before its post step took it out.
find "$home/_work" -path "$home/_work/_*" -prune -o -name node_modules -prune -o -path '*/.git/objects' -prune -o \
    -type f \( -path '*/.git/config' -o -path '*/.git/modules/*/config' \) -print0 2> /dev/null \
    | xargs -0 -r sed -i '/extraheader/d'
rm -f "$marker"
exit 0

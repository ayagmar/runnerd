#!/bin/sh
# Installs runnerd and runnerctl from a GitHub release.
#
#   curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/ayagmar/runnerd/releases/latest/download/install.sh | sh -s -- --prefix /opt/runnerd
#
# Downloads the archives for this machine, checks their SHA-256 against the release's checksum files, and copies the
# binaries to PREFIX/bin (default ~/.local/bin). It never uses sudo, installs packages or changes system config.
set -eu

repo=ayagmar/runnerd
# The release workflow replaces "latest" with the release's tag in the copy it publishes.
version=${RUNNERD_VERSION:-latest}
# Empty when HOME is unset; --prefix is then required.
prefix=${HOME:+$HOME/.local}

usage() {
  cat <<EOF
Usage: install.sh [--prefix DIR] [--version TAG]

  --prefix DIR   install into DIR/bin (default: ~/.local)
  --version TAG  release to install, such as v0.0.1 (default: $version)
EOF
}

die() {
  echo "install.sh: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift 2 ;;
    --prefix=*) prefix=${1#--prefix=}; shift ;;
    --version) [ $# -ge 2 ] || die "--version needs a tag"; version=$2; shift 2 ;;
    --version=*) version=${1#--version=}; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

[ -n "$prefix" ] || die "HOME is not set; pass --prefix DIR"
[ "$(uname -s)" = Linux ] || die "runnerd runs on Linux only, not $(uname -s)"
case "$(uname -m)" in
  x86_64|amd64) target=x86_64-unknown-linux-musl ;;
  aarch64|arm64) target=aarch64-unknown-linux-musl ;;
  *) die "no runnerd build for $(uname -m); releases cover x86_64 and aarch64" ;;
esac
for tool in curl tar xz sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

if [ "$version" = latest ]; then
  base=https://github.com/$repo/releases/latest/download
else
  base=https://github.com/$repo/releases/download/$version
fi
# Tests point this at a directory of tampered archives; nothing else should set it.
base=${RUNNERD_DOWNLOAD_BASE:-$base}
bindir=$prefix/bin

tmp=$(mktemp -d)
# Also removes copies left in bindir when the install stops before renaming them.
trap 'rm -rf "$tmp" "$bindir/.runnerd.new" "$bindir/.runnerctl.new"' EXIT
trap 'exit 1' INT TERM

# Download and verify every archive before installing anything. Archives are named for the crate: runnerctl comes
# in runnerd-cli's.
for crate in runnerd runnerd-cli; do
  case "$crate" in
    runnerd) app=runnerd ;;
    runnerd-cli) app=runnerctl ;;
  esac
  archive=$crate-$target.tar.xz
  echo "downloading $base/$archive"
  curl -fsSL -o "$tmp/$archive" "$base/$archive" || die "could not download $archive"
  curl -fsSL -o "$tmp/$archive.sha256" "$base/$archive.sha256" || die "could not download $archive.sha256"
  expected=$(cut -d ' ' -f 1 "$tmp/$archive.sha256")
  actual=$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)
  [ -n "$expected" ] && [ "$expected" = "$actual" ] \
    || die "checksum mismatch for $archive: expected $expected, got $actual; nothing was installed"
  tar -xJf "$tmp/$archive" -C "$tmp"
  [ -f "$tmp/$crate-$target/$app" ] || die "$archive does not contain $app"
  mv "$tmp/$crate-$target/$app" "$tmp/$app"
done

mkdir -p "$bindir" || die "cannot create $bindir; pick another directory with --prefix"
[ -w "$bindir" ] || die "$bindir is not writable; pick another directory with --prefix"
# Copy both binaries next to their targets first, then rename both, so a failed copy (a full disk) leaves the old pair
# in place, and a running binary is replaced in one step.
for app in runnerd runnerctl; do
  cp "$tmp/$app" "$bindir/.$app.new"
  chmod 755 "$bindir/.$app.new"
done
for app in runnerd runnerctl; do
  mv -f "$bindir/.$app.new" "$bindir/$app"
done
echo "installed $("$bindir/runnerd" --version) and runnerctl to $bindir"

echo
echo "runnerd doctor:"
"$bindir/runnerd" doctor || true

echo
echo "Next steps:"
case ":$PATH:" in
  *":$bindir:"*) ;;
  *) echo "- add $bindir to your PATH" ;;
esac
echo "- fix what runnerd doctor reports above: runners need Docker with the sysbox runtime"
echo "  (https://github.com/nestybox/sysbox) and a systemd slice whose MemoryMax is their budget"
echo "- write a runnerd.toml (https://github.com/$repo/blob/main/examples/runnerd.toml) and check it:"
echo "  runnerd doctor runnerd.toml"

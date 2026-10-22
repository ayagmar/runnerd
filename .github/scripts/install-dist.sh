#!/bin/sh
# Installs the pinned dist release for this runner's architecture into $RUNNER_TEMP/dist and adds it to the PATH.
set -eu

version=0.33.1
case "$(uname -m)" in
  x86_64)
    target=x86_64-unknown-linux-gnu
    sha256=da68e159d5fed1ee61b00284da444e719927dbd0b7d0e2c34e57a04414453cf9
    ;;
  aarch64)
    target=aarch64-unknown-linux-gnu
    sha256=e883f4c6c17f2458d6ced950d978c04b45423ed3a5fdbebcb8f8de568389e1c1
    ;;
  *)
    echo "no pinned dist for $(uname -m)" >&2
    exit 1
    ;;
esac

dir="$RUNNER_TEMP/dist"
mkdir -p "$dir"
curl -sSfL -o "$dir/dist.tar.xz" \
  "https://github.com/axodotdev/cargo-dist/releases/download/v$version/cargo-dist-$target.tar.xz"
echo "$sha256  $dir/dist.tar.xz" | sha256sum -c -
tar -xJf "$dir/dist.tar.xz" -C "$dir" --strip-components=1 "cargo-dist-$target/dist"
echo "$dir" >> "$GITHUB_PATH"
"$dir/dist" --version

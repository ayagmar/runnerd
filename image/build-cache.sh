# shellcheck shell=bash
# Sourced by entrypoint.sh, and by runnerd's build cache test. The shared build cache (docs/adr/0052): runnerd serves
# it on a socket of this runner's own, which it mounts at /run/runnerd-build-cache, and start_build_cache, run as root
# before the runner starts, forwards it to 127.0.0.1 for the tools and points them at it. A job reaches only its own
# runner's socket; runnerd decides from GitHub whether its uploads are kept. Without the socket (the cache off, or
# the repository opted out), the tools are left as they are.

BUILD_CACHE_SOCKET=/run/runnerd-build-cache/cache.sock
BUILD_CACHE_PORT=41771
BUILD_CACHE_GRADLE_INIT=/usr/local/share/runnerd/build-cache.gradle

start_build_cache() {
    # GRADLE_USER_HOME is the image's, in the slot's cache layer (Dockerfile).
    local init="$GRADLE_USER_HOME/init.d/runnerd-build-cache.gradle"
    # From a slot's layer or a snapshot: put back below, or gone with the cache.
    rm -f "$init"
    [ -S "$BUILD_CACHE_SOCKET" ] && command -v socat > /dev/null || return 0
    # As root, before any job: no job can take the port or stop the forwarder.
    socat "TCP-LISTEN:$BUILD_CACHE_PORT,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$BUILD_CACHE_SOCKET" \
        > /dev/null 2>&1 &
    export RUNNERD_BUILD_CACHE_URL="http://127.0.0.1:$BUILD_CACHE_PORT"
    # Gradle, for builds that turn the build cache on and set no remote cache of their own.
    # The directories stay the runner's: setup-gradle adds its own init scripts.
    install -d -o runner -g runner "$GRADLE_USER_HOME" "${init%/*}"
    install -m 644 "$BUILD_CACHE_GRADLE_INIT" "$init"
    # sccache, for builds that run it (RUSTC_WRAPPER=sccache); any other backend a workflow sets takes precedence.
    # Without its start-up check, a build cache that does not answer is a miss, not an sccache that fails to start.
    export SCCACHE_WEBDAV_ENDPOINT="$RUNNERD_BUILD_CACHE_URL/sccache" SCCACHE_WEBDAV_DISABLE_CREATE_DIR=true \
        SCCACHE_SKIP_CACHE_CHECK=true
    # Bazel reads the system rc first: a workspace's own .bazelrc overrides it. Every output is downloaded, so a
    # build never depends on an entry runnerd did not keep.
    printf 'build --remote_cache=%s/bazel\nbuild --remote_download_outputs=all\n' "$RUNNERD_BUILD_CACHE_URL" \
        > /etc/bazel.bazelrc
}

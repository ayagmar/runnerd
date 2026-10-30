# shellcheck shell=bash
# Sourced by entrypoint.sh, and by runnerd's build cache test. The shared build cache (docs/adr/0052): runnerd serves
# it on a socket of this runner's own, which it mounts at /run/runnerd-build-cache, and start_build_cache, run as root
# before the runner starts, forwards it to 127.0.0.1 for the tools and points them at it. A job reaches only its own
# runner's socket; runnerd decides from GitHub whether its uploads are kept. Without the socket (the cache off, or
# the repository opted out), the tools are left as they are.

BUILD_CACHE_SOCKET=/run/runnerd-build-cache/cache.sock
BUILD_CACHE_PORT=41771
BUILD_CACHE_GRADLE_INIT=/usr/local/share/runnerd/build-cache.gradle
# GitHub Actions' cache (docs/adr/0082), when runnerd serves it on this runner's socket.
ACTIONS_CACHE_MARKER=/run/runnerd-build-cache/actions
ACTIONS_CACHE_PORT=41772
ACTIONS_CACHE_PRELOAD=/usr/local/lib/runnerd-actions-cache.cjs

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
    start_actions_cache
}

# GitHub Actions' cache, for actions/cache, the setup-* actions' caches and BuildKit's type=gha. BuildKit runs in a
# container of this container's Docker Engine, whose loopback is its own, so the forwarder listens on this
# container's addresses; the rule drops the port on eth0 and its like, where other containers and the host arrive,
# and jobs, without root, cannot remove it. The runner's node, wrapped (node.sh), loads the preload that points each
# node action at it (runnerd-actions-cache.cjs) while RUNNERD_ACTIONS_CACHE_PORT is set. Without the marker or the
# rule, jobs use GitHub's cache service.
start_actions_cache() {
    [ -f "$ACTIONS_CACHE_MARKER" ] && [ -f "$ACTIONS_CACHE_PRELOAD" ] || return 0
    iptables -I INPUT -i eth+ -p tcp --dport "$ACTIONS_CACHE_PORT" -j DROP 2> /dev/null || return 0
    socat "TCP4-LISTEN:$ACTIONS_CACHE_PORT,fork,reuseaddr" "UNIX-CONNECT:$BUILD_CACHE_SOCKET" > /dev/null 2>&1 &
    export RUNNERD_ACTIONS_CACHE_PORT=$ACTIONS_CACHE_PORT
}

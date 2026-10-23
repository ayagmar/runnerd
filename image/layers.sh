# shellcheck shell=bash
# shellcheck disable=SC2034 # used by the scripts that source this file
# Sourced by entrypoint.sh, job-started.sh, job-cleanup.sh and seed.sh, and by runnerd's cache test.

# The slot's copy-on-write layers, as name:path. Each path shows the profile's read-only snapshot
# /base/<name> (seed.sh) over what the image has there, with this slot's own changes on top in
# /state/<name>/upper.
# An image without a layer's path (base has no Android SDK) skips that layer.
LAYERS=(cache:/home/runner/.cache tools:/home/runner/_work/_tool android:/opt/android-sdk)

# Files in the cache layer where a job may leave a registry token or its own settings: cargo's credentials and
# config, Gradle's properties (signing keys, publishing credentials). The layer outlives the job, so no container
# sees one from the slot's changes or the snapshot (mount_layers), and a new snapshot holds none (seed.sh); the
# image's own copies, if any, stay.
CACHE_SECRETS=(cargo/credentials cargo/credentials.toml cargo/config cargo/config.toml gradle/gradle.properties)

# Disk budgets per slot, in GB. A layer whose changes outgrow LAYER_BUDGET_GB starts over from the snapshot when
# a container starts; Docker images and build cache beyond DOCKER_BUDGET_GB are pruned after a job.
LAYER_BUDGET_GB=4
DOCKER_BUDGET_GB=20

# Size of a directory in GB, rounded up; 0 when it does not exist.
size_gb() {
    local size
    size=$(du -sx --block-size=1G "$1" 2> /dev/null | cut -f1) || true
    echo "${size:-0}"
}

# Bytes the container received on eth0. From /proc/net/dev, which is the container's own under sysbox, where
# /sys/class/net shows the host's interfaces. The count follows the colon without a space once it is long.
rx_bytes() {
    sed -n 's/^ *eth0: *\([0-9]*\).*/\1/p' /proc/net/dev
}

# Mounts each layer over its path: this slot's changes in /state/<name> over the profile's snapshot in /base/<name>
# (when there is one) over the image. /state is this slot's volume, which also holds its Docker Engine's data
# (daemon.json). A layer whose changes outgrew LAYER_BUDGET_GB starts over from the snapshot. CACHE_SECRETS go
# from the slot's changes while the layer is unmounted, as overlayfs does not define changes to a mounted upper;
# the snapshot's, from a snapshot seeded before seed.sh dropped them, are then deleted through the mounted layer,
# which leaves a whiteout in the slot's changes that the next mount drops and makes again.
mount_layers() {
    local layer name path lower
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*} path=${layer#*:}
        [ -d "$path" ] || continue
        [ "$(size_gb "/state/$name")" -le "$LAYER_BUDGET_GB" ] || rm -rf "/state/$name"
        lower=$path
        [ ! -d "/base/$name" ] || lower=/base/$name:$lower
        mkdir -p "/state/$name/upper" "/state/$name/work"
        [ "$name" != cache ] || drop_cache_secrets "/state/$name/upper"
        chown runner:runner "/state/$name/upper"
        # Full copies on copy-up and no index, so an upper never depends on the snapshot it was made against.
        mount -t overlay overlay \
            -o "lowerdir=$lower,upperdir=/state/$name/upper,workdir=/state/$name/work,index=off,metacopy=off" "$path"
        [ "$name" != cache ] || hide_snapshot_secrets "$path"
    done
}

# Deletes CACHE_SECRETS from a copy of the cache layer: a slot's changes or a snapshot.
drop_cache_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        rm -f "$1/$file"
    done
}

# Deletes from the mounted cache layer at $1 the CACHE_SECRETS the profile's snapshot holds.
hide_snapshot_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        [ ! -e "/base/cache/$file" ] || rm -f "$1/$file"
    done
}

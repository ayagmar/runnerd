# shellcheck shell=bash
# shellcheck disable=SC2034 # used by the scripts that source this file
# Sourced by entrypoint.sh, job-started.sh, job-cleanup.sh and seed.sh, and by runnerd's cache tests.

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
# a container starts; Docker images and build cache beyond DOCKER_BUDGET_GB are pruned after a job and when a
# container starts. runnerd sets the slot's Docker budget, its share of the disk (docs/adr/0054), in the container's
# RUNNERD_DOCKER_BUDGET_GB; the entrypoint keeps it in DOCKER_BUDGET_FILE for job-cleanup.sh, which sudo starts
# without that environment. Without one, 20.
LAYER_BUDGET_GB=4
DOCKER_BUDGET_FILE=/run/runnerd-docker-budget-gb
DOCKER_BUDGET_GB=$(cat "$DOCKER_BUDGET_FILE" 2> /dev/null || echo 20)

# Where the slot's Docker Engine keeps its images, layers and build cache: the slot's volume.
DOCKER_DATA_ROOT=/state/docker

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

# Starts the container's Docker Engine on DOCKER_DATA_ROOT and waits up to a minute for it; prints its log if it
# does not come up. The data root is a flag, never daemon.json's data-root: sysbox reads that key from the image and
# mounts a directory of its own over it, which it deletes with the container, so no job would find the last one's
# images.
start_docker() {
    dockerd --group docker --data-root "$DOCKER_DATA_ROOT" > /var/log/dockerd.log 2>&1 &
    for _ in $(seq 60); do
        docker info > /dev/null 2>&1 && return 0
        sleep 1
    done
    cat /var/log/dockerd.log
    return 1
}

# Keeps the Docker budget runnerd set in RUNNERD_DOCKER_BUDGET_GB in DOCKER_BUDGET_FILE, root's, which no job can
# change. A value that is not a number of GB leaves the default.
keep_docker_budget() {
    [[ ${RUNNERD_DOCKER_BUDGET_GB:-} =~ ^[0-9]+$ ]] || return 0
    echo "$RUNNERD_DOCKER_BUDGET_GB" > "$DOCKER_BUDGET_FILE"
}

# Mounts each layer over its path: this slot's changes in /state/<name> over the profile's snapshot in /base/<name>
# (when there is one) over the image. /state is this slot's volume, which also holds its Docker Engine's data
# (DOCKER_DATA_ROOT). A layer whose changes outgrew LAYER_BUDGET_GB starts over from the snapshot. CACHE_SECRETS go
# from the slot's changes while the layer is unmounted, as overlayfs does not define changes to a mounted upper;
# the snapshot's, from a snapshot seeded before seed.sh dropped them, are then deleted through the mounted layer,
# which leaves a whiteout in the slot's changes that the next mount drops and makes again. The whiteout would hide
# the image's own copy too, so that copy is put back over it.
mount_layers() {
    local layer name path lower image_copies
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*} path=${layer#*:}
        [ -d "$path" ] || continue
        [ "$(size_gb "/state/$name")" -le "$LAYER_BUDGET_GB" ] || rm -rf "/state/$name"
        lower=$path
        [ ! -d "/base/$name" ] || lower=/base/$name:$lower
        mkdir -p "/state/$name/upper" "/state/$name/work"
        if [ "$name" = cache ]; then
            drop_cache_secrets "/state/$name/upper"
            image_copies=$(mktemp -d)
            keep_image_secrets "$path" "$image_copies"
        fi
        chown runner:runner "/state/$name/upper"
        # Full copies on copy-up and no index, so an upper never depends on the snapshot it was made against.
        mount -t overlay overlay \
            -o "lowerdir=$lower,upperdir=/state/$name/upper,workdir=/state/$name/work,index=off,metacopy=off" "$path"
        [ "$name" != cache ] || hide_snapshot_secrets "$path" "$image_copies"
    done
}

# Deletes CACHE_SECRETS from a copy of the cache layer: a slot's changes or a snapshot.
drop_cache_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        rm -f "$1/$file"
    done
}

# Whether the profile's snapshot holds the cache layer's file $1.
in_snapshot() {
    [ -e "/base/cache/$1" ] || [ -L "/base/cache/$1" ]
}

# Copies into $2 the image's own copies, in the unmounted cache layer's path $1, of the CACHE_SECRETS the profile's
# snapshot also holds.
keep_image_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        if in_snapshot "$file" && [ -f "$1/$file" ] && [ ! -L "$1/$file" ]; then
            mkdir -p "$2/${file%/*}"
            cp -a "$1/$file" "$2/$file"
        fi
    done
}

# Deletes from the mounted cache layer at $1 the CACHE_SECRETS the profile's snapshot holds, and puts back the
# image's copies keep_image_secrets kept in $2, which it then removes.
hide_snapshot_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        in_snapshot "$file" || continue
        rm -f "$1/$file"
        [ ! -e "$2/$file" ] || cp -a "$2/$file" "$1/$file"
    done
    rm -rf "$2"
}

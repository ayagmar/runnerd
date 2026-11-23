# shellcheck shell=bash
# shellcheck disable=SC2034 # used by the scripts that source this file
# Sourced by entrypoint.sh, job-started.sh, job-cleanup.sh and seed.sh, and by runnerd's cache tests.

# The slot's copy-on-write layers, as name:path. Each path shows the profile's read-only snapshot
# /base/<name> (seed.sh) over the whiteouts of what the snapshot no longer holds of the image, in
# /base/<name>.hidden, over what the image has there, with this slot's own changes on top in /state/<name>/upper.
# An image without a layer's path (base has no Android SDK) skips that layer.
LAYERS=(cache:/home/runner/.cache tools:/home/runner/_work/_tool android:/opt/android-sdk)

# The layers a snapshot over its limit starts over from the image first (seed.sh), in order, until the rest fits: the
# tools a job's setup step downloads again, then Android's SDK packages. The cache layer, whose Gradle and npm caches a
# job is slowest without, is never among them: a snapshot they do not bring under its limit starts over whole.
START_OVER_FIRST=(tools android)

# Files and directories in the cache layer where a job may leave a registry token or its own settings: cargo's
# credentials and config, Gradle's properties (signing keys, publishing credentials) and init scripts (credentials, and
# code every later build runs), the Maven wrapper's settings (MAVEN_USER_HOME), Hugging Face's tokens (HF_HOME). The
# layer outlives the job, so no container sees one from the slot's changes or the snapshot (mount_layers), and a new
# snapshot holds none (seed.sh); the image's own copies, if any, stay.
CACHE_SECRETS=(cargo/credentials cargo/credentials.toml cargo/config cargo/config.toml gradle/gradle.properties
    gradle/init.d gradle/init.gradle gradle/init.gradle.kts m2/settings.xml m2/settings-security.xml
    huggingface/token huggingface/stored_tokens)

# Present while seed.sh changes the profile's snapshot or a slot's layers (docs/adr/0066). One a reseed that did not
# finish left means the snapshot and the slots' changes may not match: a container leaves out both.
RESEED_MARKER=/base/.reseeding

# Which snapshot a slot's layers were made against: seed.sh writes a new value to the profile's with each change to
# the snapshot, and mount_layers keeps the one the slot's layers are on in the slot's. A slot whose value differs, as
# one a lower `slots` closed while its profile reseeded, starts over from the snapshot. Missing is 0.
GENERATION=/base/.generation
SLOT_GENERATION=/state/.generation

# Disk budgets per slot, in GB. A layer whose changes outgrow LAYER_BUDGET_GB starts over from the snapshot when
# a container starts. The build cache keeps layers within about half of DOCKER_BUDGET_GB and, with cache mounts,
# within three quarters, cache mounts given up last (BuildKit's GC, docker_gc_policy, and job-cleanup.sh after a job
# and when a container starts), and a Docker store still over it loses its images, then everything. runnerd sets the slot's Docker budget, its share of the disk
# (docs/adr/0054), in the container's RUNNERD_DOCKER_BUDGET_GB; start_docker keeps it in DOCKER_BUDGET_FILE for
# job-cleanup.sh, which sudo starts without that environment. Without one, 20.
LAYER_BUDGET_GB=4
DOCKER_BUDGET_FILE=/run/runnerd-docker-budget-gb
DOCKER_BUDGET_GB=$(cat "$DOCKER_BUDGET_FILE" 2> /dev/null || echo 20)

# Where the slot's Docker Engine keeps its images, layers and build cache: the slot's volume.
DOCKER_DATA_ROOT=/state/docker

# Touched as the runner starts (entrypoint.sh), so present only once a job may have run: job-cleanup.sh measures and
# reports the store only then (docs/adr/0156).
JOB_SINCE_FILE=/run/runnerd-since
# When the slot's last job began, its runner's start in seconds since the epoch, on the slot's volume: a layer used
# since then is one the next job most likely needs, which the routine prune and BuildKit's GC keep (recent_use).
LAST_JOB_FILE=/state/.runnerd-last-job
# What job-cleanup.sh measured of the store after the job, root's, which the entrypoint prints once the runner exits:
# `held=<bytes> share=<GiB>`.
STORE_REPORT_FILE=/run/runnerd-docker-store

# Disk a directory takes in bytes, as size_gb counts it, not its files' apparent size; 0 when it does not exist.
size_bytes() {
    local size
    size=$(du -sx --block-size=1 "$1" 2> /dev/null | cut -f1) || true
    echo "${size:-0}"
}

# Seconds back from now that a layer last used counts as recent: since the slot's last job began, and an hour more,
# which keeps what this job uses while it runs. A slot no job ran on yet gets the hour.
recent_use() {
    local last now
    now=$(date +%s)
    last=$(cat "$LAST_JOB_FILE" 2> /dev/null) || true
    [[ $last =~ ^[0-9]+$ ]] && [ "$last" -le "$now" ] || last=$now
    echo $((now - last + 3600))
}

# Size of a directory in GB, rounded up; 0 when it does not exist.
size_gb() {
    local size
    size=$(du -sx --block-size=1G "$1" 2> /dev/null | cut -f1) || true
    echo "${size:-0}"
}

# Bytes a slot's changes to the layers hold under $1, its volume as mounted: what seed.sh weighs against runnerd's
# reseed_at. Not its Docker Engine's data (DOCKER_DATA_ROOT), which a reseed leaves alone.
layer_changes() {
    local layer changes=0 size
    for layer in "${LAYERS[@]}"; do
        [ -d "$1/${layer%%:*}/upper" ] || continue
        size=$(du -sxb "$1/${layer%%:*}/upper" | cut -f1)
        changes=$((changes + size))
    done
    echo "$changes"
}

# Bytes the container received on eth0. From /proc/net/dev, which is the container's own under sysbox, where
# /sys/class/net shows the host's interfaces. The count follows the colon without a space once it is long.
rx_bytes() {
    sed -n 's/^ *eth0: *\([0-9]*\).*/\1/p' /proc/net/dev
}

# Starts the container's Docker Engine on DOCKER_DATA_ROOT and waits up to a minute for it, or until dockerd exits.
# The data root is a flag, never daemon.json's data-root: sysbox reads that key from the image and mounts a
# directory of its own over it, which it deletes with the container, so no job would find the last one's images.
# A store the engine does not start on, as one a crash or a full disk damaged, is only a cache: it is emptied, with
# dockerd's log printed, and the engine started once more. Otherwise every job of the slot would fail on it.
# BuildKit's GC follows the slot's Docker budget, kept first.
start_docker() {
    local attempt pid
    keep_docker_budget
    docker_gc_policy || echo "runnerd: could not write BuildKit's GC policy: it keeps the image's," \
        "sized for a 20 GB share, not the slot's" >&2
    for attempt in 1 2; do
        dockerd --group docker --data-root "$DOCKER_DATA_ROOT" > /var/log/dockerd.log 2>&1 &
        pid=$!
        for _ in $(seq 60); do
            docker info > /dev/null 2>&1 && return 0
            kill -0 "$pid" 2> /dev/null || break
            sleep 1
        done
        cat /var/log/dockerd.log
        [ "$attempt" = 1 ] || return 1
        echo "runnerd: the slot's Docker Engine did not start on its store: emptied it, and starting it again" >&2
        # With the containerd it started, which holds the store's databases open.
        kill -9 "$pid" 2> /dev/null || true
        pkill -9 -x containerd 2> /dev/null || true
        wait "$pid" 2> /dev/null || true
        rm -rf "$DOCKER_DATA_ROOT"
    done
}

# Keeps the Docker budget runnerd set in RUNNERD_DOCKER_BUDGET_GB in DOCKER_BUDGET_FILE, root's: a job reaches it
# only through the container's Docker Engine, with which it can fill the slot's volume anyway. A value that is not a
# number of GB leaves the default.
keep_docker_budget() {
    [[ ${RUNNERD_DOCKER_BUDGET_GB:-} =~ ^[0-9]+$ ]] || return 0
    echo "$RUNNERD_DOCKER_BUDGET_GB" > "$DOCKER_BUDGET_FILE"
    DOCKER_BUDGET_GB=$RUNNERD_DOCKER_BUDGET_GB
}

# Writes BuildKit's GC policy into the engine's daemon.json, sized from DOCKER_BUDGET_GB (docs/adr/0089), over the
# image's own, which is the same policy for the default 20 GB. Build context snapshots and git checkouts go first,
# all of them once the build cache holds over 512 MB, then layers while the unshared build cache holds over half the
# budget, never one used since the slot's last job began (recent_use, docs/adr/0156), and cache mounts
# (`RUN --mount=type=cache`) only while it holds over three quarters, so a package store of up to about three quarters
# of the budget stays. Each rule compares the whole build cache with its limit and removes
# only its type, least recently used first. A policy with only a reserved space keeps no more than that space:
# `defaultReservedSpace` alone kept 512 MB and dropped every larger cache mount within a minute of its build. A
# daemon.json GC rule takes only `type=` filters: Moby drops a `type!=` one, and the rule then matches everything.
docker_gc_policy() {
    local config=/etc/docker/daemon.json
    jq --arg half "$((DOCKER_BUDGET_GB * 512))MB" --arg most "$((DOCKER_BUDGET_GB * 768))MB" \
        --arg recent "until=$(recent_use)s" \
        '.builder.gc = {enabled: true, policy: [
            {filter: ["type=source.local"], maxUsedSpace: "512MB"},
            {filter: ["type=source.git.checkout"], maxUsedSpace: "512MB"},
            {filter: ["type=regular", $recent], maxUsedSpace: $half},
            {all: true, maxUsedSpace: $most}]}' "$config" > "$config.new" && mv "$config.new" "$config"
}

# Mounts each layer over its path: this slot's changes in /state/<name> over the profile's snapshot in /base/<name>
# (when there is one) over the image. /state is this slot's volume, which also holds its Docker Engine's data
# (DOCKER_DATA_ROOT). A layer whose changes outgrew LAYER_BUDGET_GB starts over from the snapshot. CACHE_SECRETS go
# from the slot's changes while the layer is unmounted, as overlayfs does not define changes to a mounted upper;
# the snapshot's, from a snapshot seeded before seed.sh dropped them, are then deleted through the mounted layer,
# which leaves a whiteout in the slot's changes that the next mount drops and makes again. The whiteout would hide
# the image's own copy too, so that copy is put back over it. While RESEED_MARKER is there, each layer is the image
# alone with no changes of the slot's, until the profile's next reseed starts it over. Layers made against another
# snapshot than the profile's (GENERATION) start over from it.
mount_layers() {
    local layer name path lower image_copies generation interrupted=
    if [ -e "$RESEED_MARKER" ]; then
        interrupted=1
        echo "runnerd: the profile's last reseed did not finish: this job's caches start from the image" >&2
    fi
    generation=$(cat "$GENERATION" 2> /dev/null || echo 0)
    if [ "$generation" != "$(cat "$SLOT_GENERATION" 2> /dev/null || echo 0)" ]; then
        # Never the slot's Docker store. A crash before the new value is written only removes them again.
        for layer in "${LAYERS[@]}"; do
            rm -rf "/state/${layer%%:*}"
        done
        echo "$generation" > "$SLOT_GENERATION"
    fi
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*} path=${layer#*:}
        [ -d "$path" ] || continue
        [ -z "$interrupted" ] || rm -rf "/state/$name"
        [ "$(size_gb "/state/$name")" -le "$LAYER_BUDGET_GB" ] || rm -rf "/state/$name"
        lower=$path
        if [ -z "$interrupted" ]; then
            [ ! -d "/base/$name.hidden" ] || lower=/base/$name.hidden:$lower
            [ ! -d "/base/$name" ] || lower=/base/$name:$lower
        fi
        mkdir -p "/state/$name/upper" "/state/$name/work"
        if [ "$name" = cache ]; then
            drop_cache_secrets "/state/$name/upper"
            image_copies=$(mktemp -d)
            keep_image_secrets "$path" "$image_copies"
        fi
        chown runner:runner "/state/$name/upper"
        # Full copies on copy-up and no index, so an upper never depends on the snapshot it was made against.
        mount_layer "$name" "lowerdir=$lower,upperdir=/state/$name/upper,workdir=/state/$name/work,index=off,metacopy=off" \
            "$path"
        [ "$name" != cache ] || hide_snapshot_secrets "$path" "$image_copies"
    done
}

# Mounts the layer $1's overlay with the options $2 over its path $3. While runnerd asks the runner to skip its syncs
# (RUNNERD_SKIP_SYNCS=1, docs/adr/0141), with `volatile` too, so overlayfs syncs nothing of the slot's changes, and
# unmounting the layer, when the container stops, commits nothing of the file system's: what the layer wrote is lost
# to a host crash, which empties the slot's volume before the next runner starts (docs/adr/0142). A kernel or file
# system that refuses the option gets the plain mount, said on the container's output. A volatile mount leaves
# work/work/incompat in its work directory, after which overlayfs refuses to mount the layer again, volatile or not;
# it goes first, whatever runnerd asks now. That is safe because the daemon starts no runner on a volume a crash may
# have torn: a slot that reaches this point either had an orderly unmount or was emptied since.
mount_layer() {
    local incompat=/state/$1/work/work/incompat error
    rm -rf "$incompat"
    if [ "${RUNNERD_SKIP_SYNCS:-}" = 1 ]; then
        error=$(mount -t overlay overlay -o "$2,volatile" "$3" 2>&1) && return 0
        echo "runnerd: the $1 layer's overlay refused volatile (${error%%$'\n'*}): it syncs when unmounted" >&2
        rm -rf "$incompat"
    fi
    mount -t overlay overlay -o "$2" "$3"
}

# Deletes CACHE_SECRETS from a copy of the cache layer: a slot's changes or a snapshot. A directory of theirs that is a
# symlink goes instead: deleting through it would delete outside the copy.
drop_cache_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        if [ -L "$1/${file%/*}" ]; then
            rm -f "$1/${file%/*}"
        else
            rm -rf "${1:?}/$file"
        fi
    done
}

# Whether the profile's snapshot holds the cache layer's file $1, and containers use the snapshot.
in_snapshot() {
    [ ! -e "$RESEED_MARKER" ] && { [ -e "/base/cache/$1" ] || [ -L "/base/cache/$1" ]; }
}

# Copies into $2 the image's own copies, in the unmounted cache layer's path $1, of the CACHE_SECRETS the profile's
# snapshot also holds.
keep_image_secrets() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        if in_snapshot "$file" && { [ -f "$1/$file" ] || [ -d "$1/$file" ]; } && [ ! -L "$1/$file" ]; then
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
        rm -rf "${1:?}/$file"
        [ ! -e "$2/$file" ] || cp -a "$2/$file" "$1/$file"
    done
    rm -rf "$2"
}

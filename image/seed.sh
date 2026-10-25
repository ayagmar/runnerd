#!/bin/bash
# Makes slot 1's view of each layer the profile's new read-only snapshot in /base, then drops every slot's own
# changes, which the snapshot now holds. Runs as root in a throwaway container while none of the profile's slots
# runs a job, with /base and each slot's /state mounted at /workers/<N>. With a first argument, does nothing unless
# slot 1's changes to the layers (not its Docker Engine's data) reach that many bytes. With a second, a snapshot
# that would outgrow that many bytes starts over from the image, which drops what no job changed since, such as a
# superseded tool version, with every slot's changes. Only the image is whole on its own: slot 1's changes hold
# what its jobs wrote, not what they only read from the snapshot, such as the files a cache's updated index names.
# RESEED_MARKER is there from the first change to the snapshot or a slot's layers until the last is on disk; a run
# that finds it, left by one killed in between, starts over from the image whatever its arguments.
set -euo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

# Writes what the snapshot's and every slot's volumes hold to disk.
flush() {
    sync -f /base /workers/*
}

# Puts RESEED_MARKER on disk before the first change, once.
changing=
begin_change() {
    [ -z "$changing" ] || return 0
    : > "$RESEED_MARKER"
    flush
    changing=1
}

# Removes RESEED_MARKER once every change is on disk.
end_change() {
    [ -n "$changing" ] || return 0
    flush
    rm -f "$RESEED_MARKER"
    flush
}

# Drops the snapshot of every layer and every slot's changes.
start_over() {
    begin_change
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*}
        rm -rf "/base/$name" "/base/$name.new" /workers/*/"$name"
    done
    end_change
}

if [ -e "$RESEED_MARKER" ]; then
    echo "the last reseed did not finish: started over from the image"
    start_over
    exit 0
fi

changes=0
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    # A copy a run killed before its first change left.
    rm -rf "/base/$name.new"
    [ ! -d "/workers/1/$name/upper" ] \
        || changes=$((changes + $(du -sxb "/workers/1/$name/upper" | cut -f1)))
done
if [ "$changes" -lt "${1:-0}" ]; then
    echo "not seeded: slot 1 changed $changes bytes"
    exit 0
fi

mkdir -p /mnt/merged
# Mounts slot 1's view of layer $1, its changes over the snapshot, at /mnt/merged.
mount_view() {
    mkdir -p "/base/$1"
    mount -t overlay overlay -o "ro,lowerdir=/workers/1/$1/upper:/base/$1" /mnt/merged
}

max=${2:-0}
if [ "$max" -gt 0 ]; then
    # What the new snapshot would hold: slot 1's view of each layer it changed, the snapshot of each other one.
    size=0
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*}
        if [ -d "/workers/1/$name/upper" ]; then
            mount_view "$name"
            size=$((size + $(du -sxb /mnt/merged | cut -f1)))
            umount /mnt/merged
        elif [ -d "/base/$name" ]; then
            size=$((size + $(du -sxb "/base/$name" | cut -f1)))
        fi
    done
    if [ "$size" -gt "$max" ]; then
        echo "the snapshot would hold $size bytes, over its limit of $max: started over from the image"
        start_over
        exit 0
    fi
fi
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    [ -d "/workers/1/$name/upper" ] || continue
    mount_view "$name"
    cp -a /mnt/merged "/base/$name.new"
    umount /mnt/merged
    # Gradle daemon logs and registry: only meaningful to the daemons of the slot that wrote them. Not through a
    # symlink, which would delete outside the copy.
    [ -L "/base/$name.new/gradle" ] || rm -rf "/base/$name.new/gradle/daemon"
    [ "$name" != cache ] || drop_cache_secrets "/base/$name.new"
    begin_change
    rm -rf "/base/$name"
    mv "/base/$name.new" "/base/$name"
    rm -rf /workers/*/"$name"
    echo "seeded $name: $(du -sh "/base/$name" | cut -f1)"
done
end_change

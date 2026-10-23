#!/bin/bash
# Makes slot 1's view of each layer the profile's new read-only snapshot in /base, then drops every slot's own
# changes, which the snapshot now holds. Runs as root in a throwaway container while none of the profile's slots
# runs a job, with /base and each slot's /state mounted at /workers/<N>. With a first argument, does nothing unless
# slot 1's changes to the layers (not its Docker Engine's data) reach that many bytes. With a second, a snapshot
# that would outgrow that many bytes starts over: from slot 1's changes alone, which drops what no job changed
# since, such as a superseded tool version; from the image alone when those outgrow it too.
set -euo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

changes=0
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    [ ! -d "/workers/1/$name/upper" ] \
        || changes=$((changes + $(du -sxb "/workers/1/$name/upper" | cut -f1)))
done
if [ "$changes" -lt "${1:-0}" ]; then
    echo "not seeded: slot 1 changed $changes bytes"
    exit 0
fi
max=${2:-0}
# What slot 1's changes go over: the snapshot, or nothing.
under=/base
if [ "$max" -gt 0 ]; then
    # At most the new snapshot's size: a change may replace a file the snapshot holds.
    size=$(($(du -sxb /base | cut -f1) + changes))
    if [ "$changes" -gt "$max" ]; then
        echo "slot 1 changed $changes bytes, over the snapshot's limit of $max: started over from the image"
        for layer in "${LAYERS[@]}"; do
            name=${layer%%:*}
            rm -rf "/base/$name" /workers/*/"$name"
        done
        exit 0
    elif [ "$size" -gt "$max" ]; then
        echo "the snapshot would hold up to $size bytes, over its limit of $max: started over from slot 1's changes"
        under=/mnt/empty
        mkdir -p "$under"
    fi
fi
mkdir -p /mnt/merged
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    upper=/workers/1/$name/upper
    if [ ! -d "$upper" ]; then
        # Starting over drops the layer: every slot's changes to it were made against the snapshot that had it.
        [ "$under" = /base ] || rm -rf "/base/$name" /workers/*/"$name"
        continue
    fi
    lower=$under/$name
    [ "$under" = /base ] || lower=$under
    mkdir -p "/base/$name"
    rm -rf "/base/$name.new"
    mount -t overlay overlay -o "ro,lowerdir=$upper:$lower" /mnt/merged
    cp -a /mnt/merged "/base/$name.new"
    umount /mnt/merged
    # Gradle daemon logs and registry: only meaningful to the daemons of the slot that wrote them.
    rm -rf "/base/$name.new/gradle/daemon"
    [ "$name" != cache ] || drop_cache_secrets "/base/$name.new"
    rm -rf "/base/$name"
    mv "/base/$name.new" "/base/$name"
    rm -rf /workers/*/"$name"
    echo "seeded $name: $(du -sh "/base/$name" | cut -f1)"
done

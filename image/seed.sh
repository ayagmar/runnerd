#!/bin/bash
# Makes slot 1's view of each layer the profile's new read-only snapshot in /base, then drops every slot's own
# changes, which the snapshot now holds. Runs as root in a throwaway container while none of the profile's slots
# runs a job, with /base and each slot's /state mounted at /workers/<N>. With a first argument, does nothing unless
# slot 1's changes to the layers (not its Docker Engine's data) reach that many bytes. With a second, a snapshot
# that would outgrow that many bytes starts over from the image, which drops what no job changed since, such as a
# superseded tool version, with every slot's changes. Only the image is whole on its own: slot 1's changes hold
# what its jobs wrote, not what they only read from the snapshot, such as the files a cache's updated index names.
# Slot 1's view is copied without the image, so what its jobs deleted of the image would come back: whiteouts for it
# go in /base/<layer>.hidden, which containers mount between the snapshot and the image (layers.sh).
# RESEED_MARKER is there from the first change to the snapshot or a slot's layers until the last is on disk; a run
# that finds it, left by one killed in between, starts over from the image whatever its arguments.
# With `measure` as its only argument, changes nothing: prints what slot 1 changed, as a reseed would weigh it, or that
# the last reseed did not finish. runnerd runs it with /base and /workers/1 read-only while the profile's runners run,
# so the profile's upkeep stops them only for a reseed that will replace the snapshot.
set -euo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

if [ "${1:-}" = measure ]; then
    if [ -e "$RESEED_MARKER" ]; then
        echo "the last reseed did not finish"
    else
        changes=$(layer_changes /workers/1)
        echo "slot 1 changed $changes bytes"
    fi
    exit 0
fi

# Writes what the snapshot's and every slot's volumes hold to disk.
flush() {
    sync -f /base /workers/*
}

# Puts RESEED_MARKER on disk before the first change, once, and gives the snapshot a new GENERATION: the layers of a
# slot this run does not mount, as one a lower `slots` closed, start over from the new snapshot when it next mounts
# them. A value is never removed, or an older slot without one would match a snapshot without one.
changing=
begin_change() {
    [ -z "$changing" ] || return 0
    : > "$RESEED_MARKER"
    date +%s%N > "$GENERATION"
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
        rm -rf "/base/$name" "/base/$name.new" "/base/$name.hidden" "/base/$name.hidden.new" /workers/*/"$name"
    done
    end_change
}

if [ -e "$RESEED_MARKER" ]; then
    echo "the last reseed did not finish: started over from the image"
    start_over
    exit 0
fi

for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    # What a run killed before its first change left.
    rm -rf "/base/$name.new" "/base/$name.hidden.new"
done
changes=$(layer_changes /workers/1)
if [ "$changes" -lt "${1:-0}" ]; then
    echo "not seeded: slot 1 changed $changes bytes"
    exit 0
fi

mkdir -p /mnt/merged /mnt/view
# Mounts slot 1's view of layer $1, its changes over the snapshot, at /mnt/merged.
mount_view() {
    mkdir -p "/base/$1"
    mount -t overlay overlay -o "ro,lowerdir=/workers/1/$1/upper:/base/$1" /mnt/merged
}

# Whether the cache layer's entry $1 is one of CACHE_SECRETS, or under one that is a directory.
is_secret() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        [ "$file" != "$1" ] && [ "${1#"$file"/}" = "$1" ] || return 0
    done
    return 1
}

# Whether the cache layer's entry $1 is a directory of one of CACHE_SECRETS.
holds_secret() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        [ "${file#"$1"/}" = "$file" ] || return 0
    done
    return 1
}

# Makes the directory $3 of the image's $2, with each one above it, in the whiteouts' layer $1 as the image has them.
make_hidden_dir() {
    local dir=$1${3:+/$3}
    [ ! -d "$dir" ] || return 0
    if [ -n "$3" ]; then
        if [ "${3%/*}" = "$3" ]; then
            make_hidden_dir "$1" "$2" ""
        else
            make_hidden_dir "$1" "$2" "${3%/*}"
        fi
    fi
    mkdir "$dir"
    chown --reference="$2${3:+/$3}" "$dir"
    chmod --reference="$2${3:+/$3}" "$dir"
}

# Puts a whiteout in /base/$1.hidden.new for each entry of the image's $2 that slot 1's view hides: one a job
# deleted, or one under a directory it replaced. The image's CACHE_SECRETS, and what one that is a directory holds,
# get none, as its own copies stay (layers.sh), and a directory or symlink above one none either: only what else such
# a directory held. Nothing under a symlink or a file of the
# view is looked at, and the layer holds only directories it makes.
hide_image_entries() {
    local hidden=/base/$1.hidden.new image=$2 entry done_with='' lower=/base/$1 entries
    [ -d "$image" ] && [ ! -L "$image" ] || return 0
    [ ! -d "/base/$1.hidden" ] || lower=$lower:/base/$1.hidden
    # Listed in full first, so a find that fails fails the reseed instead of leaving whiteouts out.
    entries=$(mktemp)
    find "$image" -mindepth 1 -printf '%P\0' > "$entries"
    mount -t overlay overlay -o "ro,lowerdir=/workers/1/$1/upper:$lower:$image" /mnt/view
    while IFS= read -r -d '' entry; do
        # find lists a directory before what it holds.
        [ -z "$done_with" ] || [ "${entry#"$done_with"/}" = "$entry" ] || continue
        done_with=
        if [ "$1" = cache ] && is_secret "$entry"; then
            done_with=$entry
        elif [ -e "/mnt/view/$entry" ] || [ -L "/mnt/view/$entry" ]; then
            { [ -d "/mnt/view/$entry" ] && [ ! -L "/mnt/view/$entry" ]; } || done_with=$entry
        elif [ "$1" = cache ] && holds_secret "$entry"; then
            if [ -d "$image/$entry" ] && [ ! -L "$image/$entry" ]; then
                make_hidden_dir "$hidden" "$image" "$entry"
            else
                done_with=$entry
            fi
        else
            if [ "${entry%/*}" = "$entry" ]; then
                make_hidden_dir "$hidden" "$image" ""
            else
                make_hidden_dir "$hidden" "$image" "${entry%/*}"
            fi
            mknod "$hidden/$entry" c 0 0
            done_with=$entry
        fi
    done < "$entries"
    umount /mnt/view
    rm -f "$entries"
}

max=${2:-0}
if [ "$max" -gt 0 ]; then
    # What the new snapshot would hold: slot 1's view of each layer it changed, the snapshot of each other one, and
    # the whiteouts there are now.
    size=0
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*}
        [ ! -d "/base/$name.hidden" ] || size=$((size + $(du -sxb "/base/$name.hidden" | cut -f1)))
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
    name=${layer%%:*} path=${layer#*:}
    [ -d "/workers/1/$name/upper" ] || continue
    mount_view "$name"
    cp -a /mnt/merged "/base/$name.new"
    umount /mnt/merged
    hide_image_entries "$name" "$path"
    # Gradle daemon logs and registry: only meaningful to the daemons of the slot that wrote them. Not through a
    # symlink, which would delete outside the copy.
    [ -L "/base/$name.new/gradle" ] || rm -rf "/base/$name.new/gradle/daemon"
    [ "$name" != cache ] || drop_cache_secrets "/base/$name.new"
    begin_change
    rm -rf "/base/$name" "/base/$name.hidden"
    mv "/base/$name.new" "/base/$name"
    [ ! -d "/base/$name.hidden.new" ] || mv "/base/$name.hidden.new" "/base/$name.hidden"
    rm -rf /workers/*/"$name"
    echo "seeded $name: $(du -sh "/base/$name" | cut -f1)"
done
end_change

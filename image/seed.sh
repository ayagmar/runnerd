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
        rm -rf "/base/$name" "/base/$name.new" "/base/$name.hidden" "/base/$name.hidden.new" /workers/*/"$name"
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
    # What a run killed before its first change left.
    rm -rf "/base/$name.new" "/base/$name.hidden.new"
    [ ! -d "/workers/1/$name/upper" ] \
        || changes=$((changes + $(du -sxb "/workers/1/$name/upper" | cut -f1)))
done
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

# Whether the cache layer's entry $1 is one of CACHE_SECRETS.
is_secret() {
    local file
    for file in "${CACHE_SECRETS[@]}"; do
        [ "$file" != "$1" ] || return 0
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
# deleted, or one under a directory it replaced. The image's CACHE_SECRETS get none, as its own copies stay
# (layers.sh), and a directory of theirs none either, only what else it held. Nothing under a symlink or a file of the
# view is looked at, and the layer holds only directories it makes.
hide_image_entries() {
    local hidden=/base/$1.hidden.new image=$2 entry done_with='' lower=/base/$1
    [ -d "$image" ] && [ ! -L "$image" ] || return 0
    [ ! -d "/base/$1.hidden" ] || lower=$lower:/base/$1.hidden
    mount -t overlay overlay -o "ro,lowerdir=/workers/1/$1/upper:$lower:$image" /mnt/view
    while IFS= read -r -d '' entry; do
        # find lists a directory before what it holds.
        [ -z "$done_with" ] || [ "${entry#"$done_with"/}" = "$entry" ] || continue
        done_with=
        if [ -e "/mnt/view/$entry" ] || [ -L "/mnt/view/$entry" ]; then
            { [ -d "/mnt/view/$entry" ] && [ ! -L "/mnt/view/$entry" ]; } || done_with=$entry
        elif [ "$1" = cache ] && is_secret "$entry"; then
            done_with=$entry
        elif [ "$1" = cache ] && holds_secret "$entry" && [ -d "$image/$entry" ] && [ ! -L "$image/$entry" ]; then
            make_hidden_dir "$hidden" "$image" "$entry"
        else
            if [ "${entry%/*}" = "$entry" ]; then
                make_hidden_dir "$hidden" "$image" ""
            else
                make_hidden_dir "$hidden" "$image" "${entry%/*}"
            fi
            mknod "$hidden/$entry" c 0 0
            done_with=$entry
        fi
    done < <(find "$image" -mindepth 1 -printf '%P\0')
    umount /mnt/view
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

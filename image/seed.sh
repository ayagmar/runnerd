#!/bin/bash
# Makes slot 1's view of each layer the profile's new read-only snapshot in /base, then drops every slot's own
# changes, which the snapshot now holds. Runs as root in a throwaway container while none of the profile's slots
# runs a job, with /base and each slot's /state mounted at /workers/<N>. With a first argument, does nothing unless
# slot 1's changes to the layers (not its Docker Engine's data) reach that many bytes. With a second, a snapshot
# that would outgrow that many bytes starts the layers of START_OVER_FIRST over from the image, in order, until the
# rest fits, and every layer when they are not enough: a layer that starts over loses its snapshot and every slot's
# changes to it, which drops what no job changed since, such as a superseded tool version. Only the image is whole on
# its own: slot 1's changes hold what its jobs wrote, not what they only read from the snapshot, such as the files a
# cache's updated index names. Each layer is a mount of its own, so one that starts over leaves the others whole but
# for what they run from it: a known Python environment (VENV_ROOTS) that does goes with it, and anything else, or a
# look that fails, starts the whole snapshot over. The rest is made as below the limit (docs/adr/0152).
# Slot 1's view is copied without the image, so what its jobs deleted of the image would come back: whiteouts for it
# go in /base/<layer>.hidden, which containers mount between the snapshot and the image (layers.sh). They are made
# before the snapshot is weighed, so its weight counts them.
# RESEED_MARKER is there from the first change to the snapshot or a slot's layers until the last is on disk; a run
# that finds it, left by one killed in between, starts over from the image whatever its arguments.
# With `measure` as its only argument, changes nothing: prints what slot 1 changed, as a reseed would weigh it, or that
# the last reseed did not finish. runnerd runs it with /base and /workers/1 read-only while the profile's runners run,
# so the profile's upkeep stops them only for a reseed that will replace the snapshot. The count is advisory: files a
# job deletes meanwhile are left out, and so are du's complaints about them.
set -euo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

if [ "${1:-}" = measure ]; then
    if [ -e "$RESEED_MARKER" ]; then
        echo "the last reseed did not finish"
    else
        # A job slot 1 runs meanwhile may delete a file du is counting: the count is advisory, as the reseed
        # weighs slot 1 again.
        changes=$(layer_changes /workers/1 2> /dev/null)
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

# Drops the snapshot of each layer named, and every slot's changes to it.
start_layers_over() {
    local name
    begin_change
    for name in "$@"; do
        rm -rf "/base/$name" "/base/$name.new" "/base/$name.hidden" "/base/$name.hidden.new" /workers/*/"$name"
    done
}

# Drops the snapshot of every layer and every slot's changes.
start_over() {
    local layer names=()
    for layer in "${LAYERS[@]}"; do
        names+=("${layer%%:*}")
    done
    start_layers_over "${names[@]}"
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
    local hidden=/base/$1.hidden.new image=$2 entry done_with='' lower=/workers/1/$1/upper entries
    [ -d "$image" ] && [ ! -L "$image" ] || return 0
    # A profile's first reseed has no snapshot yet.
    [ ! -d "/base/$1" ] || lower=$lower:/base/$1
    [ ! -d "/base/$1.hidden" ] || lower=$lower:/base/$1.hidden
    # Listed in full first, so a find that fails fails the reseed instead of leaving whiteouts out.
    entries=$(mktemp)
    find "$image" -mindepth 1 -printf '%P\0' > "$entries"
    mount -t overlay overlay -o "ro,lowerdir=$lower:$image" /mnt/view
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

# The Python environments of the cache layer that go with a layer that starts over, as their place there: pipx's
# venvs and those of `pipx run`, Poetry's virtualenvs, and pre-commit's Python environments in its clones. Each holds
# its own interpreter link, scripts, packages and metadata, and its tool makes it again when it is gone. Matched one
# directory below each, by `case`.
VENV_ROOTS=('pipx/venvs/*' 'pipx/.cache/*' 'pypoetry/virtualenvs/*' 'pre-commit/repo*/py_env-*')

# Prints the Python environment of VENV_ROOTS that the cache layer's entry $1 is in, if it is in one: its first
# directories, as many as the pattern has parts, when they match it and the entry is inside them.
venv_of() {
    local pattern venv rest slashes
    for pattern in "${VENV_ROOTS[@]}"; do
        slashes=${pattern//[^\/]/} venv='' rest=$1
        while [ "${#venv}" = 0 ] || [ "${venv//[^\/]/}" != "$slashes" ]; do
            [ "${rest#*/}" != "$rest" ] || continue 2
            venv+=${venv:+/}${rest%%/*}
            rest=${rest#*/}
        done
        # shellcheck disable=SC2254 # the pattern is the match
        case $venv in
            $pattern)
                echo "$venv"
                return 0
                ;;
        esac
    done
    return 1
}

# Whether the path $1 is one of the paths that follow, or under one.
under_any() {
    local path=$1 top
    shift
    for top in "$@"; do
        [ "$path" != "$top" ] && [ "${path#"$top"/}" = "$path" ] || return 0
    done
    return 1
}

# Looks in the snapshot a kept layer $1 would get, at $2, which containers mount at $3, for what runs from the paths
# of the layers that start over, which follow: a symlink that resolves into one, and an executable or pyvenv.cfg whose
# first 4 KiB name one anywhere. Writes in $venvs/$1 each Python environment of VENV_ROOTS that holds one,
# NUL-separated, to go with those layers. Prints why, and returns 1, for any other, and when it cannot look at all of
# the snapshot: the whole snapshot then starts over (docs/adr/0152). What only names such a path in a file it does
# not look at, as pip's self-check or Gradle's file hashes do, stays: a cache checks those against the file system.
runs_from_dropped() {
    local name=$1 root=$2 at=$3 kind entry target venv listed hits
    shift 3
    listed=$(mktemp) hits=$(mktemp)
    : > "$venvs/$name"
    if ! LC_ALL=C find "$root" -mindepth 1 \( -type l -o -type f \( -name pyvenv.cfg -o -perm /111 \) \) \
        -printf '%y\0%P\0%l\0' > "$listed"; then
        echo "the $name layer could not be listed"
        return 1
    fi
    # Files named by the triples' second field, read at most 4 KiB each, never through a symlink: those that name a
    # path, NUL-separated. Any error fails the look.
    if ! python3 -I -c '
import os, sys
fields = open(sys.argv[1], "rb").read().split(b"\0")
paths = [path.encode() for path in sys.argv[3:]]
for kind, entry in zip(fields[0::3], fields[1::3]):
    if kind != b"f":
        continue
    fd = os.open(os.path.join(sys.argv[2].encode(), entry), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        head = os.read(fd, 4096)
    finally:
        os.close(fd)
    if any(path in head for path in paths):
        sys.stdout.buffer.write(entry + b"\0")
' "$listed" "$root" "$@" > "$hits"; then
        echo "the $name layer's files could not be read"
        return 1
    fi
    # The symlinks that resolve into a path join the files.
    while IFS= read -r -d '' kind && IFS= read -r -d '' entry && IFS= read -r -d '' target; do
        [ "$kind" = l ] || continue
        [ "${target#/}" != "$target" ] || target=$(realpath -ms -- "$at/$(dirname -- "$entry")/$target")
        ! under_any "$target" "$@" || printf '%s\0' "$entry" >> "$hits"
    done < "$listed"
    while IFS= read -r -d '' entry; do
        venv=''
        [ "$name" != cache ] || venv=$(venv_of "$entry") || true
        if [ -z "$venv" ] || [ ! -d "$root/$venv" ] || [ -L "$root/$venv" ] || [ ! -f "$root/$venv/pyvenv.cfg" ] \
            || [ -L "$root/$venv/pyvenv.cfg" ]; then
            printf 'the %s layer runs from %s: %q\n' "$name" "${dropped[*]}" "$entry"
            return 1
        fi
        printf '%s\0' "$venv" >> "$venvs/$name"
    done < "$hits"
    rm -f "$listed" "$hits"
}

# Drops from the copy of a layer at $1 the Python environments listed in the file $2, NUL-separated.
drop_venvs() {
    local dir
    while IFS= read -r -d '' dir; do
        rm -rf "${1:?}/$dir"
    done < "$2"
}

# Stages the whiteouts of each layer slot 1 changed in /base/<layer>.hidden.new, so a snapshot's weight counts them,
# and its publishing below moves them in.
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*} path=${layer#*:}
    [ -d "/workers/1/$name/upper" ] || continue
    hide_image_entries "$name" "$path"
done

# The Python environments of each kept layer that go with a layer that starts over, in $venvs/<layer>.
venvs=$(mktemp -d)
# What the reseed says it started over, once the rest is seeded and weighed.
partial=''
max=${2:-0}
if [ "$max" -gt 0 ]; then
    # What the new snapshot would hold of each layer: slot 1's view of it if slot 1 changed it, with the whiteouts
    # staged above, else its snapshot with the whiteouts it has.
    declare -A weighs
    size=0
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*} weight=0
        if [ -d "/workers/1/$name/upper" ]; then
            [ ! -d "/base/$name.hidden.new" ] || weight=$(du -sxb "/base/$name.hidden.new" | cut -f1)
            mount_view "$name"
            weight=$((weight + $(du -sxb /mnt/merged | cut -f1)))
            umount /mnt/merged
        else
            [ ! -d "/base/$name.hidden" ] || weight=$(du -sxb "/base/$name.hidden" | cut -f1)
            [ ! -d "/base/$name" ] || weight=$((weight + $(du -sxb "/base/$name" | cut -f1)))
        fi
        weighs[$name]=$weight
        size=$((size + weight))
    done
    if [ "$size" -gt "$max" ]; then
        over="the snapshot would hold $size bytes, over its limit of $max"
        left=$size dropped=() dropped_paths=()
        for name in "${START_OVER_FIRST[@]}"; do
            [ "$left" -gt "$max" ] || break
            [ "${weighs[$name]:-0}" -gt 0 ] || continue
            left=$((left - weighs[$name]))
            dropped+=("$name")
            for layer in "${LAYERS[@]}"; do
                [ "${layer%%:*}" != "$name" ] || dropped_paths+=("${layer#*:}")
            done
        done
        if [ "$left" -gt "$max" ]; then
            echo "$over: started over from the image"
            start_over
            exit 0
        fi
        # What a kept layer runs from a dropped one would be left broken, not cold: only the Python environments of
        # VENV_ROOTS go with it, and anything else starts the whole snapshot over (docs/adr/0152).
        for layer in "${LAYERS[@]}"; do
            name=${layer%%:*} path=${layer#*:} root=/base/$name
            [[ " ${dropped[*]} " != *" $name "* ]] || continue
            if [ -d "/workers/1/$name/upper" ]; then
                mount_view "$name"
                root=/mnt/merged
            fi
            [ -d "$root" ] || continue
            why=$(runs_from_dropped "$name" "$root" "$path" "${dropped_paths[@]}") && found=0 || found=$?
            [ "$root" != /mnt/merged ] || umount /mnt/merged
            if [ "$found" != 0 ]; then
                echo "$over: started over from the image, as ${why:-the $name layer could not be looked at}"
                start_over
                exit 0
            fi
        done
        count=0
        for list in "$venvs"/*; do
            [ ! -s "$list" ] || count=$((count + $(sort -zu "$list" | tr -cd '\0' | wc -c)))
        done
        with=''
        [ "$count" -ne 1 ] || with=", with 1 Python environment that runs from it"
        [ "$count" -le 1 ] || with=", with $count Python environments that run from it"
        partial="$over: started ${dropped[*]} over from the image$with"
        start_layers_over "${dropped[@]}"
    fi
fi
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*} path=${layer#*:}
    if [ ! -d "/workers/1/$name/upper" ]; then
        # A snapshot slot 1 did not change loses only the Python environments that ran from a layer started over.
        if [ -s "$venvs/$name" ]; then
            begin_change
            drop_venvs "/base/$name" "$venvs/$name"
        fi
        continue
    fi
    mount_view "$name"
    cp -a /mnt/merged "/base/$name.new"
    umount /mnt/merged
    [ ! -s "$venvs/$name" ] || drop_venvs "/base/$name.new" "$venvs/$name"
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
if [ -n "$partial" ]; then
    # The snapshot as kept, weighed once: a file linked from two places counts once.
    kept=()
    for layer in "${LAYERS[@]}"; do
        name=${layer%%:*}
        for dir in "/base/$name" "/base/$name.hidden"; do
            [ ! -d "$dir" ] || kept+=("$dir")
        done
    done
    weight=0
    [ "${#kept[@]}" = 0 ] || weight=$(du -csxb "${kept[@]}" | tail -n 1 | cut -f1)
    echo "$partial, keeping $weight bytes"
fi
end_change
rm -rf "$venvs"

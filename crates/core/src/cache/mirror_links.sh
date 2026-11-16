#!/bin/sh
# The registry mirror's links whose blob is gone or empty (docs/adr/0144). Runs in the registry image's busybox sh,
# with its storage at /var/lib/registry, or on the directory `$2`.
#
# `repair` removes each such link, and an empty blob's data, which the registry would keep when it fetches the blob
# again; `check` lists them and says what the running mirror's blob descriptor cache is. Prints
# `<fault> <digest> <link>` for each: `gone` when the blob's data is missing, `empty` when it has none, `garbled`
# with `-` for a link that holds no digest. Prints `links <n>` last, once every link was read: output without that
# line was cut off. A volume without the layout of registry 2 and 3 has no links to read.
set -u
mode=$1
root=${2:-/var/lib/registry}/docker/registry/v2
# The digest of no bytes: its data is rightly empty.
nothing=sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
if [ "$mode" = check ]; then
    # Unset in a mirror an older runnerd started: the image's config keeps the cache in memory.
    printf 'cache %s\n' "${REGISTRY_STORAGE_CACHE_BLOBDESCRIPTOR-image}"
fi
if [ ! -d "$root/repositories" ]; then
    echo "links 0"
    exit 0
fi
cd "$root" || exit 1
find repositories \( -path '*/_layers/*' -o -path '*/_manifests/*' \) -type f -name link | {
    count=0
    failed=0
    while IFS= read -r link; do
        count=$((count + 1))
        target=
        # A link holds its digest without a newline, which `read` reports as the end of its input.
        IFS= read -r target <"$link" || :
        case $target in
        *[!a-z0-9:]* | *:*:* | :* | *:) data= ;;
        *:??*)
            hex=${target#*:}
            data=blobs/${target%%:*}/${hex%"${hex#??}"}/$hex/data
            ;;
        *) data= ;;
        esac
        if [ -z "$data" ]; then
            fault=garbled
            target=-
        elif [ ! -f "$data" ]; then
            fault=gone
        elif [ -s "$data" ] || [ "$target" = "$nothing" ]; then
            continue
        else
            fault=empty
        fi
        echo "$fault $target $link"
        if [ "$mode" = repair ]; then
            rm -f "$link" || failed=1
            if [ "$fault" = empty ]; then
                rm -f "$data" || failed=1
            fi
        fi
    done
    echo "links $count"
    exit "$failed"
}

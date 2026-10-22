#!/bin/bash
# Makes slot 1's view of each layer the profile's new read-only snapshot in /base, then drops every slot's own
# changes, which the snapshot now holds. Runs as root in a throwaway container while none of the profile's slots
# runs a job, with /base and each slot's /state mounted at /workers/<N>. With an argument, does nothing unless
# slot 1's changes reach that many GB.
set -euo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

if [ "$(size_gb /workers/1)" -lt "${1:-0}" ]; then
    exit 0
fi
mkdir -p /mnt/merged
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*}
    upper=/workers/1/$name/upper
    [ -d "$upper" ] || continue
    mkdir -p "/base/$name"
    rm -rf "/base/$name.new"
    mount -t overlay overlay -o "ro,lowerdir=$upper:/base/$name" /mnt/merged
    cp -a /mnt/merged "/base/$name.new"
    umount /mnt/merged
    # Gradle daemon logs and registry: only meaningful to the daemons of the slot that wrote them.
    rm -rf "/base/$name.new/gradle/daemon"
    rm -rf "/base/$name"
    mv "/base/$name.new" "/base/$name"
    rm -rf /workers/*/"$name"
    echo "seeded $name: $(du -sh "/base/$name" | cut -f1)"
done

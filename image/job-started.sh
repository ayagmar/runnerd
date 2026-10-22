#!/bin/bash
# The runner's job-started hook: notes the bytes received so far, for the line job-cleanup.sh logs. A failure here
# does not fail the job.
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh
rx_bytes > "$HOME/.runnerd-job-rx" || true

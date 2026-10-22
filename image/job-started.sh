#!/bin/bash
# The runner's job-started hook: notes the bytes received so far, for the line job-cleanup.sh logs.
cat /sys/class/net/eth0/statistics/rx_bytes > "$HOME/.runnerd-job-rx"

#!/bin/bash
# The runner's job-completed hook: job-cleanup.sh as root (its sudo rule is in the Dockerfile). A failed cleanup
# does not fail the job.
sudo -n /usr/local/sbin/runnerd-job-cleanup || true

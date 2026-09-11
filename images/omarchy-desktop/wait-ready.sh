#!/bin/bash
set -eu
for ((i=0;i<300;i++)); do
    [[ ! -e $1 ]] || exit 0
    sleep 0.1
done
echo "Desktop readiness timed out: $1" >&2
exit 1

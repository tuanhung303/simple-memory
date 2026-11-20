#!/usr/bin/env bash
# Wait until no process holds a file under DIR (as its cwd or an open file), at most SECONDS.
# smoke.sh calls it so that backup and the cleanup never race the hook's detached catch-up.
#
#   scripts/wait_free.sh DIR SECONDS
#
# Exit 0 when DIR is free, 1 when a process still holds a file under DIR after SECONDS (the
# message names the PIDs), 2 when lsof is missing or cannot see a process it must see. Call it
# with a cwd outside DIR. lsof is in /usr/sbin on macOS, and it can exit 1 with a full list, so
# its output decides, never its exit status.
set -uo pipefail

dir=${1:?usage: scripts/wait_free.sh DIR SECONDS}
secs=${2:?usage: scripts/wait_free.sh DIR SECONDS}
lsof=$(PATH=/usr/sbin:/usr/bin:/sbin:/bin command -v lsof) || { echo "no lsof" >&2; exit 2; }
holders() { "$lsof" -t -w +D "$dir" 2>/dev/null || true; }

# a blind lsof lists nothing and would pass the wait, so it must list this shell while its cwd
# is under DIR
cd "$dir" || exit 2
holders | grep -x "$$" > /dev/null || { echo "lsof does not list this shell, whose cwd is under $dir" >&2; exit 2; }
cd /

deadline=$((SECONDS + secs))
while held=$(holders) && [[ -n $held ]]; do
    if ((SECONDS >= deadline)); then
        echo "processes still hold files under $dir after $secs s: $(tr '\n' ' ' <<< "$held")" >&2
        exit 1
    fi
    sleep 0.5
done

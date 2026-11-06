#!/usr/bin/env bash
# Summarise the YoruPlay daemon, TUI and optional extra processes by PSS. Pass PIDs or let
# the script find matching processes.
set -euo pipefail

if [[ $# -gt 0 ]]; then
    pids=("$@")
else
    mapfile -t pids < <(pgrep -f '(^|/)yoruplay (daemon|tui)|yoruplay.*daemon' 2>/dev/null || true)
fi

printf '%-8s %-10s %-8s %-8s %-8s %s\n' PID PSS_KB RSS_KB THREADS CPU CMD
for pid in "${pids[@]}"; do
    [[ -r "/proc/$pid/smaps_rollup" ]] || continue
    pss=$(awk '/^Pss:/{print $2}' "/proc/$pid/smaps_rollup")
    rss=$(awk '/^Rss:/{print $2}' "/proc/$pid/smaps_rollup")
    threads=$(awk '/^Threads:/{print $2}' "/proc/$pid/status")
    cpu=$(ps -o %cpu= -p "$pid" | tr -d ' ')
    cmd=$(ps -o args= -p "$pid")
    printf '%-8s %-10s %-8s %-8s %-8s %s\n' "$pid" "$pss" "$rss" "$threads" "$cpu" "$cmd"
done

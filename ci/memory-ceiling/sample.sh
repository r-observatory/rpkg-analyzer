#!/usr/bin/env bash
# sample.sh <dir> [seconds]: until <dir>/stop exists, once per interval append
#   <dir>/mem.tsv    epoch, MemAvailable kB
#   <dir>/procs.tsv  epoch, pid, parent pid, name, VmHWM kB, VmRSS kB
# for every R and analyzer process. Linux only.
set -u
dir=$1
interval=${2:-1}
: > "$dir/mem.tsv"
: > "$dir/procs.tsv"
while [ ! -e "$dir/stop" ]; do
  now=$(date +%s)
  awk -v now="$now" '/^MemAvailable:/ { print now "\t" $2 }' /proc/meminfo >> "$dir/mem.tsv"
  # cat skips a process that exited during the scan; each status begins with Name.
  cat /proc/[0-9]*/status 2> /dev/null | awk -v now="$now" '
    function flush() {
      if ((name == "R" || name ~ /analyzer/) && hwm != "")
        print now "\t" pid "\t" ppid "\t" name "\t" hwm "\t" rss
    }
    /^Name:/ { flush(); name = $2; pid = ""; ppid = ""; hwm = ""; rss = "" }
    /^Pid:/ { pid = $2 }
    /^PPid:/ { ppid = $2 }
    /^VmHWM:/ { hwm = $2 }
    /^VmRSS:/ { rss = $2 }
    END { flush() }' >> "$dir/procs.tsv"
  sleep "$interval"
done
touch "$dir/stopped"

#!/usr/bin/env bash
# Inside the measuring container: run /mc/bin/analyzer on /mc/tree RUNS times with the
# cache unset and print one tab-separated row per run. LIMIT_MIB, when set, is an
# address-space limit. Columns: id build limit_mib run exit vmhwm_kb vmpeak_kb
# maxrss_kb user_s sys_s wall_s oom_kill cgroup_peak_kb stderr_bytes stdout_bytes sha256
set -uo pipefail
unset RPKG_ANALYZER_CACHE_DIR RPKG_ANALYZER_CACHE_VERIFY RPKG_ANALYZER_STATS
cg=/sys/fs/cgroup
oom_kills() {
  if [ -r "$cg/memory.events" ]; then
    awk '/^oom_kill /{print $2; found = 1} END {if (!found) print -1}' "$cg/memory.events"
  else
    echo -1
  fi
}
out=/tmp/out.ndjson
limit=${LIMIT_MIB:-none}
for run in $(seq 1 "${RUNS:-1}"); do
  rm -f "$out" "$out.err"
  before=$(oom_kills)
  if [ "$limit" = none ]; then
    line=$(/mc/bin/peak "$out" /mc/bin/analyzer /mc/tree --input-kind "$KIND")
  else
    line=$(prlimit --as=$((limit * 1024 * 1024)) /mc/bin/peak "$out" /mc/bin/analyzer /mc/tree --input-kind "$KIND" 2>/dev/null)
  fi
  after=$(oom_kills)
  # No line means the wrapper itself could not start under the limit.
  read -r code hwm vmpeak maxrss user sys wall <<< "${line:--1 -1 -1 -1 0 0 0}"
  kills=-1
  if [ "$before" -ge 0 ] && [ "$after" -ge 0 ]; then kills=$((after - before)); fi
  peak_kb=-1
  if [ -r "$cg/memory.peak" ]; then peak_kb=$(($(cat "$cg/memory.peak") / 1024)); fi
  stderr_bytes=0
  stdout_bytes=0
  sha=none
  if [ -f "$out.err" ]; then stderr_bytes=$(stat -c %s "$out.err"); fi
  if [ -f "$out" ]; then
    stdout_bytes=$(stat -c %s "$out")
    sha=$(sha256sum "$out" | cut -d' ' -f1)
  fi
  # The start of a non-empty stderr is kept beside the results, not printed.
  if [ "$stderr_bytes" -gt 0 ] && [ -d /mc/out ]; then
    head -c 400 "$out.err" > "/mc/out/$ID.$BUILD.$limit.$run.stderr"
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$ID" "$BUILD" "$limit" "$run" "$code" "$hwm" "$vmpeak" "$maxrss" "$user" "$sys" "$wall" \
    "$kills" "$peak_kb" "$stderr_bytes" "$stdout_bytes" "$sha"
done

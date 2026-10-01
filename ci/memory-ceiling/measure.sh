#!/usr/bin/env bash
# measure.sh: peak memory of the analyzer on each input of inputs.tsv, on Linux.
# Per input: fetch the tree, run CAND RUNS times in a container limited to MEMORY, run
# BASE once the same way, run CAND once under each address-space limit of LIMITS, then
# delete the tree. Rows go to $OUT/runs.tsv (in-container.sh lists the columns, plus
# container_exit and container_oom), the platform to $OUT/platform.txt.
#
#   CAND    the build under test, a linux binary                  required
#   PEAK    peak.c compiled for the container, statically linked  required
#   OUT     results directory                                     required
#   BASE    the build CAND is compared with                       optional
#   SHARD   i/n: every nth input, starting at the ith             optional
#   ONLY    ids to run (package@ref), space separated             optional
#   ENGINE  docker or podman                                      default docker
#   IMAGE   measuring image, glibc                                default ubuntu 24.04
#   RUNS 3, MEMORY 2g, LIMITS "3072 2560", LIMIT_MEMORY 4g, JOBS 1, WORK $OUT/work
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${CAND:?}" "${PEAK:?}" "${OUT:?}"
export CAND PEAK OUT
export BASE=${BASE:-}
export ENGINE=${ENGINE:-docker}
export IMAGE=${IMAGE:-docker.io/library/ubuntu:24.04}
export INPUTS=${INPUTS:-$here/inputs.tsv}
export RUNS=${RUNS:-3}
export MEMORY=${MEMORY:-2g}
export LIMITS=${LIMITS-3072 2560}
export LIMIT_MEMORY=${LIMIT_MEMORY:-4g}
export WORK=${WORK:-$OUT/work}
JOBS=${JOBS:-1}
filler=$'-1\t-1\t-1\t-1\t0\t0\t0\t-1\t-1\t0\t0\tnone'

# One container: BUILD on the tree, RUNS times, under MEMORY and an optional limit.
container() { # build limit runs memory binary
  local build=$1 limit=$2 runs=$3 memory=$4 bin=$5
  local name="mc-$key-$build-$limit" rc oom got
  local args=(run --name "$name" --network none --memory "$memory" --memory-swap "$memory"
    --cap-add SYS_PTRACE -e "ID=$id" -e "BUILD=$build" -e "KIND=$kind" -e "RUNS=$runs"
    -v "$tree:/mc/tree:ro" -v "$bin:/mc/bin/analyzer:ro" -v "$PEAK:/mc/bin/peak:ro"
    -v "$here/in-container.sh:/mc/run.sh:ro" -v "$OUT/stderr:/mc/out")
  if [ "$limit" != none ]; then args+=(-e "LIMIT_MIB=$limit"); fi
  "$ENGINE" rm -f "$name" > /dev/null 2>&1
  "$ENGINE" "${args[@]}" "$IMAGE" bash /mc/run.sh > "$rows.c" 2> "$rows.engine"
  rc=$?
  oom=$("$ENGINE" inspect -f '{{.State.OOMKilled}}' "$name" 2> /dev/null) || oom=unknown
  "$ENGINE" rm -f "$name" > /dev/null 2>&1
  [ -s "$rows.engine" ] || rm -f "$rows.engine"
  # A container that died early leaves fewer rows than runs: the rest are failures.
  got=$(grep -c . "$rows.c")
  while [ "$got" -lt "$runs" ]; do
    got=$((got + 1))
    printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$build" "$limit" "$got" "$filler" >> "$rows.c"
  done
  awk -F'\t' -v rc="$rc" -v oom="$oom" 'BEGIN { OFS = "\t" } { print $0, rc, oom }' "$rows.c" >> "$rows.tmp"
  awk -F'\t' '{ printf "%s %s limit=%s run=%s exit=%s VmHWM=%.1f MiB VmPeak=%.1f MiB oom_kill=%s\n",
    $1, $2, $3, $4, $5, $6 / 1024, $7 / 1024, $12 }' "$rows.c"
  rm -f "$rows.c"
}

one_input() { # id
  local id=$1 owner pkg commit kind key tree rows limit
  IFS=$'\t' read -r _ owner pkg _ commit kind \
    <<< "$(awk -F'\t' -v id="$id" 'NR > 1 && $3 "@" $4 == id' "$INPUTS")"
  if [ -z "${commit:-}" ]; then
    echo "$id is not in the input list" >&2
    return 1
  fi
  key=$(printf '%s' "$id" | cksum | cut -d' ' -f1)
  tree="$WORK/tree-$key"
  rows="$OUT/rows/$id.tsv"
  : > "$rows.tmp"
  if "$here/fetch-tree.sh" "$owner" "$pkg" "$commit" "$tree" 2> "$rows.fetch"; then
    rm -f "$rows.fetch"
    printf '%s\t%s\n' "$id" "$(du -sk "$tree" | cut -f1)" > "$OUT/rows/$id.tree"
    container cand none "$RUNS" "$MEMORY" "$CAND"
    if [ -n "$BASE" ]; then container base none 1 "$MEMORY" "$BASE"; fi
    for limit in $LIMITS; do container cand "$limit" 1 "$LIMIT_MEMORY" "$CAND"; done
  else
    echo "$id tree not fetched"
    printf '%s\tfetch\tnone\t1\t%s\t-1\tunknown\n' "$id" "$filler" >> "$rows.tmp"
  fi
  rm -rf "$tree"
  mv "$rows.tmp" "$rows"
}

if [ "${1:-}" = --one ]; then
  one_input "$2"
  exit
fi

mkdir -p "$OUT/rows" "$OUT/stderr" "$WORK"
chmod 777 "$OUT/stderr"
"$ENGINE" image inspect "$IMAGE" > /dev/null 2>&1 || "$ENGINE" pull -q "$IMAGE" > /dev/null

# The platform, the builds, and a check that the exit stop works in this container.
base_mount=()
if [ -n "$BASE" ]; then base_mount=(-v "$BASE:/mc/bin/base:ro"); fi
{
  echo "date $(date -u +%FT%TZ)"
  echo "engine $ENGINE $("$ENGINE" --version 2> /dev/null | head -1)"
  echo "image $IMAGE $("$ENGINE" image inspect -f '{{.Id}}' "$IMAGE" 2> /dev/null)"
  echo "runs $RUNS memory $MEMORY limits_mib ${LIMITS:-none} limit_memory $LIMIT_MEMORY"
  # shellcheck disable=SC2016
  "$ENGINE" run --rm --network none --memory "$MEMORY" --memory-swap "$MEMORY" --cap-add SYS_PTRACE \
    -v "$CAND:/mc/bin/analyzer:ro" -v "$PEAK:/mc/bin/peak:ro" ${base_mount[@]+"${base_mount[@]}"} \
    "$IMAGE" bash -c '
      echo "arch $(uname -m)"
      echo "kernel $(uname -r)"
      echo "glibc $(ldd --version | head -1 | awk "{print \$NF}")"
      echo "cpus $(nproc)"
      echo "memtotal_kb $(awk "/^MemTotal/{print \$2}" /proc/meminfo)"
      echo "cgroup_memory_max $(cat /sys/fs/cgroup/memory.max 2> /dev/null)"
      echo "candidate $(/mc/bin/analyzer --version) sha256 $(sha256sum /mc/bin/analyzer | cut -c1-16)"
      if [ -x /mc/bin/base ]; then
        echo "baseline $(/mc/bin/base --version) sha256 $(sha256sum /mc/bin/base | cut -c1-16)"
      fi
      echo "exit_stop $(/mc/bin/peak /tmp/selftest /bin/true)"'
} > "$OUT/platform.txt"
cat "$OUT/platform.txt"
read -r _ code hwm vmpeak _ <<< "$(grep '^exit_stop ' "$OUT/platform.txt")"
if [ "${code:-1}" != 0 ] || [ "${hwm:--1}" -le 0 ] || [ "${vmpeak:--1}" -le 0 ]; then
  echo "the exit stop gave no peaks in this container, nothing measured" >&2
  exit 2
fi

ids=$(awk -F'\t' -v shard="${SHARD:-}" -v only=" ${ONLY:-} " '
  NR == 1 { next }
  { n++; id = $3 "@" $4 }
  only != "  " && index(only, " " id " ") == 0 { next }
  shard != "" { split(shard, s, "/"); if ((n - 1) % s[2] != s[1] - 1) next }
  { print id }' "$INPUTS")
if [ -z "$ids" ]; then
  echo "no input selected" >&2
  exit 2
fi
if [ "$JOBS" -gt 1 ]; then
  printf '%s\n' "$ids" | xargs -P "$JOBS" -n 1 bash "$0" --one
else
  for id in $ids; do one_input "$id"; done
fi

# Rows and tree sizes in list order.
header=$'id\tbuild\tlimit_mib\trun\texit\tvmhwm_kb\tvmpeak_kb\tmaxrss_kb\tuser_s\tsys_s\twall_s'
header+=$'\toom_kill\tcgroup_peak_kb\tstderr_bytes\tstdout_bytes\tsha256\tcontainer_exit\tcontainer_oom'
printf '%s\n' "$header" > "$OUT/runs.tsv"
printf 'id\ttree_kb\n' > "$OUT/trees.tsv"
for id in $ids; do
  if [ -f "$OUT/rows/$id.tsv" ]; then cat "$OUT/rows/$id.tsv" >> "$OUT/runs.tsv"; fi
  if [ -f "$OUT/rows/$id.tree" ]; then cat "$OUT/rows/$id.tree" >> "$OUT/trees.tsv"; fi
done
rmdir "$WORK" 2> /dev/null || true
echo "measured $(printf '%s\n' "$ids" | grep -c .) inputs, $(($(grep -c . "$OUT/runs.tsv") - 1)) runs"

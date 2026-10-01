#!/usr/bin/env bash
# worker.sh <pipeline dir> <out dir> <runs.tsv>...
# The real worker on two lists, CORES packages at a time, with nothing limited:
#   largest  the four Bioconductor packages with the largest peaks in the runs
#   block    six wide-output packages the pipeline meets next to each other, in its order
# RPKG_ANALYZER_BIN names the build. CORES defaults to 4. LARGEST and BLOCK replace a list.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
pipeline=$1 out=$2
shift 2
CORES=${CORES:-4}
mkdir -p "$out/largest" "$out/block"
rm -f "$out/largest/skipped.txt" "$out/largest/given.txt" "$out/block/given.txt"
# A list given by hand is marked, so the verdict says it is not the standing one.
if [ -n "${LARGEST+x}" ]; then echo "$LARGEST" > "$out/largest/given.txt"; fi
if [ -n "${BLOCK+x}" ]; then echo "$BLOCK" > "$out/block/given.txt"; fi
BLOCK=${BLOCK-RTCGA.methylation RTCGA.miRNASeq RTCGA.mRNA RTCGA.mutations RTCGA.PANCAN12 RTCGA.rnaseq}
if [ -z "${LARGEST+x}" ]; then
  # A package over the ceiling alone would take four workers and the machine with it.
  if ! LARGEST=$(python3 "$here/judge.py" largest --runs "$@" 2> "$out/largest/skipped.txt"); then
    LARGEST=
    echo "largest: not run, $(cat "$out/largest/skipped.txt")"
  else
    rm -f "$out/largest/skipped.txt"
  fi
fi
status=0
run_list() { # name package...
  local name=$1
  shift
  echo "$name: $* ($CORES at a time)"
  Rscript "$here/worker.R" "$pipeline" "$out/$name" "$CORES" "$@" || status=1
  rm -rf "$out/$name/work" "$out/$name/exit" "$out/$name/stop" "$out/$name/stopped"
}
# shellcheck disable=SC2086
if [ -n "$LARGEST" ]; then run_list largest $LARGEST; fi
# shellcheck disable=SC2086
if [ -n "$BLOCK" ]; then run_list block $BLOCK; fi
exit "$status"

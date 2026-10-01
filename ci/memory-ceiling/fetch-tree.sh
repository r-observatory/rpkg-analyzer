#!/usr/bin/env bash
# fetch-tree.sh <owner> <package> <commit> <dest>
# One commit of github.com/<owner>/<package> as a plain tree in <dest>, extracted with
# git archive and LFS filters off, so LFS files stay pointers.
set -euo pipefail
owner=$1 pkg=$2 commit=$3 dest=$4
repo=$(mktemp -d "${TMPDIR:-/tmp}/mc-repo.XXXXXX")
trap 'rm -rf "$repo"' EXIT
export GIT_LFS_SKIP_SMUDGE=1 GIT_TERMINAL_PROMPT=0
git init -q --bare "$repo"
tries=0
until git -C "$repo" fetch -q --depth 1 "https://github.com/$owner/$pkg.git" "$commit"; do
  tries=$((tries + 1))
  if [ "$tries" -ge 4 ]; then
    echo "fetch failed: $owner/$pkg $commit" >&2
    exit 1
  fi
  sleep $((tries * 10))
done
rm -rf "$dest"
mkdir -p "$dest"
git -c filter.lfs.smudge= -c filter.lfs.process= -c filter.lfs.required=false \
  -C "$repo" archive "$commit" | tar -x -C "$dest"

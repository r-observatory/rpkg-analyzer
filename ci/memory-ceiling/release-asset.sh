#!/usr/bin/env bash
# release-asset.sh <tag> <dest>: the linux x86_64 build of a published release of this
# repository, checksum verified, written to <dest>. Needs no token on a public repository.
set -euo pipefail
tag=$1 dest=$2
if [[ ! "$tag" =~ ^v[0-9][0-9A-Za-z._-]*$ ]]; then
  echo "not a release tag: $tag" >&2
  exit 2
fi
name=rpkg-analyzer-linux-x86_64.tar.gz
url="https://github.com/${GITHUB_REPOSITORY:-r-observatory/rpkg-analyzer}/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 5 --retry-delay 10 -o "$tmp/$name" "$url/$name"
curl -fsSL --retry 5 --retry-delay 10 -o "$tmp/$name.sha256" "$url/$name.sha256"
(cd "$tmp" && sha256sum -c "$name.sha256")
tar -xzf "$tmp/$name" -C "$tmp"
mkdir -p "$(dirname "$dest")"
install -m 755 "$tmp/rpkg-analyzer" "$dest"

#!/bin/sh
# First-deploy bootstrap: replace with a release's download-worker.sh to pin the version.
set -eu

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' 0
trap 'exit 1' HUP INT TERM

curl -fsSL --retry 3 -o "$tmp/download-worker.sh" \
  "https://github.com/sagikazarmark/atuin-worker/releases/latest/download/download-worker.sh"
sh "$tmp/download-worker.sh"

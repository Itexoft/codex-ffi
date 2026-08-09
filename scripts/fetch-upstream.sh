#!/usr/bin/env bash
# Clones the Codex sources at <ref> into <dest>.
set -euo pipefail

ref="${1:?usage: fetch-upstream.sh <ref> <dest>}"
dest="${2:?usage: fetch-upstream.sh <ref> <dest>}"
repo="${CODEX_UPSTREAM_REPO:-https://github.com/openai/codex.git}"

rm -rf "$dest"
git clone --depth 1 --branch "$ref" "$repo" "$dest"

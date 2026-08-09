#!/usr/bin/env bash
# Applies this repository's overlay onto a Codex checkout, in place.
#
# Two things happen: the overlay tree is copied over the checkout (new files
# only), and the patch is applied — four lines that register the crate as a
# workspace member and publish the raw endpoint client. Afterwards the checkout
# is ready for scripts/build.sh.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tree="${1:?usage: compose.sh <codex-checkout>}"
tree="$(cd "$tree" && pwd)"

[[ -f "$tree/codex-rs/Cargo.toml" ]] || {
    echo "not a Codex checkout: $tree" >&2
    exit 1
}

cp -R "$root/overlay/." "$tree/"

for patch in "$root"/patches/*.patch; do
    git -C "$tree" apply --verbose "$patch"
done

echo "composed $tree"

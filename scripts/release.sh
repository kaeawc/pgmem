#!/usr/bin/env bash
# Bump the workspace version, commit and tag. Push the tag to trigger .github/workflows/release.yml.
#   scripts/release.sh 0.2.0
set -euo pipefail
new="${1:?usage: scripts/release.sh <version>}"
[[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || { echo "not a semver version: $new" >&2; exit 1; }
cd "$(dirname "$0")/.."
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }
old="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
# workspace.package.version and the `version = "=X"` pins on the internal path dependencies.
sed -i.bak -e "s/^version = \"$old\"$/version = \"$new\"/" -e "s/version = \"=$old\" }/version = \"=$new\" }/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace --offline
git commit -am "Release v$new"
git tag -a "v$new" -m "pgmem v$new"
echo "Tagged v$new. Push with: git push origin HEAD && git push origin v$new"

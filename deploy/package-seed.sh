#!/bin/sh
# Refresh the exact standalone dependency lockfile after agent/core dependency changes.
set -eu
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
seed_build=$(mktemp -d)
trap 'rm -rf "$seed_build"' EXIT HUP INT TERM
mkdir -p "$seed_build/crates" "$project_root/seed"
cp -R "$project_root/crates/agent" "$project_root/crates/core" "$seed_build/crates/"
sed 's@members = .*@members = ["crates/core", "crates/agent"]@' "$project_root/Cargo.toml" > "$seed_build/Cargo.toml"
cp "$project_root/Cargo.lock" "$seed_build/Cargo.lock"
cargo generate-lockfile --offline --manifest-path "$seed_build/Cargo.toml"
cp "$seed_build/Cargo.toml" "$seed_build/Cargo.lock" "$project_root/seed/"

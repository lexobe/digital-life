#!/usr/bin/env bash
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo "Linux host root required" >&2; exit 1; }
[[ ${SURVIVE_LINUX_TEST:-} == YES && $# == 1 ]] || {
    echo "Explicit fixture test: SURVIVE_LINUX_TEST=YES $0 CONFIG.toml" >&2
    echo "Requires setup already complete, 8089 free, slots 60000..60003 free, and disk space for four images/templates." >&2
    exit 2
}
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
export SURVIVE_SANDBOX_CONFIG
SURVIVE_SANDBOX_CONFIG=$(realpath -- "$1")
cd -- "$repo"
cargo build --locked -p survive-sandbox --bins --example linux_probe
target_dir=${CARGO_TARGET_DIR:-$repo/target}
export SURVIVE_PROBE_BINARY
SURVIVE_PROBE_BINARY=$(realpath -- "$target_dir/debug/examples/linux_probe")
"${SURVIVE_SANDBOX_BIN:-$target_dir/debug/survive-sandbox}" "$SURVIVE_SANDBOX_CONFIG" doctor
cargo test --locked -p survive-sandbox --test linux -- --ignored --nocapture --test-threads=1

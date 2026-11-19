#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 2 || ( "$1" != setup && "$1" != restore ) ]]; then
    echo "usage: $0 setup|restore CONFIG.toml (explicit root host mutation)" >&2
    exit 2
fi
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo "Linux host root required" >&2; exit 1; }
exec "${SURVIVE_SANDBOX_BIN:-/opt/survive-platform/bin/survive-sandbox}" "$2" "$1"

#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 1 ]]; then echo "usage: $0 CONFIG.toml" >&2; exit 2; fi
exec "${SURVIVE_SANDBOX_BIN:-/opt/survive-platform/bin/survive-sandbox}" "$1" doctor

#!/bin/sh
# Preserve guarded first-install/reboot recovery, while keeping the observer
# console available if execution prerequisites are temporarily unavailable.
set -eu
state_dir=${SURVIVE_DATA_DIR:-/var/lib/survive-platform}
runtime=${SURVIVE_SANDBOX_BIN:-/opt/survive-platform/bin/survive-sandbox}
config=${SURVIVE_SANDBOX_CONFIG:-/etc/survive-platform/sandbox.toml}
if ! test -f "$config"; then
    echo 'Sandbox configuration missing; starting control plane without execution.' >&2
    exit 0
fi
operation=doctor
if ! test -f "$state_dir/installation.json"; then
    operation=setup
elif test -f "$state_dir/boot-id" && test -r /proc/sys/kernel/random/boot_id; then
    current_boot=$(cat /proc/sys/kernel/random/boot_id)
    installed_boot=$(cat "$state_dir/boot-id")
    if test "$current_boot" != "$installed_boot"; then
        operation=restore
    fi
fi
if ! "$runtime" "$config" "$operation"; then
    echo 'Sandbox execution unavailable; starting control plane for diagnostics.' >&2
fi
# Runtime admission still requires the complete Rust doctor check.
exit 0

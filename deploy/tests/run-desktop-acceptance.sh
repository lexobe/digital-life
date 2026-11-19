#!/usr/bin/env bash
# Build prerequisites, resource requirements and example: deploy/tests/README.md.
set -euo pipefail
usage() {
    echo "Usage: $0 DESKTOP_ROOTFS YOUKI BINARY_DIR FIXTURE_PARENT CGROUP_V2_PARENT"
}
if [[ $# == 1 && $1 == --help ]]; then usage; exit 0; fi
if [[ $# != 5 ]]; then usage >&2; exit 2; fi
die() { echo "$*" >&2; exit 1; }
[[ $(uname -s) == Linux && $EUID == 0 ]] || die "Linux host root required"
here=$(cd -- "$(dirname -- "$0")" && pwd)
python_bin=$(command -v -- "${CUA_PYTHON:-python3}") || die "Host Python 3 required"
"$python_bin" -c 'from PIL import features; assert features.check("jpg"), "Pillow needs JPEG support"'
for cmd in unshare findmnt mount ip nft iptables fallocate mkfs.ext4 sysctl realpath timeout; do
    command -v -- "$cmd" >/dev/null || die "Missing host command: $cmd"
done
rootfs=$(realpath -e -- "$1")
youki=$(realpath -e -- "$2")
target=$(realpath -e -- "$3")
parent=$(realpath -e -- "$4")
cg_parent=$(realpath -e -- "$5")
[[ -d $rootfs && -x $youki && -f $youki ]] || die "Expected a rootfs directory and executable youki"
for binary in survive-sandbox survive-server survive-init survive-agent; do
    [[ -f $target/$binary && -x $target/$binary ]] || die "Missing executable: $target/$binary"
done
[[ -d $parent && -w $parent ]] || die "Fixture parent must be a writable directory"
[[ $rootfs != / && $parent != "$rootfs" && $parent != "$rootfs/"* ]] || die "Keep fixture data outside the template"
case $(findmnt -n -o FSTYPE -T "$parent") in ext4|xfs) ;; *) die "Fixture parent must use ext4 or XFS";; esac
[[ $(stat -f -c %T "$cg_parent") == cgroup2fs && -w $cg_parent ]] || die "Writable cgroup v2 parent required"
for controller in cpu memory pids; do
    grep -qw -- "$controller" "$cg_parent/cgroup.subtree_control" ||
        die "Enable $controller for children of the chosen cgroup v2 parent before running"
done
# Verify the paths to the host executables before launching privileged code.
"$python_bin" - "$target/survive-sandbox" "$target/survive-server" "$parent" <<'PY'
import os, pathlib, sys
for raw in sys.argv[1:]:
    path = pathlib.Path(raw)
    for part in [path, *path.parents]:
        meta = part.lstat()
        if part.is_symlink() or meta.st_uid or meta.st_mode & 0o022:
            sys.exit(f"Expected a root-owned path without symlinks or group/world writes: {part}")
PY
umask 077
fixture=$(mktemp -d -- "$parent/desktop-check-XXXXXXXX")
group="$cg_parent/${fixture##*/}"
mkdir -- "$group"
cleanup() {
    local rc=$?
    if [[ -e $group/cgroup.kill ]]; then echo 1 > "$group/cgroup.kill" || rc=1; fi
    find "$group" -depth -type d -exec rmdir -- {} \; || rc=1
    if [[ -e $group ]]; then echo "Could not remove private cgroup $group" >&2; rc=1; fi
    exit "$rc"
}
trap cleanup EXIT
echo "Fixture artifacts: $fixture"
export fixture target group rootfs youki here
export CUA_PYTHON="$python_bin"
# Network changes occur after unshare. The cgroup bind limits all sandbox
# management to the new child. A private /run holds namespace pins and sockets.
timeout --kill-after=15s 12m unshare --mount --net --pid --fork --kill-child --mount-proc bash -se <<'INNER' 2>&1 | tee "$fixture/result.log"
set -euo pipefail
mount --make-rprivate /
mkdir "$fixture/cgroup" "$fixture/data"
mount --bind "$group" "$fixture/cgroup"
mount -t sysfs sysfs /sys
mount --bind "$fixture/cgroup" /sys/fs/cgroup
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run
mkdir -m 0755 /run/netns
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run/netns
ip link set lo up
sysctl -q -w net.ipv4.ip_forward=1
"$CUA_PYTHON" - "$fixture" "$rootfs" "$youki" "$target" > "$fixture/sandbox.toml" <<'PY'
import json, sys
fixture, rootfs, youki, target = sys.argv[1:]
for key, value in dict(data_dir=fixture + "/data", rootfs=rootfs, youki=youki,
                       init_binary=target + "/survive-init", agent_binary=target + "/survive-agent",
                       gateway_url="http://10.203.0.1:8089", total_memory_bytes=805306368,
                       disk_bytes=268435456, command_timeout_secs=120).items():
    print(f"{key} = {json.dumps(value, ensure_ascii=False)}")
PY
"$target/survive-sandbox" "$fixture/sandbox.toml" setup
"$target/survive-sandbox" "$fixture/sandbox.toml" doctor
bash "$here/desktop-acceptance.sh"
INNER

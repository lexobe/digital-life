#!/usr/bin/env bash
# Runs inside the namespaces prepared by run-desktop-acceptance.sh.
set -euo pipefail
trap 'echo "Desktop hook failed at line $LINENO" >&2' ERR
: "${fixture:?private fixture required}" "${target:?build directory required}"
here=$(cd -- "$(dirname -- "$0")" && pwd)
source_tree=${CUA_SOURCE_DIR:-$(cd -- "$here/../.." && pwd)}
python_bin=${CUA_PYTHON:-python3}
[[ $(uname -s) == Linux && $EUID == 0 && $fixture == /* && $target == /* ]]
[[ -f $fixture/sandbox.toml && ! -e $fixture/data/platform.redb ]]
toml_string() { "$python_bin" -c 'import json, sys; print(json.dumps(sys.argv[1], ensure_ascii=False))' "$1"; }
data_value=$(toml_string "$fixture/data")
source_value=$(toml_string "$source_tree")
grep -Fqx -- "data_dir = $data_value" "$fixture/sandbox.toml"
mountpoint -q /run/netns
[[ $(stat -f -c %T /run/netns) == tmpfs ]]
[[ $(stat -c "%d:%i" "$fixture/cgroup") == "$(stat -c "%d:%i" /sys/fs/cgroup)" ]]
"$python_bin" -c 'from PIL import Image; assert Image.core.jpeglib_version'
# Keep staging directory entries as well as mounts private to this validation.
[[ -d /run/survive-platform ]] || mkdir -m 0711 /run/survive-platform
mount -t tmpfs -o mode=0711,nosuid,nodev tmpfs /run/survive-platform
umask 077
touch "$fixture/host-only-marker" "$fixture/environment"
export SURVIVE_CREATOR_TOKEN=desktop-fixture-loopback-only-credential
cat > "$fixture/config.toml" <<CONFIG
data_dir=$data_value
source_dir=$source_value
listen="127.0.0.1:8088"
env_listen="10.203.0.1:8089"

[[creators]]
id="desktop-fixture"
token_env="SURVIVE_CREATOR_TOKEN"

[gateway.prices."gpt-6-cua-fixture"]
input=2
output=3

[[gateway.providers]]
name="loopback-fixture"
base_url="http://127.0.0.1:18090/v1"
protocols=["responses"]
models=["gpt-6-cua-fixture"]

[sandbox]
CONFIG
cat "$fixture/sandbox.toml" >> "$fixture/config.toml"
"$target/survive-server" --config "$fixture/config.toml" --env-file "$fixture/environment" > "$fixture/server.log" 2>&1 &
server=$!
cleanup() {
    kill "$server" 2>/dev/null || true
    wait "$server" || true
}
trap cleanup EXIT
echo "Private desktop fixture: $fixture"
timeout 420 "$python_bin" -u "$here/desktop_acceptance.py" "$fixture"
id=$(cat "$fixture/organism-id")
"$target/survive-sandbox" "$fixture/sandbox.toml" stop "$id"
[[ $("$target/survive-sandbox" "$fixture/sandbox.toml" status "$id") == '"stopped"' ]]
cg=/sys/fs/cgroup/survive/survive-$id
[[ ! -e $cg/cgroup.events ]] || grep -qx 'populated 0' "$cg/cgroup.events"
echo "Confirmed youki stopped and guest cgroup empty."
"$python_bin" -u "$here/desktop_acceptance.py" "$fixture" --verify-stopped-frame

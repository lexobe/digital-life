#!/bin/sh
# Run on the designated Linux host after building native binaries. No model calls.
set -eu
test "$(id -u)" = 0
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
test -f "$project_root/crates/console/dist/index.html" || {
    echo 'Build the Rust console before installing: sh crates/console/build.sh' >&2
    exit 1
}
for name in survive-server survive-agent survive-init survive-sandbox; do
    test -x "$project_root/target/release/$name" || {
        echo "Missing release binary: $name" >&2
        exit 1
    }
done
install -d -m 0755 /opt/survive-platform/bin
install -d -m 0755 /opt/survive-platform/console
install -d -m 0700 /etc/survive-platform /var/lib/survive-platform
for name in survive-server survive-agent survive-init survive-sandbox; do
    install -m 0755 "$project_root/target/release/$name" "/opt/survive-platform/bin/$name"
done
cp -R "$project_root/crates/console/dist/." /opt/survive-platform/console/
install -m 0755 "$project_root/deploy/start-pre.sh" /opt/survive-platform/bin/start-pre
if ! test -f /etc/survive-platform/environment; then
    creator_token=$(openssl rand -hex 32)
    umask 077
    {
        printf 'SURVIVE_CREATOR_TOKEN=%s\n' "$creator_token"
        printf 'SURVIVE_PRIMARY_API_KEY=\nSURVIVE_CLIPROXY_API_KEY=\nRUST_LOG=info\n'
    } > /etc/survive-platform/environment
    unset creator_token
fi
chmod 0600 /etc/survive-platform/environment
if ! test -f /etc/survive-platform/config.toml; then
    install -m 0600 "$project_root/config.example.toml" /etc/survive-platform/config.toml
fi
# Export with the Rust parser so later provider tables cannot leak into sandbox config.
sandbox_export=$(mktemp /etc/survive-platform/.sandbox.XXXXXX)
trap 'rm -f "$sandbox_export"' EXIT HUP INT TERM
/opt/survive-platform/bin/survive-server --config /etc/survive-platform/config.toml --export-sandbox > "$sandbox_export"
chmod 0600 "$sandbox_export"
mv "$sandbox_export" /etc/survive-platform/sandbox.toml
install -m 0644 "$project_root/deploy/survive-platform.service" /etc/systemd/system/survive-platform.service
systemctl daemon-reload
systemctl enable survive-platform.service
if test "${1:-}" != "--no-start"; then
    systemctl restart survive-platform.service
    systemctl --no-pager --full status survive-platform.service
fi

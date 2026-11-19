#!/bin/sh
# Build the exact upstream revision and report actual compile-time capabilities.
set -eu
test "$(uname -s)" = Linux
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_dir=${SURVIVE_BUILD_DIR:-/opt/survive-platform/build}
install -d "$build_dir/youki-source"
curl -fsSL --connect-timeout 10 --max-time 120 \
  https://codeload.github.com/youki-dev/youki/tar.gz/94ba653efbb180ce04650f6ae01a8e6bc8f96d92 \
  -o "$build_dir/youki-source.tar.gz"
if ! test -f "$build_dir/youki-source/.survive-feature-report-patched"; then
  tar -xzf "$build_dir/youki-source.tar.gz" --strip-components=1 -C "$build_dir/youki-source"
  patch -d "$build_dir/youki-source" -p1 < "$project_root/deploy/youki-feature-report.patch"
  touch "$build_dir/youki-source/.survive-feature-report-patched"
fi
docker build -t survive-youki-builder:0.1 -f "$project_root/deploy/Dockerfile.youki" "$project_root/deploy"
docker run --rm --cpus=1.5 --cpu-shares=128 --memory=1536m --memory-swap=2048m \
  -v survive-cargo-cache:/cargo-cache -v "$build_dir/youki-source:/source" \
  survive-youki-builder:0.1 cargo build --release --locked -p youki --bin youki \
  --no-default-features --features v2,seccomp,cgroupsv2_devices -j 1
install -d /opt/survive-platform/bin
install -m 0755 "$build_dir/youki-source/target/release/youki" /opt/survive-platform/bin/youki
cp "$build_dir/youki-source/LICENSE" "$build_dir/youki-LICENSE"
sha256sum /opt/survive-platform/bin/youki > "$build_dir/youki-installed.sha256"
/opt/survive-platform/bin/youki features > "$build_dir/youki-features.json"

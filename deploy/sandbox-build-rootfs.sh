#!/usr/bin/env bash
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo "Linux root required" >&2; exit 1; }
[[ $# == 2 && $2 == /* && ! -e $2 ]] || {
    echo "usage: $0 PINNED_RUST_IMAGE NEW_ABSOLUTE_ROOTFS_PATH" >&2; exit 2;
}
# Docker only builds/exports a trusted template. No Docker sandbox runtime,
# --privileged, --network host, network create/delete, prune, or daemon restart.
dockerfile=$(cd -- "$(dirname -- "$0")" && pwd)/sandbox-rootfs.Dockerfile
context=$(mktemp -d)
cp -- "$dockerfile" "$context/Dockerfile"
image="survive-rootfs-build:$$"
container="survive-rootfs-export-$$"
docker build --network default --build-arg "BASE_IMAGE=$1" --tag "$image" "$context"
docker create --network none --name "$container" "$image"
mkdir -m 755 -- "$2"
docker export "$container" | tar --numeric-owner --no-same-permissions -xpf - -C "$2"
chmod 755 -- "$2"
echo "Exported $2. Retained build image $image, stopped export container $container, and context $context."
echo "No existing Docker containers, networks, rules, or published ports were changed."

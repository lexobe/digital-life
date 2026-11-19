#!/bin/sh
set -eu
console_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
console_target_dir=${CARGO_TARGET_DIR:-"$console_dir/target"}
cargo build --manifest-path "$console_dir/Cargo.toml" --locked \
    --target-dir "$console_target_dir" --target wasm32-unknown-unknown --release
mkdir -p "$console_dir/dist/assets"
wasm-bindgen "$console_target_dir/wasm32-unknown-unknown/release/survive_console.wasm" \
    --target web --out-dir "$console_dir/dist/assets" --out-name survive_console
cp "$console_dir/assets/preview.html" "$console_dir/dist/preview.html"
cp "$console_dir/index.html" "$console_dir/dist/index.html"
cp "$console_dir/assets/style.css" "$console_dir/dist/style.css"
cp "$console_dir/assets/boot.js" "$console_dir/dist/boot.js"
printf 'Console built: %s/dist\n' "$console_dir"

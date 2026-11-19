# Survive Observatory

A browser console written in Rust, compiled to WebAssembly with Yew. The HTTP server hosts the generated assets under `/console/`. No Node.js, npm, remote fonts or external CDN are required to build or run the application. The small `boot.js` module only initializes generated WebAssembly bindings and reports loading failures.

## Build

The frontend is an independent Cargo workspace, so native server builds do not compile browser dependencies. `Cargo.lock` pins frontend dependencies separately.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked
sh crates/console/build.sh
```

The CLI version must match `wasm-bindgen` in the frontend lockfile. The output directory is `crates/console/dist`. Point the server's `console_dir` configuration at the deployed copy of that directory; the server API and frontend should share an origin.

```sh
cargo test --manifest-path crates/console/Cargo.toml --locked
cargo clippy --manifest-path crates/console/Cargo.toml --locked --all-targets -- -D warnings
cargo fmt --manifest-path crates/console/Cargo.toml --check
```

The native contract tests send the form payloads through the core environment, batch birth and grant types, including birth override resolution and report-linked grant retries.

## Navigation

- **Overview** shows creator-owned environments, organism life status, clickable topology nodes and recent recorded events.
- **Environments** lists immutable descriptions and current members. Create an empty environment with an optional editable example, then create organisms in independent batches.
- **Organisms** opens life status, creator dialogue, read-only desktop, current DNA, events, energy ledger, files, reports, grant history and service previews. Open **桌面与对话** on a selected topology node or in its profile. The read-only desktop appears on the left and the conversation on the right. On narrow screens, the conversation appears below the desktop. Desktop Enter sends and Shift+Enter inserts a line break. Touch users send with the button. You can compose another message while a send is in progress. Drafts are kept per organism for the current login session. Earlier messages load above the current reading position. A jump-to-latest control shows incoming messages while you are reading older ones. Each message accepts up to 2,048 UTF-8 bytes. A stopped organism can receive a message in its inbox and read it after resuming. You can grant energy or extend lifetime from its profile.
- Click the desktop image for an enlarged view and press Escape to return. The desktop panel requests live frames while the organism runs. When it stops, the viewer shows the most recent saved capture with its capture time when available. If a running desktop repeatedly returns no frame, the viewer also checks for a saved capture. Service previews, workspace files and reports remain accessible from the fallback view.
- **Infrastructure** displays runtime readiness, public model prices, provider configuration and resource limits.
- **API** links to the deployed usage guide and OpenAPI document.

The creator bearer token stays in page memory. Rust/Wasm retrieves the registered service or its archive, resolves local assets under that service route and builds a self-contained page. The fixed preview frame receives only that page and runs with an opaque origin and a network-blocking CSP. Browser-side tools and bundled downloads work in archived previews. Server-backed actions require the running service. Open `/console/#demo` to explore synthetic records locally. Demo forms disable submission. See [the console design](../../docs/CONSOLE_DESIGN.md) for interaction and data contracts.

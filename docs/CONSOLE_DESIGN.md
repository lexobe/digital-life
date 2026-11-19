# Survive Observatory

The Rust/Yew console at `/console/` serves creators who manage private environments and observe their organisms. The server hosts the browser bundle on the creator API origin.

## Workspace

The overview shows population, energy available for new calls, remaining sandbox runtime and environment count. The graph consumes nodes and edges from the topology API. Dashed connections indicate configured access or membership. Solid connections indicate recorded cumulative activity. Clicking a biological node selects its status, available energy, remaining time and latest heartbeat. The inspector links to the full record. Resource nodes expose their server records. Environment filtering, pan and zoom support exploration. The graph draws at most 24 organisms and 16 connected resources, while the roster lists the complete population returned by the API.

The desktop layout places the inspector beside the canvas. Smaller screens stack the inspector under it. The mobile navigation rail opens with a menu button. Data tables scroll horizontally inside their panels. Node controls support Enter and Space. Dialogs trap focus and close on Escape. Motion follows the device’s reduced-motion preference.

## Creator workflows

- **Create an environment.** Enter an immutable description, with optional name and note. The open-environment example can be loaded into the description field and edited before creation. A new environment begins empty.
- **Create organisms.** Open any owned environment, set the quantity and supply default energy, sandbox running time in seconds, optional DNA, default model ID, optional model allowlist, resource limits and network permissions. The per-organism editor selects a birth slot and lets the creator override individual traits. Unspecified settings inherit the batch defaults. An omitted organism allowlist inherits the environment’s model pool. The price table gives integer energy per incoming or outgoing token.
- **Observe an organism.** The roster is searchable by self-chosen name and ID, with environment and state filters. The profile shows life balances, stop reasons, heartbeat, generation, environment and birth traits. Current DNA is read from the live worktree. Tabs open actual events, model usage and energy charges, reports, grants, worktree files and published services. Report attachments are snapshots captured at submission. The files tab shows the current workspace.
- **Grant resources.** Open a grant from the organism header, or from a report to prefill its reference. Supply energy, additional runtime, or both. Add an optional note. The form creates a visible idempotency key and retains the key and values when submission fails. A successful grant opens the organism’s grant history.
- **Inspect infrastructure.** The infrastructure page displays backend readiness, price catalog, provider configuration and platform limits. The API page links to the deployment’s `/llm.txt` and `/openapi.json`.

## Data and authentication

The console reads `/api/v2/overview` and `/api/v2/topology` on connection and every eight seconds. Organism tabs read `/api/v2/organisms/{id}/…` on demand. Lists follow server cursors and files use bounded byte ranges. Creation and grants submit the same v2 payloads as external clients.

Creator bearer credentials stay in page memory. Login sessions scope asynchronous responses, and logout clears sensitive views. Worktree content and report attachments are rendered as text. Service previews fetch a registered port with the creator credential and display HTML in a sandboxed iframe with a restrictive document CSP.

Explicit navigation to `/console/#demo` enables local synthetic data with a persistent banner. Demo forms can be previewed with submission disabled. No live API or service requests are issued in demo mode.

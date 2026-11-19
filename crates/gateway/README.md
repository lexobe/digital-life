# survive-gateway

The crate exposes `Gateway::new(GatewayConfig, Store) -> anyhow::Result<Gateway>`,
`completion(agent_id, Protocol::{Chat,Responses}, serde_json::Value)`,
`response(agent_id, response_id)`, and `models()`. `Gateway` is cloneable;
`GatewayError` implements `Error`, `Display`, and Axum `IntoResponse`.

The HTTP server authenticates and source-binds callers before invoking these
methods. It should also cap the incoming HTTP body before parsing JSON. The
gateway checks the serialized request limit again, validates the organism's
model permissions, and uses provider model mappings to select the deployment.

In deployment configuration, use `models = ["gpt-6.2-sol"]` to expose an upstream model ID directly. The catalog, requests and price table then use that exact ID. Existing route maps are still accepted.

Each model has integer energy rates for an input token and an output
token. Configure them under `[gateway.prices.<alias>]` with the keys `input`
and `output`. Each rate must be positive. A configured table must quote every
routed logical alias. Older configurations without a table use one unit of
energy per token for both rates. `Gateway::catalog(allowed_models)` returns
allowed models, both rates, and route availability.

`example.toml` contains an embeddable `[gateway]` section. Configuration uses
serde defaults and rejects unknown fields. Rust callers can use
`GatewayConfig { providers, ..Default::default() }`. Each `ProviderConfig` has
`name`, `base_url`, `api_key_env: Option<String>`,
`models: BTreeMap<String, String>`, and `protocols: Vec<Protocol>`. In TOML,
`protocols` defaults to `["chat", "responses"]` and `api_key_env` is optional
for explicitly unauthenticated/self-hosted endpoints.

Credentials are read from the named environment variables once during
construction. Missing or empty variables disable the associated route while
allowing the server to start; requests without an eligible route receive a
generic 503 mentioning credentials and model routes. Reconstruct the gateway
after changing environment credentials. No request, error, model list, or log
contains credentials or provider URLs added by the gateway. Returned payloads
are scrubbed for configured secret values and URLs. Full payloads and events
are retained with the same redaction applied before persistence, including
escaped JSON, because agents can read their own event history.
HTTP redirects are disabled, including same-origin redirects; ambient proxy
environment variables are ignored.

## Accounting and retries

Every inference attempt reserves energy before sending. The ledger pins the
logical model and its rates at reservation time across retries and later usage
reconciliation. Input and output quantities are multiplied by their respective
rates and added with checked integer arithmetic. Input reservation
is the larger of `minimum_input_reservation` and request bytes plus known prior
response context; output reservation is the requested limit times Chat `n`.
Explicit limits must be positive integers and the aggregate output limit cannot
exceed `max_output_tokens`. Missing limits are injected. For streamed Chat,
`stream_options.include_usage` is always true.

The input reservation is an estimate, not a tokenizer or a hard upstream billing
cap. Provider tokenization, remotely fetched images/audio, hidden context and
hosted tools can exceed it. Set the minimum for the workloads/providers you
allow. Exact valid reported token usage settles through Store, including usage
above the reservation. Non-token tool fees are outside this ledger.

Connection failures and complete explicit rejection statuses
400/401/403/404/405/413/415/422/429 release reservations. Ambiguous failures,
timeouts, malformed/oversized results, and absent/invalid usage remain reserved
as `Unknown`. Valid usage requires nonnegative integer input and output counts,
checked arithmetic, and a consistent total when supplied. Cached/reasoning
token detail counts are already included in the top-level totals.

Stateless requests can move to the next eligible provider after a connection
failure, retryable HTTP error, or malformed response, before streaming starts.
Each ambiguous attempt remains charged against availability independently.
Sticky references, hosted tools and provider prompt resources disable retries.
There is no failover once an SSE response is handed downstream. A crash preserves
the durable `Reserved` balance; it cannot refund a request automatically.
Client cancellation does not cancel the worker or its accounting.

The parent can schedule `reconcile_unknown().await -> Result<usize, GatewayError>`
every 30 seconds. It skips live workers and selects only `Unknown` reservations
with an upstream ID and a matching owner/provider binding. It retrieves Responses
from that configured provider, or Chat completions only when `store: true` was
explicitly requested. Retrieval uses the existing concurrency limits, no new
token reservation, no redirects, a maximum ten-second transport timeout and the
response byte limit. Only a matching ID, protocol object, terminal status and
valid usage allow settlement; missing credentials, missing resources and
unverifiable usage leave balances held. Repeated recovery is idempotent. The
result counts verified settlements. Store's atomic idempotency prevents duplicate
charges if another worker already settled the same usage. Concurrent calls on the same gateway
return zero while a reconciliation pass is already running.

Each pass examines at most `reconcile_max_attempts` reservations (default 16,
allowed 1..1024). A cursor shared by gateway clones rotates through entries,
including unrecoverable entries, so they cannot starve later ones. The shared
`reconcile_time_budget_ms` deadline (default 20000, allowed 1..60000) bounds
network work across the whole pass. Database operations finish normally rather
than being cancelled mid-settlement. A bounded batch is selected inside
`spawn_blocking` and only that batch remains in memory during HTTP requests.

Selection uses Store's indexed, cyclic
`unknown_reservations(after: Option<&str>, limit: usize)` query. It loads at most
one configured batch and never loads complete ledger or agent histories. If the
cursor names the only unresolved attempt, an empty page restarts from the
beginning so the singleton is retried. Settlement relies on Store's atomic
idempotency instead of scanning history before a write. This crate does not
modify Store or implement the parent's boot recovery loop.

## Responses and streaming

`previous_response_id`, input item IDs, and tool call outputs referencing stored
calls are owner scoped and pinned to the originating provider. Mixed-provider
references are rejected. Full inline function call/output history without remote
IDs remains stateless. Each response, output item and call ID is bound before its
event is sent. Completed response snapshots are persisted, including full output,
and `response()` returns the owner's local snapshot without network or ledger
charges. Item IDs cannot be used as response retrieval IDs.

Background Responses and Conversations are rejected: their independently
billable lifecycles are not represented by this API. No Files/Conversations CRUD,
response cancellation API, or provider-side asynchronous polling is implemented.
A disconnected/incomplete response may have an `in_progress` local snapshot;
this is not an assertion that the upstream is still executing.

Defaults match the 512 MiB server configuration: global concurrency 16, 2
requests per agent, 262144 request bytes, 4194304 response bytes, 262144 bytes
per SSE event, and 8 buffered downstream events. These are protocol-buffer
limits; Store cache and other server allocations are managed by the parent.

SSE supports fragmented UTF-8, BOM, LF/CRLF/CR boundaries, multiline `data`, and
metadata/comments. Every complete event is persisted before forwarding; a
terminal event is forwarded after accounting. Chat requires `[DONE]`; Responses
requires a completed/incomplete/failed terminal event. EOF without a terminal
marker produces an error event and unknown usage, even if an earlier usage-like
event appeared. Tool-call deltas are forwarded unchanged as data.

Workers retain only the current decoder buffer, a bounded batch of decoded
events, one response snapshot and a bounded downstream channel. There is no
in-memory transcript. `max_response_bytes` caps total upstream bytes per attempt,
and `max_sse_event_bytes` caps individual frames. A client whose bounded queue
cannot accept an event for one second is disconnected while the worker continues
draining upstream to recover usage. Upstream requests have a total transport
timeout. Global and per-agent limits reject excess concurrency with HTTP 429;
permits stay held until the worker, including disconnected stream drainage, ends.

## Validation

Run `cargo test -p survive-gateway`. Tests create temporary real Store databases
and local Axum upstreams; they never call external models. Tests cover routing,
tool payloads, Responses ownership/stickiness, accounting, retries, missing
credentials, concurrency, cancellation, timeouts, response size limits, and SSE
framing. TCP integration tests require permission to bind loopback listeners.
`cargo test -p survive-gateway --lib` additionally exercises stream HTTP bodies,
real Store accounting, late usage recovery, ID/owner validation, and redaction
without opening sockets.

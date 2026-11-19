# survive-agent

The Rust seed core runs continuously inside an organism's youki sandbox. Each turn reads the environment's immutable description and life status, reloads the organism's DNA, and makes an inference through the platform gateway. GPT-6 uses the Responses API for function tools. The organism can change its own name, select its next model, report to its creator, and replace its executable.

## Workspace and boot

The platform seeds `/work/dna.md` with optional birth DNA, copies buildable Rust sources to `/work/source` and installs the initial entrypoint in `/work/boot.json`. DNA can be empty. The organism owns these files. The independent sandbox init reads boot.json and launches its argv. Replacing boot.json takes effect on the next restart. Failed modifications remain available for the organism's own recovery processes.

`/work/model.txt` stores an optional logical model selection. Its absence uses the organism's birth model. The model catalog gives the aliases the organism can use and their current input and output energy prices. Selection takes effect at the next inference and persists across restarts.

| Variable | Meaning |
|---|---|
| SURVIVE_ENV_URL | Private environment and LLM gateway base URL |
| SURVIVE_TOKEN | Per-organism credential supplied by the sandbox |
| SURVIVE_WORK_DIR | Writable work directory, default /work |
| SURVIVE_CONTEXT_BYTES | Maximum serialized request size, default and ceiling 262144 bytes. Text context is capped at 65536 bytes |
| SURVIVE_OUTPUT_TOKENS | Maximum output tokens per seed inference, default 4096 |

## Tools and accounting

Every inference receives a fresh, dedicated life observation from the authenticated `GET /env/v2/self` response, even when other environment metadata is truncated. It includes `energy_balance`, `energy_available`, `energy_reserved` (energy units), `remaining_lifetime_ms` (milliseconds of running sandbox time), and `stop_reasons`. A zero balance or zero remaining lifetime prevents a new inference. Fully reserved available energy makes the core wait for settlement or a grant.

- `env` reads self, life, models, peers, events, reports, messages or ledger. The environment keeps authoritative descriptions, energy and time. Once per hour it records a life update. Inference follows the organism's own actions and sleep decisions.
- `reply` answers an incoming creator message in the organism's conversation. The oldest unanswered message appears in full with its sequence on each inference until answered. Pass that sequence as `reply_to` and up to 2048 UTF-8 bytes of nonblank text. The seed uses a stable idempotency key for each parent message. Recent conversation entries appear when space permits. Long sleeps check the inbox every two seconds and finish early when a message is waiting. Remaining actions in the batch still run.
- `choose_model` reads the current catalog and persists the selected alias. The gateway checks access and balance on each request. Chat Completions and Responses use standard model token usage, with a separate energy charge at the quoted rate.
- `report` sends a body, a stable idempotency key and optional workspace file paths to its creator. File snapshots retain their contents at submission. A creator can grant energy or running time independently.
- `name` updates the display name. The stable organism ID identifies it across renames and restarts.
- `shell`, `read_file` and `write_file` execute commands and inspect or modify files inside the sandbox. `service` registers a listening port, `sleep` pauses the seed loop and `restart` asks the platform to relaunch the current boot entry.
- The full protocol is described in [llm.txt](../../docs/llm.txt). The platform owns grant authorization, runtime metering and cgroup termination. Fully reserved energy causes the seed to wait. Actual exhaustion of energy or lifetime stops the organism until the creator replenishes the missing resource.

### Desktop

The `computer` function operates the 1280 × 720 X11 desktop inside the sandbox. The seed uses `scrot` for observation, `xdotool` for input, and `xclip` for UTF-8 clipboard paste. Window coordinates are pixels measured from the top-left of the full screen. For example, an organism can focus Chromium's address bar, paste a URL, navigate, then inspect the page by issuing these ordered calls.

```json
[
  {"action":"key","key":"ctrl+l"},
  {"action":"type","text":"http://127.0.0.1:8080"},
  {"action":"key","key":"Return"},
  {"action":"wait","ms":500},
  {"action":"screenshot"}
]
```

| Action | Arguments |
|---|---|
| `screenshot` | The most recent successful screenshot in the batch supplies one JPEG to the next inference. |
| `move`, `click`, `double_click` | `x`, `y`, with optional `button` (left, middle, right) for clicks |
| `drag` | `x`, `y`, `to_x`, `to_y`, and optional `button` |
| `scroll` | `x`, `y`, `direction` (up, down, left, right), and optional `steps` (1–20, default 3) |
| `type` | `text` containing 1–4096 Unicode characters. Replaces the sandbox clipboard and pastes with Ctrl+V. Set `terminal` to true for Ctrl+Shift+V. |
| `key` | `key` with a single key or chord, e.g. `Return`, `Escape`, `Ctrl+Shift+Tab`, `Alt+F4` |
| `wait` | `ms` from 0 through 10000 |

Use a vision-capable model to interpret screenshots. The seed keeps the screen at native resolution, reducing JPEG quality as needed to fit 128 KiB. For Responses it uses an `input_image` part. For Chat Completions it uses an `image_url` part. One frame takes precedence over optional history. If its wire size or energy hold exceeds the budget, the seed sends a text observation explaining that the pixels were omitted. An organism can request a fresh screenshot after interacting.

The sandbox init also starts `/opt/survive/survive-agent --desktop-observer` once its desktop is ready. This independent process long-polls the private environment API. A viewer ticket triggers a fresh JPEG in a private temporary directory and a single authenticated upload. The observer has no model loop or workspace journal. It connects to the existing environment service and exposes no listening port.

The standalone seed workspace includes `crates/core`, `crates/agent` and the manifest and lockfile from `seed/`. Building inside a sandbox uses its own toolchain, memory, storage and dependencies.

## Persistence and context

Each `.agent/history/00000000000000000001/` directory contains:

* `state.json`: versioned checkpoint with full normalized assistant message,
  original tool arguments, status/results, and pending batch order.
* `dna.md`, `model-request.json`: current DNA and inference request. Image URLs in the request record use an `artifact:` reference to the original JPEG.
* `self/` and `model/`: request metadata/body (excluding auth), response metadata,
  raw `response.body`, completion marker `received.json`, or transport error.
* `action-0000/`, etc.: immutable intent/result records, tool HTTP artifacts when
  applicable, full `stdout`/`stderr` files for shell commands, and `screen.jpg` for a successful screenshot.

Checkpoints/intents/results use write–fsync–rename–directory-fsync before dependent
side effects. Tool state transitions are `planned -> running -> done`; an
interrupted `running` action with no durable result becomes `uncertain`. It is
never automatically executed again, even for shell, sleep, report, or restart.
Already saved results are recovered without re-executing their action. Actions
still `planned` resume in order. A complete durable model response can be parsed
on restart if no tool intents have begun; an incomplete/uncertain request is
retained and not replayed. Raw bytes from incomplete responses are never tools.
The core provides at-most-once automatic **attempts**, not an impossible guarantee
of exactly-once remote effects across crashes. Models can deliberately issue a
new call after examining uncertainty; the runtime cannot infer semantic duplicates.

All history remains on disk. Each model request starts with the immutable
environment description. Required context also includes the current DNA, life,
and oldest unanswered creator message. The remaining space holds recent chat,
a bounded current environment preview, and whole recent assistant/tool batches
that fit the serialized request budget (considering at most 32 prior iterations).
Tool previews default to 4096 bytes and point to their full artifact; large
batches can be omitted entirely. The prompt is never silently shortened: an
oversized required context fails before inference. Full response bodies are
spooled to disk; bodies over 8 MiB are retained but not parsed/executed. History
and raw outputs have no retention deletion or disk quota here; the sandbox must
provision durable storage. Editing/corrupting journal files can halt the core;
they are accessible to the agent, and there is deliberately no self-repair.

## Tests and library

```sh
cargo test -p survive-agent
```

Tests use a local authenticated mock environment/gateway, temporary work
directories, and local shell/rustc. They make no real model calls and do not deploy.
They cover prompt reload, exact HTTP bodies, all tools, bounded context/full
history, truncated batches, uncertain effects, pending batches through restart,
durable model-response recovery, backoff, file locking, energy exhaustion, shell
timeouts, and compiling a replacement entrypoint. The library exposes `Config`,
`Agent::open`, `Agent::step`, `Agent::run`, and `StepOutcome`. Tests/integrators can
set limits/durations through `Config` without changing production defaults.

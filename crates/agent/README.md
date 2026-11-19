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
| SURVIVE_CONTEXT_BYTES | Maximum serialized request size, default 65536 bytes |
| SURVIVE_OUTPUT_TOKENS | Maximum output tokens per seed inference, default 4096 |

## Tools and accounting

- `env` reads self, life, models, peers, events, reports or ledger. The environment keeps authoritative descriptions, energy and time. Once per hour it records a life update. Inference follows the organism's own actions and sleep decisions.
- `choose_model` reads the current catalog and persists the selected alias. The gateway checks access and balance on each request. Chat Completions and Responses use standard model token usage, with a separate energy charge at the quoted rate.
- `report` sends a body, a stable idempotency key and optional workspace file paths to its creator. File snapshots retain their contents at submission. A creator can grant energy or running time independently.
- `name` updates the display name. The stable organism ID identifies it across renames and restarts.
- `shell`, `read_file` and `write_file` execute commands and inspect or modify files inside the sandbox. `service` registers a listening port, `sleep` pauses the seed loop and `restart` asks the platform to relaunch the current boot entry.
- The full protocol is described in [llm.txt](../../docs/llm.txt). The platform owns grant authorization, runtime metering and cgroup termination. Fully reserved energy causes the seed to wait. Actual exhaustion of energy or lifetime stops the organism until the creator replenishes the missing resource.

The standalone seed workspace includes `crates/core`, `crates/agent` and the manifest and lockfile from `seed/`. Building inside a sandbox uses its own toolchain, memory, storage and dependencies.

## Persistence and context

Each `.agent/history/00000000000000000001/` directory contains:

* `state.json`: versioned checkpoint with full normalized assistant message,
  original tool arguments, status/results, and pending batch order.
* `dna.md`, `model-request.json`: current DNA and full inference request.
* `self/` and `model/`: request metadata/body (excluding auth), response metadata,
  raw `response.body`, completion marker `received.json`, or transport error.
* `action-0000/`, etc.: immutable intent/result records, tool HTTP artifacts when
  applicable, and full `stdout`/`stderr` files for shell commands.

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

All history remains on disk. Context includes the current current DNA, a
bounded current environment preview, and whole recent assistant/tool batches
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

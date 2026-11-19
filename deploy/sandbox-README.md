# Rootful youki sandbox

Implementation lives in `crates/sandbox`; the Rust server manages individual lifecycle and accounting. Runtime operations require Linux host root, cgroup v2 with `cgroup.kill`, OverlayFS, loop/ext4 support, an ext4/XFS data filesystem, iproute2, nftables, iptables-nft, e2fsprogs, util-linux and coreutils. youki must include cgroup-v2 and seccomp support. There is no Docker runtime fallback or host networking.

The example config exactly matches `SandboxConfig`: [sandbox-config.example.toml](sandbox-config.example.toml). It uses the supplied `/opt/survive-platform/bin/youki` and `/var/lib/survive-rootfs`. The rootfs and binaries must be trusted, root-owned, and inaccessible to sandbox writes. Use a patched, checksum-verified runtime; youki 0.7.0 exposes cgroup features through `features` and its seccomp build through `--version` ([upstream implementation](https://github.com/youki-dev/youki/blob/v0.7.0/crates/youki/src/commands/info.rs)). Actual namespace, UID, capability, seccomp and cgroup properties are checked before enabling each workload's network.

## Explicit setup and preflight

Build Linux binaries with a libc compatible with the rootfs (build against Bookworm or use a suitable static target for init/agent). Ubuntu-built dynamically linked binaries may require a newer glibc than Bookworm. Install `survive-sandbox`, `survive-init`, `survive-agent`, and youki at the configured paths. Do not run the host manager inside Docker or with a private mount namespace that disappears on exit.

```sh
cargo build --release --locked -p survive-sandbox --bins -p survive-agent
# After installing binaries and reviewing the standalone sandbox TOML:
sudo deploy/sandbox-setup.sh setup /etc/survive/sandbox.toml
sudo deploy/sandbox-preflight.sh /etc/survive/sandbox.toml
```

`doctor`/`preflight` only reads the host. `setup` is an explicit mutation: it creates dedicated cgroups, namespaces, interfaces, routes and firewall rules. `prepare`/`start` never call setup. Setup requires host IPv4 forwarding to be enabled already; Docker hosts normally have it enabled. No global forwarding sysctl, firewall policy, Docker rule, Docker bridge, existing container, public port 80/443, or Caddy configuration is changed. The manager enables controllers additively and sets forwarding only inside its own routing namespace.

Setup rejects existing routes overlapping `10.203.0.0/16` or `10.204.0.0/30`, namespace/interface name collisions, preexisting owned firewall objects and incomplete setup. `doctor` checks stored boot identity, configuration identity, exact owned firewall snapshots, controller limits, runtime build features and scoped iptables jumps. Same-boot drift fails closed. Do not blindly refresh expected snapshots to hide a drift error.

Reserve the mapped UID/GID ranges for this installation: `uid_base + (slot - 1) * 65536 .. +65535`, default base 65536. Never create host users or run unrelated host processes under those IDs. Root inside an agent maps to its own nonzero host UID, and package users retain their IDs within that range.

For an optional fresh rootfs, [sandbox-build-rootfs.sh](sandbox-build-rootfs.sh) builds and exports [sandbox-rootfs.Dockerfile](sandbox-rootfs.Dockerfile). Pass a digest-pinned Rust image. Docker is used only to export the trusted filesystem; the export container never starts and uses network `none`. Existing unrelated Docker networks are never joined or altered. Existing exported rootfs can be used directly; private per-agent copies get empty mount targets during prepare.

## Filesystem, tools and hard limits

The parent seeds `data_dir/agents/<id>/work/{boot.json,dna.md,model.txt,source/...}` before the first `prepare`. The source workspace can contain only `crates/agent` and `crates/core`, with its trimmed workspace Cargo.toml and Cargo.lock. Prepare preserves those files, copies them into bounded storage without following symlinks, then bind-mounts the bounded workspace at the exact same host path. The original trusted seeds remain hidden beneath that bind mount, never restored automatically.

`data_dir` remains root-owned mode **0700**, including when it is `/var/lib/survive-platform`; the manager never grants world search access to the database/credential tree. Before create, with the old cgroup empty, it exposes exactly four bind views (`rootfs`, `work`, `assets`, `resolv.conf`) under `/run/survive-platform/<id>`. The staging base is root:root **0711**. Each individual directory is host-root-owned with GID `uid_base + (slot - 1) * 65536` and mode **0710**: only host root and that mapped group can search it. The mapped group cannot list or write the directory, and other individuals cannot traverse it. OCI root and bind source paths use these views; runtime configuration, tokens, logs and database files stay in the private data tree.

Staging rejects symlinks, unexpected directory entries/permissions, stacked mounts, mismatched source device/inode identities, and incorrect mount flags. Views are private mounts with `nosuid,nodev`; assets and resolver views are read-only. Binding never changes source UID/GID ownership. A new image's overlay upper root is initialized with the individual's mapped ownership before exposure, so its merged root is not host-root-owned. Existing agent filesystems are not recursively repaired or chowned. `/run` views disappear on reboot and are rebuilt at the next explicit start after restore remounts persistent storage. Stopped views may remain mounted until reboot or explicit administrative fixture cleanup.

Each individual has a fully preallocated `storage.ext4` image of `disk_bytes` (default 2 GiB). Its filesystem contains `work`, overlay `upper`, and overlay workdir. Both `/work` and package installations/rootfs edits consume this one hard capacity and inode limit. `fallocate` reserves backing storage before admission; ext4 formatting uses `nodiscard`, and guest mounts cannot discard the host file. Data directories on CoW/network filesystems are rejected. An admission fails if host storage cannot reserve the image. Allow space for the trusted lower-template copy and original seeds in addition to the image; those files are not workload-writable. Images, lower copies and seeds are retained after stop and require explicit administrative deletion after retirement. No automatic pruning occurs.

The trusted rootfs is copied once per agent (reflink where available), its ownership shifted, and mounted as an OverlayFS lower layer. The upper layer survives stop/start and reboot restore. `/work` also persists. `/tmp`, `/run`, `/dev` and `/dev/shm` are bounded tmpfs mounts charged to the memory cgroup. Workload stdout/stderr/stdin are `/dev/null`; trusted init closes all inherited descriptors above 2 before reading mutable boot state or spawning the core. Agent history and diagnostics should be written under `/work`.

During first preparation, the trusted template copy receives at least 180 seconds to accommodate a full disk copy on ext4. Runtime, network and other administrative commands use `command_timeout_secs`.

Each new individual's `/etc/hosts` points to its read-only `/opt/survive/hosts` asset. The file resolves `localhost` and the individual's own `agent-<slot>` hostname to loopback. This keeps X11 clients and local HTTP servers usable when Internet access is disabled.

Runtime errors use youki `--log` with a private FIFO. The manager drains it and retains at most 64 KiB per operation in root-owned mode-0600 `agents/<id>/youki-{create,start,state,kill,delete}.log`; repeating an operation replaces its previous log. Errors include up to 16 KiB of retained diagnostics. The create capture stays alive through paused-init wiring and start. No workload file-size rlimit is changed, and youki never receives a descriptor to the regular host log file.

`/opt/survive` is a read-only mount of the configured init and agent binaries; `/opt/survive/survive-agent` always exists. Mutable boot argv may select a rebuilt executable under `/work`. Environment includes:

```text
SURVIVE_ENV_URL=http://10.203.0.1:8089
SURVIVE_TOKEN=<individual credential>
SURVIVE_WORK_DIR=/work
HOME=/work/.home
CARGO_HOME=/work/.cargo
RUSTUP_HOME=/usr/local/rustup
DISPLAY=:0
XDG_RUNTIME_DIR=/run/survive-desktop
PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
```

Cargo/rustc, C build tools, curl, Python and package tools are available from the rootfs. Agents can install dependencies into their own rootfs/workspace within limits. Capabilities are limited to filesystem ownership, local UID/GID switching, chroot, signaling and low port binding; namespace/admin/raw-network capabilities are absent. Seccomp denies privileged operations and additional user/network namespaces while allowing normal compiler processes and threads. `clone3` returns ENOSYS for libc fallback; classic clone flags below `CLONE_NEWCGROUP` permit threads, while `CLONE_NEWNS` still requires the absent `CAP_SYS_ADMIN`. The remaining namespace flags are outside the allowed range.

Each cgroup enforces `memory.max`, zero swap, `memory.oom.group=1`, `pids.max`, and a 100 ms CPU quota period. `/survive` enforces `total_memory_bytes` across agents. The control server belongs outside that hierarchy. No cgroup filesystem or host socket is exposed inside an individual.

## Personal desktop

On each sandbox start, PID 1 starts Xvfb on display `:0` at 1280 × 720, starts Openbox and opens a maximized Chromium window. Seeded guests open the read-only `/opt/survive/home.html` with their environment and registered local service links. It checks the X display, window manager and mapped browser window before launching the mutable core. Each readiness check has a 20-second limit. A desktop startup failure is recorded and the core still launches, so recovery code can inspect the guest. PID 1 reaps the desktop processes along with the other children. Closing a window or terminating a desktop process is an organism action and does not trigger an automatic restart.

Desktop programs use the guest X11 socket under its private `/tmp`. Xvfb disables TCP and the MIT-SHM extension. Chromium uses software rendering and the sandbox network policy. No VNC, browser debugging port, host display or GPU device is needed. `xdotool`, `xclip`, `scrot` and `xterm` are available inside the individual. For example, `scrot -z -o /work/screen.png` captures the current screen and `xdotool getactivewindow getwindowname` reports the focused window title.

Chromium runs as guest UID/GID 65534 with zero effective capabilities. Desktop processes start without `SURVIVE_TOKEN` in their environments. The browser owns its mode-0700 home and persistent profile at `/work/.home/chromium`; its runtime directory is on `/run`. New workspaces allow directory traversal to reach that profile. The host staging directory retains its mapped-group restriction. The outer sandbox denies the setuid elevation and nested user namespaces Chromium normally uses for its inner sandbox, so the launcher passes `--no-sandbox` to Chromium. Browser subprocesses share the browser user's guest privileges. The outer youki seccomp, capabilities, UID mapping, firewall, filesystem and cgroup policy apply to every process.

The desktop and core share one resource budget. A new organism defaults to 768 MiB, 1,000 CPU millicores and 384 tasks. The organism profile records its original birth limits in `limits` and its effective configuration in `runtime_limits`. Lower explicit budgets can prevent Chromium from starting or trigger a cgroup OOM kill. Browser history, downloads and cookies consume the individual's existing disk quota and persist across explicit restarts. Existing prepared individuals keep their copied rootfs and init binary. New individuals use the updated template.

The guest observer can receive a startup capture ticket followed by live-view tickets. The control plane stores the last accepted JPEG per organism and exposes it to the owner through `GET /api/v2/organisms/{id}/desktop/snapshot`, including after shutdown.

PID 1 writes advisory desktop state to `/work/.survive-desktop-status.json`. The `state` values are `starting`, `ready`, `failed` and `exited`. The file includes the display, known desktop process IDs and any startup or exit error. Like the core status, this file belongs to the individual.

## Network and services contract

`survive-router` is a dedicated routing namespace. Host `sv-uplink` owns `10.204.0.1/30` and environment address `10.203.0.1/32`; router `uplink` owns `10.204.0.2/30`. OCI creates each agent's network namespace inside its user namespace; the manager pins it as `sv-<slot>` using the paused init PID. A routed veth pair `sa<slot>`/`eth0` uses source `10.203.<(slot+1)/256>.<(slot+1)%256>/32`. Agent next hop is router link-local `169.254.203.1`, with no shared bridge/L2 segment. IPv6 is disabled and dropped.

Router ingress verifies the source IP against the veth, then enforces the following matrix. Source addresses remain unchanged to the environment server; only Internet egress is masqueraded on the host.

| Origin and destination | Allowed |
| --- | --- |
| Agent → `10.203.0.1:8089` | TCP environment/gateway |
| Agent → another agent | Both have peers enabled, same experiment |
| Agent → public Internet | Its `internet` flag is true |
| Agent → host, Docker/private, loopback, link-local, metadata, reserved/multicast ranges | Denied, except environment connection and established service replies |
| Host `10.203.0.1` → individual | TCP only when that individual has `services=true` |
| Agent → host reply to that service connection | Established reply direction with original source `10.203.0.1` |
| Other inbound Internet/host source → individual | Denied |

**Service proxy must bind its client source to `10.203.0.1`.** The parent restricts requests to registered HTTP ports and issues HTTP itself. Firewall enforcement is TCP/address/state based, not an HTTP parser; it permits the trusted host to contact any TCP port on a services-enabled individual. A client using the route-selected `10.204.0.1` is rejected. Redirects/proxy environment variables must not turn the parent HTTP client into an unrestricted outbound proxy. Host input on agent links remains port 8089 plus established replies to explicitly host-originated service connections.

Add public management/control destinations to `denied_cidrs`; private ranges are always denied. Other public host addresses are blocked by the host input filter. The same-experiment peer table is rebuilt atomically inside the routing namespace on prepare. `peers=false` prevents both incoming and outgoing peer connections. `services` is independent of `peers`.

Owned host `inet survive_host` rules are scoped to `sv-uplink`; they never flush the global ruleset. A dedicated `SURVIVE-FWD` chain and two interface-scoped jumps precede Docker's FORWARD rules so Docker's DROP policy does not break sandbox Internet traffic. Two narrowly scoped INPUT rules permit environment traffic and service replies after the earlier nft guard. Existing Docker chains/rules are unchanged. Other host firewalls may impose additional restrictions; the Linux test verifies real connectivity. Do not delete or relax unrelated firewall rules to make a test pass.

## Lifecycle, failures and reboot

Public API: `SandboxManager::new(config)`, `prepare(&spec)`, `start(id)`, `stop(id)`, `status(id)` and additional `doctor()`, `setup()`, `restore()`, `work_dir(id)`. Manager clones share configuration; operations run in blocking tasks under a cross-process flock. Cancellation does not abandon an in-progress privileged operation. Routine prepare validates the identity, recorded limits and network policy against the existing spec. Repeated prepare with the same completed spec preserves all files. A targeted offline desktop upgrade records its effective quotas separately from the original birth traits.

Prepare builds bounded storage, OCI configuration and firewall policy; it creates no agent namespace or veth. Start kills any old cgroup and removes its old veth and namespace pin, prepares and verifies the four runtime filesystem views, then runs `youki create`. It validates the paused init PID and isolated user/network namespaces, runs `ip netns attach sv-<slot> PID`, and wires the veth and guest routes while the router end stays DOWN. Only after `youki start` and strict namespace-inode/UID/cgroup/capability/seccomp verification does it raise the router end and install the agent route with `route replace`. Existing root-owned OCI configs are regenerated at start; mutable workspace files are preserved. Any start failure disconnects and kills the cgroup; filesystem changes are retained. Stop disconnects first, writes `cgroup.kill`, verifies the group is empty, deletes that runtime record and retires its veth/pin. A legitimately absent agent link is accepted only after a successful router link query; failed queries remain errors. If doctor/firewall is broken, emergency stop still attempts the cgroup kill. Depletion must call `stop`, not just signal the original core PID.

`survive-init` is PID 1. It reads `/work/boot.json` on each explicit start, spawns its argv once, adopts/reaps descendants, and stays alive after a normal/crashed core exit or launch failure. It never kills recovery children, retries the core, rewrites boot.json or restores code automatically. Parent restart is explicit `stop` then `start`, rereading the latest manifest. `Running` means PID 1/cgroup is running, even if the initial core is gone.

PID 1 atomically updates `/work/.survive-init-status.json`:

```json
{"state":"running","pid":150}
{"state":"core_exited","pid":150,"exit_code":42,"signal":null}
{"state":"core_exited","pid":150,"exit_code":null,"signal":11}
{"state":"launch_failed","error":"..."}
```

This is **advisory, agent-writable telemetry**, not a trusted death/authorization signal. The parent may record a core failure event while leaving the sandbox and its recovery processes running. Read with `O_NOFOLLOW|O_NONBLOCK`, require a regular file, cap bytes (64 KiB suffices), and parse JSON; never follow an agent-controlled host symlink or block on a FIFO. Status reporting may fail when the individual fills its disk; PID 1 still keeps recovery alive. No initial core retry is implied by the file.

After a host reboot `doctor` fails with a boot-ID mismatch. `status` reports prepared old instances stopped without consulting their stale PIDs, and `stop` refuses stale PID operations. Explicitly run:

```sh
sudo deploy/sandbox-setup.sh restore /etc/survive/sandbox.toml
sudo deploy/sandbox-preflight.sh /etc/survive/sandbox.toml
# Parent may now start authorized agents individually.
```

Restore requires a *different* kernel boot ID and absence of all owned volatile resources/rules. It quarantines old youki state as `youki-<old-boot-id>` without signaling old PIDs, rebuilds dedicated isolation, mounts existing images/overlays and restores policy. Per-agent namespaces and veths remain absent until explicit start creates a new OCI runtime namespace. All agents remain stopped. Same-boot drift, persisted stale firewall objects, config mismatch, incomplete prepare or a partial restore causes failure with retained diagnostics. No blanket flush, automatic rollback, reboot, fsck repair, rootfs reset or workload autostart occurs. If a firewall persistence service restores owned objects independently, reconcile those owned objects manually before restore; do not persist these volatile rules separately.

## Verification

```sh
cargo test --locked -p survive-sandbox
# Explicit Linux fixture only, after setup; env listener 8089 must be free:
sudo env SURVIVE_LINUX_TEST=YES \
  deploy/sandbox-linux-integration.sh /etc/survive/sandbox.toml
```

The Linux test runs real youki processes, compiles/runs C and Rust, installs an offline Debian package, checks desktop readiness after core exit, recovery behavior, source IPs, peers, service source binding, denied destinations, forged source rejection, PID/memory/disk enforcement, persistent rootfs/work and mutable boot restart. It snapshots existing Docker rules and compares them afterward. Slots 60000..60003 must be free. Provision disk space for four images plus four lower copies and a 2 GiB memory pool for this four-individual fixture (512 MiB and 256 tasks each). The public connectivity probe defaults to `1.1.1.1:443`; override `SURVIVE_PUBLIC_TEST_ADDR` if needed. Tests stop their individuals on success and retain all artifacts; failures also retain evidence, so stop the reported fixture identities explicitly after investigation. They do not run as part of ordinary cargo tests. Reboot restore additionally requires a real reboot drill on a fixture; the test rejects same-boot restore.

The Linux acceptance suite must pass on the target before release. Record separately whether a real host reboot drill was performed; do not reboot a shared production host as part of routine verification. Local macOS tests exercise validation, manifest behavior, generated isolation policy and fail-closed platform handling; they do not establish kernel isolation correctness.


## Control-plane integration

The management console remains available when sandbox preflight fails. `start-pre.sh` only reports diagnostics; it does not install, repair or restore host isolation. The lifecycle service refuses new workload admission until `doctor` succeeds. Operators explicitly run `setup` for a new installation or `restore` after a confirmed host reboot.

Each reconciliation tick revokes zero-balance individuals before performing health or status queries. Confirmed stops can be rechecked without subprocesses only when the trusted marker matches the current boot, the runtime record is absent, the dedicated cgroup is empty, and the namespace pin is absent. Starts invalidate that marker. Observation batches the installation lock; authoritative youki state calls are limited to extant runtime records. The human-readable youki 0.7 `list` output is not used as a substitute for refreshing process state.

Registered HTTP services are reached directly at the individual namespace IP and port. The server still authenticates the observer, checks registration and policy, binds the trusted gateway source address, and treats returned HTML as untrusted. No additional guest proxy or reserved application port is required.

## Control plane startup

`start-pre.sh` preserves guarded setup on first install and restore after a verified change of kernel boot ID. Same-boot startup runs doctor only. Failures remain diagnostic so the observer console can display the unavailable state; workload admission independently requires full preflight success. No same-boot firewall refresh or repair is performed.

# Linux desktop acceptance

From a trusted checkout, run one real organism against a local Responses provider. Before the seed takes its first action, the verifier reads the saved desktop frame and checks the rendered environment homepage. The provider chooses a click target from the screenshot it receives, sends Unicode text through the seed's computer tool, and verifies the result before and after an explicit organism reboot. The seed then changes a visible marker and sleeps. An authenticated creator reads the live viewport and sends a chat message. The seed wakes, replies and continues its work. The test verifies the environment description and life snapshot on each upstream request, seven priced ledger settlements, unchanged billing for viewing the desktop, guest namespaces and cgroup limits, and retrieval of the last desktop frame after shutdown.

## Prepare the host

Use a rootful Linux host with the kernel and command prerequisites in the [youki deployment guide](../sandbox-README.md). The runner also uses Bash, util-linux `unshare`, GNU `timeout`, and host Python 3 with Pillow's JPEG decoder. Set `CUA_PYTHON` to select a virtual environment. Build the host and seed binaries for Linux with a libc compatible with the guest rootfs.

```sh
cargo build --locked --release -p survive-server -p survive-sandbox -p survive-agent --bins
```

Supply the following five paths in order.

1. An unpacked desktop rootfs built with [sandbox-build-rootfs.sh](../sandbox-build-rootfs.sh).
2. The trusted youki executable with cgroup v2 and seccomp support.
3. A directory containing `survive-server`, `survive-sandbox`, `survive-init`, and `survive-agent`.
4. An existing writable ext4 or XFS directory for retained fixture data. Allow room for one full rootfs copy, a 256 MiB preallocated guest disk, the private database and logs. Use root-owned paths without group or world writes for this directory, the rootfs, and the binaries, including their ancestors.
5. A writable cgroup v2 parent whose `cgroup.subtree_control` already enables `cpu`, `memory`, and `pids`. The runner creates and removes its own child beneath this parent.

Place the checkout, interpreter and input files outside `/run`. Each test creates private mount, PID and network namespaces with its own `/run` and network configuration. The fixture uses the default birth limits of 768 MiB RAM, one CPU and 384 PIDs. The runner allocates a 768 MiB sandbox pool. The driver allows seven minutes, and the outer setup, test and teardown have a twelve-minute timeout.

## Run

```sh
sudo deploy/tests/run-desktop-acceptance.sh \
  "$DESKTOP_ROOTFS" "$YOUKI_BIN" "$LINUX_BIN_DIR" "$FIXTURE_PARENT" "$CGROUP_PARENT"
```

Every run creates a fresh directory under `FIXTURE_PARENT` and prints its path before setup. `result.log` records preflight, the Responses turns, the stopped-cgroup check and the stopped snapshot check. `desktop-result.json` includes the two conversation messages, screenshot hashes, guest process identities, remaining lifetime and settled debits. The model receives `input.jpg`, `saved.jpg` and `restored.jpg`. The owner endpoints return the startup frame `initial.jpg` and the subsequent `live.jpg`. The server log and disk image remain available for diagnosis.

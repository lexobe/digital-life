"""Local Responses provider and verifier for desktop-acceptance.sh."""
import base64
import hashlib
import io
import json
import os
import shlex
import sys
import threading
import time
import traceback
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

from PIL import Image

MODEL = "gpt-6-cua-fixture"
DESCRIPTION = "Run the local graphical acceptance scenario."
QUESTION = "桌面恢复了吗？"
ANSWER = "已恢复。Chromium 仍显示已保存的输入。"
ENERGY = 1_000_000
LIFETIME = 300
MEMORY = 768 * 1024**2
INPUT_TOKENS, OUTPUT_TOKENS = 137, 19
COST = 2 * INPUT_TOKENS + 3 * OUTPUT_TOKENS
TEXT = "沙盒 café 🐦"
PAGE = """<!doctype html><html lang="en"><meta charset="utf-8"><title>Desktop acceptance</title>
<style>body{background:#fff;color:#222;margin:4vw;font:36px sans-serif}
input,output{display:block;box-sizing:border-box;width:65vw;height:130px;margin:30px;
padding:20px;border:0;font:32px sans-serif}input{background:#00a2d8}
output{background:white}body[data-stage=saved] output{background:#38bc38}
body[data-stage=restored] output{background:#b642ca}
#live{position:fixed;right:8px;top:8px;width:180px;height:140px;background:#e89214}</style>
<form><label for="entry">Enter the fixture phrase</label>
<input id="entry" autocomplete="off"><output id="result"></output></form><div id="live" hidden>Live frame</div>
<script>
const wanted = "沙盒 café 🐦";
function show(stage, value) {
  document.body.dataset.stage = stage;
  document.getElementById("result").textContent = value;
}
const field = document.getElementById("entry");
let clicked = false;
field.onclick = e => { if (e.isTrusted) clicked = true; };
document.querySelector("form").onsubmit = e => {
  e.preventDefault();
  if (clicked && field.value === wanted) {
    localStorage.setItem("desktop-acceptance", field.value);
    show("saved", field.value);
  }
};
if (localStorage.getItem("desktop-acceptance") === wanted) show("restored", wanted);
document.addEventListener("keydown", e => {
  if (e.isTrusted && e.key === "j" && document.body.dataset.stage === "restored")
    document.getElementById("live").hidden = false;
});
</script></html>"""
# Solid colored rectangles occupy thousands of pixels. Median coordinates stay
# inside the control despite text and antialiasing around the glyphs.
COLORS = {"input": (0, 162, 216), "saved": (56, 188, 56), "restored": (182, 66, 202),
          "live": (232, 146, 20)}
LOCAL_URL = "http://127.0.0.1:8123/desktop.html"
HTTP = build_opener(ProxyHandler({}))


def require(condition, message):
    if not condition:
        raise ValueError(message)


def api(path, body=None):
    req = Request("http://127.0.0.1:8088" + path,
                  headers={"Authorization": "Bearer " + os.environ["SURVIVE_CREATOR_TOKEN"],
                           "Content-Type": "application/json"},
                  data=None if body is None else json.dumps(body).encode())
    with HTTP.open(req, timeout=10) as res:
        return json.load(res)


def snapshot(path, wait=0):
    req = Request("http://127.0.0.1:8088" + path + "/desktop/snapshot",
                  headers={"Authorization": "Bearer " + os.environ["SURVIVE_CREATOR_TOKEN"]})
    deadline = time.monotonic() + wait
    while True:
        with HTTP.open(req, timeout=10) as res:
            if res.status == 200:
                require(res.headers.get_content_type() == "image/jpeg", "expected a saved JPEG frame")
                require(int(res.headers["x-survive-captured-at"]) > 0, "missing snapshot capture time")
                frame = res.read(128 * 1024 + 1)
                require(len(frame) <= 128 * 1024, "saved frame exceeds the capture limit")
                with Image.open(io.BytesIO(frame)) as image:
                    require(image.size == (1280, 720) and image.format == "JPEG", "invalid saved frame")
                    image.verify()
                return frame
            require(res.status == 204 and time.monotonic() < deadline, "no saved frame available")
        time.sleep(0.25)


def check_home_frame(frame):
    # Measure the page area below the browser toolbar. The default environment
    # card includes both a white interior and text on a pale green background.
    with Image.open(io.BytesIO(frame)) as image:
        pixels = image.convert("RGB").load()
        samples = [pixels[x, y] for x in range(8, 1272, 6) for y in range(155, 690, 6)]
    background = sum(all(abs(v - target) <= 5 for v, target in zip(rgb, (236, 242, 237)))
                     for rgb in samples)
    white = sum(min(rgb) >= 250 for rgb in samples)
    text = sum(max(rgb) < 135 for rgb in samples)
    require(background > 300 and white > 300 and text >= 10,
            f"initial frame does not show the rendered environment card ({background}, {white}, {text})")


def computer(action, **args):
    return "computer", {"action": action, **args}


def navigate():
    return [computer("key", key="ctrl+l"), computer("type", text=LOCAL_URL),
            computer("key", key="Return"), computer("wait", ms=1500), computer("screenshot")]


def target_center(jpeg, color):
    with Image.open(io.BytesIO(jpeg)) as image:
        require(image.format == "JPEG" and image.size == (1280, 720), "unexpected screen format or size")
        # Sample physical coordinates. No display scale or window position assumption.
        pixels = image.convert("RGB").load()
        points = [(x, y) for y in range(4, 720, 8) for x in range(4, 1280, 8)
                  if all(abs(a - b) <= 16 for a, b in zip(pixels[x, y], COLORS[color]))]
    require(len(points) >= 300, f"rendered {color} target missing ({len(points)} samples)")
    xs, ys = zip(*points)
    return sorted(xs)[len(xs) // 2], sorted(ys)[len(ys) // 2]


def browser_isolation(organism):
    cg = Path("/sys/fs/cgroup/survive") / ("survive-" + organism)
    for name, expected in (("memory.max", str(MEMORY)), ("pids.max", "384"),
                           ("memory.swap.max", "0"), ("cpu.max", "100000 100000")):
        require((cg / name).read_text().strip() == expected, f"incorrect cgroup {name}")
    processes = {}
    for pid in (cg / "cgroup.procs").read_text().split():
        proc = Path("/proc") / pid
        try:
            command = (proc / "cmdline").read_bytes().split(b"\0")
            name = (proc / "comm").read_text().strip()
        except (FileNotFoundError, ProcessLookupError):
            continue
        if name == "chromium" and any(arg.startswith(b"--type=") for arg in command):
            continue
        processes[name] = proc
    for name in ("Xvfb", "chromium", "survive-agent"):
        require(name in processes, f"{name} absent from organism cgroup")
        proc = processes[name]
        status = dict(line.split(":", 1) for line in (proc / "status").read_text().splitlines())
        require(status["NoNewPrivs"].strip() == "1" and status["Seccomp"].strip() == "2",
                f"{name} lost no_new_privs or seccomp")
        require(int(status["Uid"].split()[0]) != 0, f"{name} has fixture-host uid zero")
        if name == "chromium":
            require(int(status["CapEff"].strip(), 16) == 0, "browser has effective capabilities")
        if name != "survive-agent":
            require(not any(key.startswith(b"SURVIVE_TOKEN=") for key in
                            (proc / "environ").read_bytes().split(b"\0")), f"{name} inherited the guest token")
        for ns in ("mnt", "pid", "net", "ipc", "uts", "user", "cgroup"):
            require((proc / "ns" / ns).stat().st_ino != (Path("/proc/self/ns") / ns).stat().st_ino,
                    f"{name} shares the fixture host {ns} namespace")
    browser = processes["chromium"]
    tcp = (browser / "net/tcp").read_text().splitlines()[1:]
    tcp6 = browser / "net/tcp6"
    if tcp6.exists():
        tcp.extend(tcp6.read_text().splitlines()[1:])
    listening = {int(line.split()[1].split(":")[1], 16) for line in tcp if line.split()[3] == "0A"}
    require(8123 in listening and not listening.intersection({5900, 6000, 9222}),
            f"unexpected desktop TCP listeners {listening}")
    # Kernel namespace inode numbers can be recycled immediately after teardown.
    # Pair the kernel process start timestamp with its PID in the fixture host.
    start_ticks = int((browser / "stat").read_text().rsplit(")", 1)[1].split()[19])
    return {"pid": int(browser.name), "start_ticks": start_ticks,
            "pid_namespace": (browser / "ns/pid").stat().st_ino}


class Scenario:
    def __init__(self, fixture):
        self.fixture = fixture
        self.born = threading.Event()
        self.sent = threading.Event()
        self.error = None
        self.turns = []
        self.previous_tools = []
        self.screens = {}
        self.browsers = []
        self.tools_checked = 0

    def create(self):
        env = api("/api/v2/environments", {"description": DESCRIPTION})
        traits = {"initial_energy": ENERGY, "lifetime_seconds": LIFETIME, "dna": "Use the desktop.",
                  "default_model": MODEL,
                  "network": {"internet": False, "peers": False, "services": False}}
        result = api(f"/api/v2/environments/{env['environment']['id']}/organisms",
                     {"count": 1, "defaults": traits})
        self.organism = result["organisms"][0]["id"]
        self.path = "/api/v2/organisms/" + self.organism
        self.history = self.fixture / "data/agents" / self.organism / "work/.agent/history"
        (self.fixture / "organism-id").write_text(self.organism)
        self.born.set()

    def start_page_server(self, check_home=False):
        marker = shlex.quote(str(self.fixture / "host-only-marker"))
        command = f"""set -eu
test ! -e {marker}
test ! -S /var/run/docker.sock
test -z "${{SURVIVE_CREATOR_TOKEN:-}}"
grep -Fx {shlex.quote(DESCRIPTION)} /opt/survive/environment.md
grep -F {shlex.quote(DESCRIPTION)} /opt/survive/home.html >/dev/null
python3 -m http.server 8123 --bind 127.0.0.1 --directory /work/desktop-fixture > /work/desktop-fixture-http.log 2>&1 &
timeout 30 sh -c 'until curl --noproxy "*" -fsS {LOCAL_URL} -o /dev/null && xdotool search --onlyvisible --class chromium >/dev/null; do sleep 0.25; done'
python3 -c 'import json; assert json.load(open("/work/.survive-desktop-status.json"))["state"] == "ready"'
"""
        if check_home:
            command += """timeout 15 sh -c 'until xdotool search --onlyvisible --class chromium getwindowname %@ | grep -F 生物环境; do sleep 0.25; done' || {
  xdotool search --onlyvisible --class chromium getwindowname %@ >&2
  exit 1
}
"""
        return "shell", {"command": command, "timeout_secs": 40}

    def check_previous_tools(self):
        turn = self.history / f"{self.turns[-1]['iteration']:020d}"
        state = json.loads((turn / "state.json").read_bytes())
        require(state["stage"] == "complete" and len(state["actions"]) == len(self.previous_tools),
                f"unfinished or missing actions in {turn}")
        for index, ((name, args), action) in enumerate(zip(self.previous_tools, state["actions"])):
            result = json.loads((turn / f"action-{index:04d}/result.json").read_bytes())
            require(action["name"] == name and json.loads(action["arguments"]) == args
                    and action["status"] == "done" and result == action["result"] and result.get("ok") is True,
                    f"tool {name} failed in {turn.name}/action-{index:04d}: {result}")
            self.tools_checked += 1
        return turn

    def check_screen(self, request, previous, color):
        images = [part["image_url"] for msg in request["input"] if isinstance(msg.get("content"), list)
                  for part in msg["content"] if part["type"] == "input_image"]
        require(images and images[-1].startswith("data:image/jpeg;base64,"), "missing model-visible JPEG")
        jpeg = base64.b64decode(images[-1].split(",", 1)[1], validate=True)
        last = previous / f"action-{len(self.previous_tools) - 1:04d}/screen.jpg"
        require(last.read_bytes() == jpeg, "inference image differs from the preceding screenshot action")
        (self.fixture / f"{color}.jpg").write_bytes(jpeg)
        self.screens[color] = hashlib.sha256(jpeg).hexdigest()
        return target_center(jpeg, color)

    def reply(self, request):
        require(self.born.wait(5), "inference received before birth completed")
        texts = [item["content"] for item in request["input"] if isinstance(item.get("content"), str)]
        require(request["model"] == MODEL and any(t.get("name") == "computer" for t in request["tools"]),
                "unexpected routed model or missing CUA function definition")
        require(any(t.get("name") == "reply" for t in request["tools"]), "missing conversation reply tool")
        context = {"role": "system", "content": "Environment description:\n" + DESCRIPTION}
        require(request["input"][0] == context and request["input"].count(context) == 1,
                "upstream request missing the canonical environment description")
        count = len(self.turns)
        previous = self.check_previous_tools() if count else None
        # Select the exact current journal record after the seed saved its request.
        current = max(self.history.glob("[0-9]*"))
        saved = json.loads((current / "self/response.body").read_bytes())
        pending = saved["pending_messages"]
        if count == 5:
            require(self.sent.wait(5), "seed woke before creator message was recorded")
            require(len(pending) == 1 and pending[0]["seq"] == self.creator_seq
                    and pending[0]["text"] == QUESTION and pending[0]["role"] == "creator",
                    "seed /self did not contain the creator's pending message")
            require(any(t.endswith(QUESTION) and f"reply_to {self.creator_seq}" in t for t in texts),
                    "upstream input missing pending creator text or sequence")
        else:
            require(not pending, "unexpected pending message")
        life = saved["life"]
        prompt = next((t for t in texts if t.startswith("Current life ") and "\n" in t), None)
        require(prompt is not None, "model input lacks a dedicated life snapshot")
        shown = json.loads(prompt.split("\n", 1)[1])
        for key in ("energy_balance", "energy_available", "energy_reserved", "remaining_lifetime_ms", "stop_reasons"):
            require(shown[key] == life[key], f"model-visible {key} differs from this turn's authenticated /self")
        require(life["energy_balance"] == ENERGY - COST * count, "wrong model-visible debit")
        remaining = life["remaining_lifetime_ms"]
        require(0 < remaining <= LIFETIME * 1000, "invalid remaining lifetime")
        if count:
            require(remaining <= self.turns[-1]["remaining_lifetime_ms"], "lifetime increased without a grant")
        self.turns.append({"iteration": int(current.name), "energy_balance": life["energy_balance"],
                           "remaining_lifetime_ms": remaining})
        if count == 0:
            # No creator demand or seed desktop action has occurred yet.
            initial = snapshot(self.path, wait=20)
            (self.fixture / "initial.jpg").write_bytes(initial)
            check_home_frame(initial)
            self.initial_hash = hashlib.sha256(initial).hexdigest()
            actions = [("write_file", {"path": "desktop-fixture/desktop.html", "content": PAGE}),
                       self.start_page_server(check_home=True), *navigate()]
        elif count == 1:
            self.browsers.append(browser_isolation(self.organism))
            x, y = self.check_screen(request, previous, "input")
            actions = [computer("click", x=x, y=y), computer("type", text=TEXT),
                       computer("key", key="Return"), computer("wait", ms=1500), computer("screenshot")]
        elif count == 2:
            self.check_screen(request, previous, "saved")
            # Give Chromium's localStorage service time to persist before a real cgroup reboot.
            actions = [computer("wait", ms=3000), ("restart", {})]
        elif count == 3:
            actions = [self.start_page_server(), *navigate()]
        elif count == 4:
            self.browsers.append(browser_isolation(self.organism))
            require(self.browsers[1]["start_ticks"] > self.browsers[0]["start_ticks"],
                    "reboot did not start a new Chromium process")
            self.check_screen(request, previous, "restored")
            # Change the screen after the seed's last screenshot. A stale model image
            # cannot satisfy the owner-facing live desktop check while the seed sleeps.
            actions = [computer("key", key="j"),
                       ("report", {"idempotency_key": "desktop-accepted", "body": "Browser profile restored."}),
                       ("sleep", {"seconds": 600})]
        elif count == 5:
            slept = json.loads((previous / "action-0002/result.json").read_bytes())
            require(slept.get("woke_for") == "creator_message" and slept["slept_seconds"] < 600,
                    "new message did not wake the sleeping seed")
            actions = [("reply", {"reply_to": self.creator_seq, "text": ANSWER})]
        elif count == 6:
            actions = [("write_file", {"path": "desktop-fixture/after-chat.txt",
                                       "content": "Autonomous work resumed."}),
                       ("sleep", {"seconds": 600})]
        else:
            raise ValueError("unexpected additional model request")
        self.previous_tools = actions
        output = [{"type": "function_call", "status": "completed", "call_id": f"desktop-{count}-{i}",
                   "name": name, "arguments": json.dumps(args)} for i, (name, args) in enumerate(actions)]
        print(f"Responses turn {count + 1}: {', '.join(item['name'] for item in output)}", flush=True)
        return {"id": f"resp_desktop_{count}", "object": "response", "model": MODEL, "status": "completed",
                "output": output, "usage": {"input_tokens": INPUT_TOKENS, "output_tokens": OUTPUT_TOKENS,
                                            "total_tokens": INPUT_TOKENS + OUTPUT_TOKENS}}

    def verify_live_desktop(self):
        url = "http://127.0.0.1:8088" + self.path + "/desktop"
        try:
            with HTTP.open(url, timeout=12) as res:
                raise ValueError(f"unauthenticated desktop returned HTTP {res.status}")
        except HTTPError as error:
            require(error.code == 401, f"expected 401 for unauthenticated desktop, got {error.code}")
        request = Request(url, headers={"Authorization": "Bearer " + os.environ["SURVIVE_CREATOR_TOKEN"]})
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            with HTTP.open(request, timeout=12) as res:
                if res.status == 200:
                    require(res.headers.get_content_type() == "image/jpeg", "live desktop returned wrong MIME type")
                    jpeg = res.read(1024 * 1024 + 1)
                    require(len(jpeg) <= 1024 * 1024, "live desktop frame exceeds 1 MiB")
                    (self.fixture / "live.jpg").write_bytes(jpeg)
                    target_center(jpeg, "restored")
                    target_center(jpeg, "live")
                    return hashlib.sha256(jpeg).hexdigest()
                require(res.status == 204, f"unexpected live desktop HTTP status {res.status}")
            time.sleep(0.25)
        raise TimeoutError("creator live desktop did not return the current guest frame")

    def send_message_while_asleep(self):
        before = api(self.path + "/ledger")
        self.live_hash = self.verify_live_desktop()
        after = api(self.path + "/ledger")
        require(all(before[k] == after[k] for k in ("used", "reserved", "balance", "reservations")),
                "creator desktop read changed model billing")
        row = api(self.path + "/messages", {"text": QUESTION, "idempotency_key": "desktop-creator-question"})["message"]
        require(row["text"] == QUESTION and row["role"] == "creator", "creator send failed")
        self.creator_seq = row["seq"]
        self.sent.set()

    def verify(self):
        organism = api(self.path)["organism"]
        require(organism["state"] == "running" and organism["generation"] == 2, "unexpected reboot generation")
        ledger = api(self.path + "/ledger")
        messages = api(self.path + "/messages")["messages"]
        require([(m["role"], m["text"], m["reply_to"]) for m in messages]
                == [("creator", QUESTION, None), ("organism", ANSWER, self.creator_seq)],
                "unexpected conversation history after seed reply")
        work = self.history.parents[1] / "desktop-fixture/after-chat.txt"
        require(work.read_text() == "Autonomous work resumed.", "seed did not resume its own work")
        entries = ledger["reservations"]
        require(len(entries) == 7 and ledger["used"] == COST * 7 and ledger["reserved"] == 0
                and ledger["balance"] == ENERGY - COST * 7, "creator ledger totals differ")
        for entry in entries:
            require(entry["status"] == "settled" and entry["energy_charged"] == COST and entry["model"] == MODEL
                    and entry["usage"] == {"input_tokens": INPUT_TOKENS, "output_tokens": OUTPUT_TOKENS},
                    "incorrect individual ledger settlement")
        events = api(self.path + "/events?limit=1000")["events"]
        require(sum(e["kind"] == "restart_requested" for e in events) == 1, "expected one explicit reboot")
        require(self.turns[-1]["remaining_lifetime_ms"] < self.turns[0]["remaining_lifetime_ms"],
                "running lifetime did not decrease")
        require(len(set(self.screens.values())) == 3, "expected three distinct observed frames")
        report = {"organism": self.organism, "generation": 2, "responses": self.turns,
                  "settled_energy": ledger["used"], "remaining_energy": ledger["balance"],
                  "screen_sha256": self.screens, "initial_screen_sha256": self.initial_hash,
                  "live_screen_sha256": self.live_hash,
                  "conversation": messages, "autonomous_work": work.read_text(),
                  "browser_processes": self.browsers, "successful_tools": self.tools_checked + 1}
        (self.fixture / "desktop-result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))


def main(fixture):
    scenario = Scenario(fixture)

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            try:
                require(self.path == "/v1/responses", "expected the Responses endpoint")
                size = int(self.headers["Content-Length"])
                require(0 < size <= 256 * 1024, "unbounded fixture request")
                response = scenario.reply(json.loads(self.rfile.read(size)))
                status = 200
            except Exception:
                scenario.error = traceback.format_exc()
                status, response = 400, {"error": {"message": "desktop acceptance failed"}}
            body = json.dumps(response).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    with HTTPServer(("127.0.0.1", 18090), Handler) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        startup = time.monotonic() + 60
        while time.monotonic() < startup:
            try:
                ready = api("/readyz")
                if ready["runtime"]["available"] and ready["runtime"]["backend"] == "youki" and not ready["test_mode"]:
                    break
            except URLError:
                pass
            time.sleep(0.5)
        else:
            raise RuntimeError("real youki server failed readiness; see server.log")
        scenario.create()
        deadline = time.monotonic() + 330
        while time.monotonic() < deadline:
            require(not scenario.error, scenario.error)
            organism = api(scenario.path)["organism"]
            require(organism["state"] not in ("failed", "depleted"),
                    f"organism stopped: {organism['state']} {organism['last_error']}")
            if len(scenario.turns) == 5 and not scenario.sent.is_set():
                current = scenario.history / f"{scenario.turns[-1]['iteration']:020d}/state.json"
                actions = json.loads(current.read_bytes())["actions"]
                if len(actions) == 3 and actions[2]["status"] == "running":
                    for action in actions[:2]:
                        require(action["status"] == "done" and action["result"]["ok"],
                                f"final {action['name']} action failed")
                    reports = api(scenario.path + "/reports")["reports"]
                    require(len(reports) == 1 and reports[0]["body"] == "Browser profile restored.", "missing report")
                    scenario.send_message_while_asleep()
            elif len(scenario.turns) == 7:
                current = scenario.history / f"{scenario.turns[-1]['iteration']:020d}/state.json"
                actions = json.loads(current.read_bytes())["actions"]
                if len(actions) == 2 and actions[1]["status"] == "running":
                    require(actions[0]["status"] == "done" and actions[0]["result"]["ok"],
                            "autonomous work after chat failed")
                    scenario.verify()
                    return
            time.sleep(0.3)
        raise TimeoutError(f"desktop scenario timed out after {len(scenario.turns)} model turns; see {fixture}")


def verify_stopped_frame(fixture):
    path = "/api/v2/organisms/" + (fixture / "organism-id").read_text().strip()
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        if api(path)["organism"]["state"] != "running":
            break
        time.sleep(0.3)
    else:
        raise TimeoutError("platform still reports a running organism after fixture shutdown")
    frame = snapshot(path)
    require(frame == (fixture / "live.jpg").read_bytes(), "stopped snapshot differs from the last live frame")
    print("Authenticated stopped snapshot matched the last live desktop frame.", flush=True)


if __name__ == "__main__":
    if sys.argv[2:] == ["--verify-stopped-frame"]:
        verify_stopped_frame(Path(sys.argv[1]))
    else:
        main(Path(sys.argv[1]))

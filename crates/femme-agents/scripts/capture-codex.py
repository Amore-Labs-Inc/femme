#!/usr/bin/env python3
"""Capture golden fixtures for the Codex app-server driver (`crates/femme-agents/src/codex.rs`).

Usage: capture-codex.py [--scratch DIR] [CASE ...]   (no CASE = every case in CASES order)

Every case spawns the installed `codex app-server` (+ the driver's FLAGS) under `timeout 120` in the
scratch repo, speaks its JSON-RPC over pipes *the way `CodexDriver` does* (same requests, same ids,
written at the same point of the stream: the `Run` class below mirrors the driver's state machine)
and writes into `fixtures/codex/`:

  <case>.jsonl        every raw stdout line of the app-server (scrubbed)
  <case>.stdin.jsonl  every line written to its stdin (scrubbed)
  <case>.calls.json   driver calls and stream positions, replayed by tests/golden_codex.rs:
                      {"argv": [...], "extra_args": [...], "launch": {...}, "exit": code|null,
                       "calls": [{"at": n, "call": "on_start|prompt|interrupt|answer|read_rate_limits", ...}],
                       "stdin_at": [n, ...]}   # `at` = stdout lines the driver had seen
The case then ends cleanly: stdin closed, process group killed after 5 s if still running.
`schema` regenerates fixtures/codex/schema.json (`codex app-server generate-json-schema`, minified).

Model: MODEL (the cheapest entry of `model/list` on the capture host) with low reasoning effort
(`-c model_reasoning_effort="low"` is capture-only, see extra_args). `resume` continues the thread
of `ok`, so run `ok` first. Stdlib only; stderr of each run goes to <scratch>/logs/.
"""
import argparse
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.normpath(os.path.join(HERE, "..", "fixtures", "codex"))
DEFAULT_SCRATCH = os.environ.get("FEMME_CODEX_SCRATCH", os.path.join(tempfile.gettempdir(), "femme-codex-capture"))
MODEL = "gpt-6-luna"
VERSION = "0.1.0"  # femme-agents CARGO_PKG_VERSION (the golden test normalises it)
# = codex::FLAGS
FLAGS = ["-c", 'approval_policy="never"', "-c", 'sandbox_mode="danger-full-access"',
         "--enable", "default_mode_request_user_input", "-c", "suppress_unstable_features_warning=true"]
CAPTURE_ARGS = ["-c", 'model_reasoning_effort="low"']
# = codex::OPT_OUT
OPT_OUT = ["turn/moderationMetadata", "turn/diff/updated", "thread/readState/changed",
           "mcpServer/startupStatus/updated", "remoteControl/status/changed", "account/updated",
           "item/commandExecution/outputDelta", "item/fileChange/outputDelta",
           "item/fileChange/patchUpdated", "hook/started", "hook/completed"]
SCRATCH = DEFAULT_SCRATCH
RUNS = [0]


def scrub(s):
    """Paths, account/installation ids, credit ids, e-mail addresses."""
    s = s.replace(SCRATCH, "/work/repo").replace(os.path.expanduser("~"), "/home/user")
    s = re.sub(r"/tmp/claude-\d+/[^\"\s]*", "/work/tmp", s)
    # the userAgent names the capturing terminal (`... x86_64) <TERM_PROGRAM>/<version> (femme; 0.1.0)`)
    s = re.sub(r'("userAgent":"[^"]*\)) \S+ (\(femme;)', r"\1 xterm-color \2", s)
    s = re.sub(r'"(accountId|installationId|userId|organizationId|email)":"[^"]*"', r'"\1":"redacted"', s)
    s = re.sub(r"RateLimitResetCredit_[A-Za-z0-9_-]+", "RateLimitResetCredit_redacted", s)
    return re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}", "user@example.com", s)


class Run:
    """One `codex app-server` process, driven exactly like `CodexDriver` drives it."""

    def __init__(self, case, resume=None):
        self.case = case
        self.argv = ["codex", "app-server", *FLAGS]
        self.launch = {"cwd": SCRATCH, "model": MODEL, "resume": resume}
        RUNS[0] += 1
        self.p = subprocess.Popen(["timeout", "120", *self.argv, *CAPTURE_ARGS], cwd=SCRATCH,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  start_new_session=True)
        print(f"[{case}] pid {self.p.pid}", file=sys.stderr)
        self.q = queue.Queue()
        self.err = []
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=lambda: self.err.extend(self.p.stderr), daemon=True).start()
        self.lines, self.sent, self.stdin_at, self.calls = [], [], [], []
        self.next_id, self.pending = 1, {}
        self.thread = self.turn = self.last_turn = None
        self.running = self.inflight = self.handshake = self.inited = False
        self.want_limits = self.interrupt_wanted = self.closed = False
        self.queue, self.asks = [], []

    def _read(self):
        for line in self.p.stdout:
            self.q.put(line.rstrip(b"\n"))
        self.q.put(None)

    # ---- what the driver writes ----
    def write(self, obj):
        if self.closed:
            raise RuntimeError(f"{self.case}: the driver would write after stdin was closed: {obj}")
        line = json.dumps(obj, separators=(",", ":"))
        self.sent.append(line)
        self.stdin_at.append(len(self.lines))
        self.p.stdin.write(line.encode() + b"\n")
        self.p.stdin.flush()

    def send(self, method, params, kind):
        obj = {"id": self.next_id, "method": method}
        if params is not None:
            obj["params"] = params
        self.pending[self.next_id] = kind
        self.next_id += 1
        self.write(obj)

    def busy(self):
        return self.running or self.inflight or self.turn is not None

    def start_turn(self, text):
        self.inflight = True
        self.send("turn/start", {"threadId": self.thread, "input": [{"type": "text", "text": text}]}, ("turn",))

    def flush(self):
        if not self.handshake and not self.busy() and self.thread and self.queue:
            self.start_turn(self.queue.pop(0))

    def interrupt_now(self):
        if self.thread and self.turn:
            self.send("turn/interrupt", {"threadId": self.thread, "turnId": self.turn}, ("interrupt",))
            return True
        return False

    def learn_turn(self, turn):
        if turn != self.last_turn:
            self.turn = turn
            if self.interrupt_wanted:
                self.interrupt_wanted = False
                self.interrupt_now()

    # ---- driver calls (recorded) ----
    def call(self, name, **kw):
        self.calls.append({"at": len(self.lines), "call": name, **kw})

    def on_start(self):
        self.call("on_start")
        self.handshake = True
        self.send("initialize", {"clientInfo": {"name": "femme", "title": "FEMME", "version": VERSION},
                                 "capabilities": {"optOutNotificationMethods": OPT_OUT}}, ("init",))

    def prompt(self, text, how="Queue"):
        self.call("prompt", text=text, how=how)
        ready = not self.handshake and self.thread
        if how == "Steer" and ready and self.running and self.turn:
            self.send("turn/steer", {"threadId": self.thread, "input": [{"type": "text", "text": text}],
                                     "expectedTurnId": self.turn}, ("steer", text))
            return
        if how == "Steer" and ready and self.inflight:
            self.start_turn(text)
            return
        if how == "InterruptThenSend" and self.busy():
            if not self.interrupt_now():
                self.interrupt_wanted = True
            self.queue.insert(0, text)
        else:
            self.queue.append(text)
        self.flush()

    def interrupt(self):
        self.call("interrupt")
        if not self.interrupt_now():
            self.interrupt_wanted = True

    def answer_option(self, request, i):
        """Answer a single-question requestUserInput with option `i` (all the captures need)."""
        p = request["params"]
        (q,) = p["questions"]
        self.call("answer", request_id=p["itemId"], answer={"Option": i})
        self.write({"id": request["id"], "result": {"answers": {q["id"]: {"answers": [q["options"][i]["label"]]}}}})

    def read_rate_limits(self):
        self.call("read_rate_limits")
        if self.handshake and not self.inited:
            self.want_limits = True
        else:
            self.send("account/rateLimits/read", None, ("limits",))

    # ---- stdout ----
    def handle(self, m):
        method, rid = m.get("method"), m.get("id")
        if method is None and rid is not None:
            kind = self.pending.pop(rid, None) if isinstance(rid, int) else None
            res = m.get("result") or {}
            if "error" in m:
                if kind and kind[0] == "turn":
                    self.inflight = False
                    self.flush()
                elif kind and kind[0] == "steer":
                    self.queue.insert(0, kind[1])
                    self.flush()
                return
            if kind and kind[0] == "init":
                self.inited = True
                self.write({"method": "initialized"})
                params = {"cwd": self.launch["cwd"], "approvalPolicy": "never", "sandbox": "danger-full-access",
                          "model": self.launch["model"]}
                if self.launch["resume"]:
                    params["threadId"] = self.launch["resume"]
                    self.send("thread/resume", params, ("thread",))
                else:
                    self.send("thread/start", params, ("thread",))
                if self.want_limits:
                    self.want_limits = False
                    self.send("account/rateLimits/read", None, ("limits",))
            elif kind and kind[0] == "turn":
                self.inflight = False
                if (res.get("turn") or {}).get("id"):
                    self.learn_turn(res["turn"]["id"])
            elif (res.get("thread") or {}).get("id"):
                self.thread = res["thread"]["id"]
                self.handshake = False
                self.flush()
            return
        params = m.get("params") or {}
        if params.get("threadId") not in (None, self.thread) and self.thread:
            return
        if method == "turn/started":
            self.running, self.inflight, self.last_turn = True, False, None
            self.learn_turn(params["turn"]["id"])
        elif method == "turn/completed":
            self.last_turn = params["turn"]["id"] or self.turn
            self.turn, self.running, self.interrupt_wanted = None, False, False
            self.flush()

    def pump(self, until=None, timeout=60):
        """Read lines (driving the state machine) until `until(msg)` is true; returns that msg."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                raw = self.q.get(timeout=max(0.05, end - time.time()))
            except queue.Empty:
                continue
            if raw is None:
                return None
            self.lines.append(raw)
            try:
                m = json.loads(raw)
            except ValueError:
                continue
            if isinstance(m, dict):
                self.handle(m)
                if until and until(m):
                    return m
        print(f"[{self.case}] pump timed out", file=sys.stderr)
        return None

    def finish(self):
        """Close stdin, end the process (group), save the fixtures."""
        self.closed = True
        self.p.stdin.close()
        t = time.time()
        while self.p.poll() is None and time.time() - t < 5:
            time.sleep(0.1)
        if self.p.poll() is None:
            os.killpg(self.p.pid, signal.SIGKILL)
            self.p.wait()
        while True:
            try:
                raw = self.q.get(timeout=5)
            except queue.Empty:
                break
            if raw is None:
                break
            self.lines.append(raw)
            try:
                m = json.loads(raw)
            except ValueError:
                continue
            if isinstance(m, dict):
                self.handle(m)  # raises if the driver would have written
        code = self.p.returncode
        print(f"[{self.case}] exit {code}, {len(self.lines)} lines", file=sys.stderr)
        os.makedirs(os.path.join(SCRATCH, "logs"), exist_ok=True)
        with open(os.path.join(SCRATCH, "logs", self.case + ".stderr.txt"), "wb") as f:
            f.write(b"".join(self.err))
        with open(os.path.join(FIXTURES, self.case + ".jsonl"), "w") as f:
            f.writelines(scrub(raw.decode(errors="replace")) + "\n" for raw in self.lines)
        with open(os.path.join(FIXTURES, self.case + ".stdin.jsonl"), "w") as f:
            f.writelines(scrub(line) + "\n" for line in self.sent)
        calls = {"argv": self.argv, "extra_args": CAPTURE_ARGS, "launch": self.launch, "exit": code,
                 "calls": self.calls, "stdin_at": self.stdin_at}
        with open(os.path.join(FIXTURES, self.case + ".calls.json"), "w") as f:
            f.write(scrub(json.dumps(calls, indent=1)) + "\n")


def method_is(name, **item):
    """Predicate: notification `name` (with `params.item` fields equal to `item`)."""
    def pred(m):
        it = (m.get("params") or {}).get("item") or {}
        return m.get("method") == name and all(it.get(k) == v for k, v in item.items())
    return pred


def turn_completed(m):
    return m.get("method") == "turn/completed"


def simple(case, text):
    r = Run(case)
    r.on_start()
    r.prompt(text)
    r.pump(turn_completed)
    r.finish()


def case_ok():
    simple("ok", "Reply with the single word ok.")


def case_tool():
    try:
        os.remove(os.path.join(SCRATCH, "hello.txt"))
    except FileNotFoundError:
        pass
    simple("tool", "Run `echo hi` with your shell tool, then use apply_patch to create a file named hello.txt "
                   "containing the word hi, then reply with the single word done.")


def case_question():
    r = Run("question")
    r.on_start()
    r.prompt("Before doing anything else, use your request_user_input tool to ask me one multiple-choice question: "
             "which color do I prefer, with the options red and blue. After I answer, reply with only the color I picked.")
    req = r.pump(lambda m: m.get("method") == "item/tool/requestUserInput")
    if req:
        r.answer_option(req, 0)
        r.pump(turn_completed)
    r.finish()


def case_interrupt():
    r = Run("interrupt")
    r.on_start()
    r.prompt("Run `sleep 6; echo hi` with your shell tool, then reply with the single word ok.")
    started = r.pump(method_is("item/started", type="commandExecution"))
    if started:
        r.interrupt()
        r.pump(turn_completed)
        # the interrupted command's item/completed arrives after turn/completed
        r.pump(method_is("item/completed", id=started["params"]["item"]["id"]), timeout=15)
    r.finish()


def case_steer():
    r = Run("steer")
    r.on_start()
    r.prompt("Run `sleep 4; echo hi` with your shell tool, then reply with the single word ok.")
    if r.pump(method_is("item/started", type="commandExecution")):
        r.prompt("After the command, reply with the word blue instead of ok.", "Steer")
        r.pump(turn_completed)
    r.finish()


def case_queue():
    r = Run("queue")
    r.on_start()
    r.prompt("Run `sleep 4; echo hi` with your shell tool, then reply with the single word ok.")
    if r.pump(method_is("item/started", type="commandExecution")):
        r.prompt("Reply with the single word second.")
        r.pump(turn_completed)
        r.pump(turn_completed)
    r.finish()


def case_resume():
    with open(os.path.join(FIXTURES, "ok.jsonl")) as f:
        thread = next(m["result"]["thread"]["id"] for m in map(json.loads, f)
                      if isinstance(m.get("result"), dict) and "thread" in m["result"])
    r = Run("resume", resume=thread)
    r.on_start()
    r.prompt("What single word did you reply with in your previous message? Reply with just that word.")
    r.pump(turn_completed)
    r.finish()


def case_rate_limits():
    r = Run("rate_limits")
    r.on_start()
    r.read_rate_limits()
    r.pump(lambda m: "rateLimits" in (m.get("result") or {}))
    r.finish()


def case_schema():
    with tempfile.TemporaryDirectory() as d:
        subprocess.run(["timeout", "120", "codex", "app-server", "generate-json-schema", "--out", d], check=True)
        with open(os.path.join(d, "codex_app_server_protocol.schemas.json")) as f:
            schema = json.load(f)
    version = subprocess.run(["codex", "--version"], capture_output=True, text=True).stdout.strip()
    schema["description"] = (f"`codex app-server generate-json-schema` ({version}, stable API), minified by "
                             "scripts/capture-codex.py. Methods: definitions.{Client,Server}{Request,Notification}"
                             ".oneOf[].properties.method.enum")
    with open(os.path.join(FIXTURES, "schema.json"), "w") as f:
        json.dump(schema, f, separators=(",", ":"), sort_keys=True)


CASES = {"schema": case_schema, "ok": case_ok, "tool": case_tool, "question": case_question,
         "interrupt": case_interrupt, "steer": case_steer, "queue": case_queue, "resume": case_resume,
         "rate_limits": case_rate_limits}


def main():
    global SCRATCH
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--scratch", default=DEFAULT_SCRATCH, help="git repo the agent runs in")
    ap.add_argument("cases", nargs="*", help=f"cases to capture (default: all): {' '.join(CASES)}")
    a = ap.parse_args()
    if unknown := [c for c in a.cases if c not in CASES]:
        ap.error(f"unknown case(s): {unknown}")
    SCRATCH = os.path.abspath(a.scratch)
    if not os.path.isdir(os.path.join(SCRATCH, ".git")):  # first run: make the repo the agent works in
        os.makedirs(SCRATCH, exist_ok=True)
        subprocess.run(["git", "init", "-q", SCRATCH], check=True)
    if not shutil.which("codex"):
        sys.exit("codex is not on PATH")
    os.makedirs(FIXTURES, exist_ok=True)
    for name in a.cases or CASES:
        CASES[name]()
    print(f"{RUNS[0]} app-server runs", file=sys.stderr)


if __name__ == "__main__":
    main()

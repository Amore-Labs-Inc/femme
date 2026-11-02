#!/usr/bin/env python3
"""Capture golden fixtures for the Pi RPC driver (`crates/femme-agents/src/pi.rs`).

Usage: capture-pi.py [--scratch DIR] [--out DIR] [--real] [CASE ...]   (no CASE = all cases)
       capture-pi.py --serve    only the scripted model, for tests/live_pi.rs (prints the env to use)

Every case spawns the installed `pi --mode rpc` under `timeout 120`, speaks its JSON-lines RPC over
pipes *the way `PiDriver` does* (same commands, same `f<n>` ids, written at the same point of the
stream) and writes, per case, into `fixtures/pi/`:

  <case>.jsonl        every raw stdout line of pi (scrubbed: paths, port)
  <case>.stdin.jsonl  every line written to pi's stdin
  <case>.calls.json   the driver calls and stream positions, so tests/golden_pi.rs can replay the
                      same conversation through `PiDriver` and compare what it writes:
                      {"argv": [...], "extra_args": [...], "launch": {...}, "exit": code|null,
                       "calls": [{"at": n, "call": "on_start|prompt|interrupt|set_model|answer|raw", ...}],
                       "stdin_at": [n, ...]}   # `at` = stdout lines the driver had seen
Then the case ends cleanly: stdin closed, killed after 5 s if still running.

The model. Pi had no provider credentials on the capture host (`pi --list-models`: "No models
available"), so by default pi talks to a scripted OpenAI-compatible chat-completions endpoint served
by this script (provider `mock`, models `mock-a`/`mock-b`, configured through an isolated
`PI_CODING_AGENT_DIR` with a `models.json`). Pi itself (RPC layer, agent loop, tools, sessions,
extensions) is the real installed binary; only the LLM is scripted, which also makes the fixtures
deterministic. `--real` drops the mock and uses pi's own configured default model (costs tokens; the
`question` case needs the mock's `ask_choice` tool call and is skipped).

Stdlib only. Logs (stderr of pi, requests the mock got) go to <scratch>/logs/.
"""
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.normpath(os.path.join(HERE, "..", "fixtures", "pi"))
DEFAULT_SCRATCH = os.environ.get("FEMME_PI_SCRATCH", os.path.join(tempfile.gettempdir(), "femme-pi-capture"))
PORT_PLACEHOLDER = "47123"
STARTED = []  # Popen objects we started, for the final cleanup
RUNS = [0]  # pi processes started (the run budget)

# --------------------------------------------------------------------------------------------
# Scripted OpenAI-compatible model
# --------------------------------------------------------------------------------------------


def _text(m):
    c = m.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def script_for(req):
    """The reply for a chat-completions request, as a list of steps. Deterministic."""
    msgs = req.get("messages") or []
    last = msgs[-1] if msgs else {}
    users = [_text(m) for m in msgs if m.get("role") == "user"]
    last_user = (users[-1] if users else "").lower()
    if last.get("role") == "tool":
        out = _text(last).strip()
        if "pick a color" in last_user:
            return [("text", "You picked %s." % out)]
        return [("text", "done")]
    t = last_user
    m = re.search(r"count slowly from 1 to (\d+)", t)
    if "steered" in t:
        return [("text", "STEERED")]
    if "followup" in t:
        return [("text", "FOLLOWUP")]
    if m:
        steps = []
        for i in range(1, int(m.group(1)) + 1):
            steps += [("text", "%d\n" % i), ("sleep", 0.2)]
        return steps
    if "echo hi" in t:
        return [("text", "Running it."), ("tool", "call_echo_1", "bash", '{"command":"echo hi"}')]
    if "pick a color" in t:
        return [("tool", "call_ask_1", "ask_choice", '{"question":"Pick a color","options":["red","blue"]}')]
    if "what word" in t:
        for u in users:
            w = re.search(r"remember the word (\w+)", u.lower())
            if w:
                return [("text", w.group(1))]
        return [("text", "I do not know.")]
    if "single word ok" in t or "remember the word" in t:
        return [("reasoning", "The user wants "), ("reasoning", "the word ok."), ("text", "ok")]
    return [("text", "ok")]


class Mock:
    def __init__(self, logdir):
        self.n = 0
        self.lock = threading.Lock()
        self.log = open(os.path.join(logdir, "mock-requests.jsonl"), "a")
        mock = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                self.send_response(404)
                self.end_headers()

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length") or 0))
                try:
                    req = json.loads(body)
                except Exception:
                    req = {}
                mock.serve(self, req)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def serve(self, h, req):
        with self.lock:
            self.n += 1
            n = self.n
            self.log.write(json.dumps({"n": n, "path": h.path, "req": req}) + "\n")
            self.log.flush()
        steps = script_for(req)
        model = req.get("model", "mock")
        rid = "chatcmpl-mock-%d" % n
        h.send_response(200)
        h.send_header("content-type", "text/event-stream")
        h.send_header("cache-control", "no-cache")
        h.end_headers()

        def chunk(delta=None, finish=None, usage=None, choices=True):
            c = {"id": rid, "object": "chat.completion.chunk", "created": 1700000000, "model": model}
            c["choices"] = [{"index": 0, "delta": delta or {}, "finish_reason": finish}] if choices else []
            if usage:
                c["usage"] = usage
            h.wfile.write(b"data: " + json.dumps(c).encode() + b"\n\n")
            h.wfile.flush()

        try:
            chunk({"role": "assistant", "content": ""})
            out_chars = 0
            tool = False
            for st in steps:
                if st[0] == "sleep":
                    time.sleep(st[1])
                elif st[0] == "text":
                    chunk({"content": st[1]})
                    out_chars += len(st[1])
                elif st[0] == "reasoning":
                    chunk({"reasoning_content": st[1]})
                    out_chars += len(st[1])
                elif st[0] == "tool":
                    _, cid, name, args = st
                    tool = True
                    chunk({"tool_calls": [{"index": 0, "id": cid, "type": "function", "function": {"name": name, "arguments": ""}}]})
                    half = len(args) // 2
                    for part in (args[:half], args[half:]):
                        chunk({"tool_calls": [{"index": 0, "function": {"arguments": part}}]})
                    out_chars += len(args)
            chunk(finish="tool_calls" if tool else "stop")
            msgs = len(req.get("messages") or [])
            usage = {
                "prompt_tokens": 1000 + 10 * msgs,
                "completion_tokens": max(1, out_chars // 4),
                "total_tokens": 1000 + 10 * msgs + max(1, out_chars // 4),
                "prompt_tokens_details": {"cached_tokens": 800 if msgs > 2 else 0},
            }
            chunk(usage=usage, choices=False)
            h.wfile.write(b"data: [DONE]\n\n")
            h.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # pi aborted the request


def write_models_json(agent_dir, port):
    os.makedirs(agent_dir, exist_ok=True)
    model = lambda mid: {
        "id": mid,
        "name": "Mock " + mid[-1].upper(),
        "reasoning": True,
        "input": ["text"],
        "contextWindow": 128000,
        "maxTokens": 4096,
        "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0},
    }
    cfg = {"providers": {"mock": {"baseUrl": "http://127.0.0.1:%d/v1" % port, "api": "openai-completions",
                                  "apiKey": "mock", "models": [model("mock-a"), model("mock-b")]}}}
    with open(os.path.join(agent_dir, "models.json"), "w") as f:
        json.dump(cfg, f, indent=2)


ASK_EXTENSION = r'''
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

// Test extension for capture-pi.py: a tool that asks the user through ctx.ui (RPC: extension_ui_request).
export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "ask_choice",
		label: "Ask choice",
		description: "Ask the user to pick one of several options. Returns the picked option.",
		parameters: Type.Object({ question: Type.String(), options: Type.Array(Type.String()) }),
		async execute(_id, params, _signal, _onUpdate, ctx) {
			ctx.ui.setStatus("femme-test", "asking");
			const choice = await ctx.ui.select(params.question, params.options);
			ctx.ui.notify(`picked ${choice ?? "nothing"}`, "info");
			ctx.ui.setStatus("femme-test", undefined);
			return { content: [{ type: "text", text: choice ?? "(cancelled)" }], details: {} };
		},
	});
}
'''

# --------------------------------------------------------------------------------------------
# A pi process plus a tiny emulation of PiDriver's stdin decisions
# --------------------------------------------------------------------------------------------


class Pi:
    """One `pi --mode rpc` process, driven like `PiDriver` drives it.

    `processed` = stdout lines handled so far, in order (the driver's view). Every write records the
    position it belongs to: user calls happen after `processed` lines, reactions to a line (a queued
    prompt flushed at `agent_settled`) after that line.
    """

    def __init__(self, ctx, case, launch, extra_args=()):
        self.ctx, self.case, self.launch = ctx, case, dict(launch)
        argv = ["pi", "--mode", "rpc"]
        if launch.get("model"):
            argv += ["--model", launch["model"]]
        if launch.get("resume"):
            argv += ["--session-id", launch["resume"]]
        self.argv = argv
        self.extra_args = list(extra_args)
        self.lines = []  # raw bytes without \n
        self.cond = threading.Condition()
        self.eof = False
        self.processed = 0
        self.sent = []  # (at, line)
        self.calls = []
        self.n = 0  # command id counter, like PiDriver
        self.in_turn = False  # between agent_start and agent_settled
        self.announced = False  # pi answered "started" and that run has not settled
        self.pending = {}  # prompt id -> text, not answered yet
        self.queue = []  # prompts held by the driver
        self.exit = None
        self.agent_sid = None
        self.stderr = []
        RUNS[0] += 1
        self.p = subprocess.Popen(["timeout", "120"] + argv + self.extra_args, cwd=ctx.cwd, env=ctx.env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  start_new_session=True)
        STARTED.append(self.p)
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._err, daemon=True).start()
        self.call("on_start")
        self.cmd({"type": "get_state"})

    # -- io ---------------------------------------------------------------------------------
    def _read(self):
        for raw in iter(self.p.stdout.readline, b""):
            with self.cond:
                self.lines.append(raw[:-1] if raw.endswith(b"\n") else raw)
                self.cond.notify_all()
        with self.cond:
            self.eof = True
            self.cond.notify_all()

    def _err(self):
        for raw in iter(self.p.stderr.readline, b""):
            self.stderr.append(raw.decode("utf-8", "replace"))

    def write(self, obj, at):
        line = json.dumps(obj, separators=(",", ":"), ensure_ascii=False)
        self.sent.append((at, line))
        try:
            self.p.stdin.write((line + "\n").encode())
            self.p.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass

    def cmd(self, obj, at=None):
        self.n += 1
        o = {"id": "f%d" % self.n}
        o.update(obj)
        self.write(o, self.processed if at is None else at)
        return o["id"]

    def call(self, name, **kw):
        c = {"at": self.processed, "call": name}
        c.update(kw)
        self.calls.append(c)

    # -- the driver's reaction to one stdout line (mirrors PiDriver) --------------------------
    def busy(self):
        return self.in_turn or self.announced or bool(self.pending)

    def _handle(self, idx, m):
        t = m.get("type")
        if t == "response" and m.get("command") == "get_state" and m.get("success"):
            self.agent_sid = self.agent_sid or (m.get("data") or {}).get("sessionId")
        if t == "response" and m.get("command") == "prompt":
            text = self.pending.pop(m.get("id"), None)
            if m.get("success"):
                if (m.get("data") or {}).get("disposition") == "started":
                    self.announced = True
            elif self.in_turn and text is not None:
                self.queue.insert(0, text)  # refused because a run we had not seen was going
            self._flush(idx + 1)
        if t == "agent_start":
            self.in_turn = True
        if t == "agent_settled":
            self.in_turn = self.announced = False
            self._flush(idx + 1)

    def _send_prompt(self, text, steer=False, at=None):
        o = {"type": "prompt", "message": text}
        if steer:
            o["streamingBehavior"] = "steer"
        self.pending[self.cmd(o, at)] = text

    def _flush(self, at):
        if self.queue and not self.busy():
            self._send_prompt(self.queue.pop(0), at=at)

    def pump(self, pred=None, timeout=60, quiet=None):
        """Handle lines in order until pred(msg) (returns msg), EOF (None) or timeout (raises).
        With `quiet`, also return None once no line arrived for `quiet` seconds."""
        end = time.time() + timeout
        last = time.time()
        while True:
            with self.cond:
                while self.processed >= len(self.lines) and not self.eof:
                    left = end - time.time()
                    if quiet is not None:
                        left = min(left, last + quiet - time.time())
                    if left <= 0:
                        break
                    self.cond.wait(left)
                if self.processed < len(self.lines):
                    raw = self.lines[self.processed]
                else:
                    raw = None
            if raw is None:
                if self.eof:
                    return None
                if quiet is not None and time.time() >= last + quiet:
                    return None
                raise TimeoutError("%s: timed out waiting (%d lines)" % (self.case, self.processed))
            idx = self.processed
            self.processed += 1
            last = time.time()
            try:
                m = json.loads(raw)
            except ValueError:
                continue
            if not isinstance(m, dict):
                continue
            self._handle(idx, m)
            if pred and pred(m):
                return m

    def drain(self):
        """Handle everything that has arrived so far (what the daemon would have done by now)."""
        self.pump(quiet=0.05, timeout=30)

    # -- driver calls -----------------------------------------------------------------------
    def prompt(self, text, how="Queue"):
        self.drain()
        self.call("prompt", text=text, how=how)
        if not self.busy():
            self._send_prompt(text)
        elif how == "Steer" and self.in_turn:
            self._send_prompt(text, steer=True)
        elif how == "InterruptThenSend":
            self.queue.insert(0, text)
            self.cmd({"type": "clear_queue"})
            self.cmd({"type": "abort"})
        else:
            self.queue.append(text)

    def interrupt(self):
        self.drain()
        self.call("interrupt")
        self.cmd({"type": "clear_queue"})
        self.cmd({"type": "abort"})

    def set_model(self, model):
        self.drain()
        self.call("set_model", model=model)
        provider, mid = model.split("/", 1)
        self.cmd({"type": "set_model", "provider": provider, "modelId": mid})

    def answer(self, request_id, answer, value_obj):
        self.drain()
        self.call("answer", request_id=request_id, answer=answer)
        o = {"type": "extension_ui_response", "id": request_id}
        o.update(value_obj)
        self.write(o, self.processed)

    def raw(self, obj):
        """A command PiDriver never sends (documents pi's own behaviour); the test skips it."""
        self.drain()
        self.call("raw", line=obj)
        self.write(obj, self.processed)

    # -- waiting helpers --------------------------------------------------------------------
    def settle(self, timeout=60):
        return self.pump(lambda m: m.get("type") == "agent_settled", timeout)

    def until_type(self, typ, timeout=60, **match):
        def p(m):
            if m.get("type") != typ:
                return False
            return all(m.get(k) == v for k, v in match.items())
        return self.pump(p, timeout)

    def streaming(self, timeout=60):
        return self.pump(lambda m: m.get("type") == "message_update"
                         and (m.get("assistantMessageEvent") or {}).get("type") == "text_delta", timeout)

    def child_pid(self):
        """pid of pi itself (our Popen is `timeout`)."""
        try:
            with open("/proc/%d/task/%d/children" % (self.p.pid, self.p.pid)) as f:
                kids = f.read().split()
            return int(kids[0]) if kids else None
        except OSError:
            return None

    def kill9(self):
        pid = self.child_pid()
        if pid:
            os.kill(pid, signal.SIGKILL)
        self.pump(timeout=10)  # to EOF
        self.p.wait(timeout=10)
        self.exit = None  # signalled: the driver sees on_exit(None)

    def finish(self):
        """Close stdin (pi's end of input), wait 5 s, else kill; handle the rest of stdout."""
        try:
            self.p.stdin.close()
        except OSError:
            pass
        try:
            rc = self.p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(self.p.pid, signal.SIGKILL)
            rc = self.p.wait(timeout=5)
        self.pump(timeout=10)
        self.exit = rc if rc >= 0 else None
        return self

    # -- output -----------------------------------------------------------------------------
    def save(self):
        ctx = self.ctx
        os.makedirs(ctx.out, exist_ok=True)
        base = os.path.join(ctx.out, self.case)
        with open(base + ".jsonl", "wb") as f:
            for raw in self.lines:
                f.write(ctx.scrub(raw.decode("utf-8", "replace")).encode() + b"\n")
        with open(base + ".stdin.jsonl", "w") as f:
            for _, line in self.sent:
                f.write(ctx.scrub(line) + "\n")
        calls = {
            "argv": self.argv,
            "extra_args": [ctx.scrub(a) for a in self.extra_args],
            "launch": self.launch,
            "exit": self.exit,
            "calls": json.loads(ctx.scrub(json.dumps(self.calls))),
            "stdin_at": [at for at, _ in self.sent],
        }
        with open(base + ".calls.json", "w") as f:
            json.dump(calls, f, indent=1)
            f.write("\n")
        with open(os.path.join(ctx.logs, self.case + ".stderr"), "w") as f:
            f.write("".join(self.stderr))
        print("   %-14s %4d lines, %2d sent, exit=%s" % (self.case, len(self.lines), len(self.sent), self.exit), flush=True)
        return self


# --------------------------------------------------------------------------------------------
# Cases
# --------------------------------------------------------------------------------------------

CASES = {}


def case(fn):
    CASES[fn.__name__[len("case_"):]] = fn
    return fn


OK = "Reply with the single word ok."


@case
def case_ok(ctx):
    pi = ctx.pi("ok")
    pi.prompt(OK)
    pi.settle()
    pi.finish().save()


@case
def case_tool(ctx):
    pi = ctx.pi("tool")
    pi.prompt("Run `echo hi` with your shell tool and then say done.")
    pi.settle()
    pi.finish().save()


@case
def case_interrupt(ctx):
    """Abort mid-stream, then show the session takes the next prompt."""
    pi = ctx.pi("interrupt")
    pi.prompt("Count slowly from 1 to 50.")
    pi.streaming()
    time.sleep(2)
    pi.interrupt()
    pi.settle()
    pi.prompt(OK)
    pi.settle()
    pi.finish().save()


@case
def case_interrupt_send(ctx):
    """PromptHow::InterruptThenSend with a steer still undelivered: clear_queue hands the steer back,
    abort stops the run, the prompt goes out when it settles."""
    pi = ctx.pi("interrupt_send")
    pi.prompt("Count slowly from 1 to 50.")
    pi.streaming()
    time.sleep(1)
    pi.prompt("Stop counting and say STEERED.", how="Steer")
    time.sleep(0.5)
    pi.prompt(OK, how="InterruptThenSend")
    pi.settle()  # the aborted run
    pi.settle()  # the new prompt's run
    pi.finish().save()


@case
def case_steer(ctx):
    """PromptHow::Steer (prompt + streamingBehavior "steer") while the assistant streams text."""
    pi = ctx.pi("steer")
    pi.prompt("Count slowly from 1 to 12.")
    pi.streaming()
    time.sleep(1)
    pi.prompt("Stop counting and say STEERED.", how="Steer")
    pi.settle()
    pi.pump(quiet=1.5)
    pi.finish().save()


@case
def case_followup(ctx):
    """Pi's native follow_up during a run (raw: PiDriver queues itself, see `queue`)."""
    pi = ctx.pi("followup")
    pi.prompt("Count slowly from 1 to 8.")
    pi.streaming()
    time.sleep(0.5)
    pi.raw({"type": "follow_up", "message": "Then say FOLLOWUP."})
    pi.settle()
    pi.pump(quiet=1.5)
    pi.finish().save()


@case
def case_queue(ctx):
    """PromptHow::Queue during a run: buffered by the driver, sent as a prompt after agent_settled."""
    pi = ctx.pi("queue")
    pi.prompt("Count slowly from 1 to 8.")
    pi.streaming()
    time.sleep(0.5)
    pi.prompt("Then say FOLLOWUP.", how="Queue")
    pi.settle()
    pi.settle()
    pi.finish().save()


@case
def case_set_model(ctx):
    pi = ctx.pi("set_model", model=ctx.model)
    pi.prompt(OK)
    pi.settle()
    pi.set_model(ctx.model2)
    pi.until_type("response", command="set_model")
    pi.prompt(OK)
    pi.settle()
    pi.finish().save()


@case
def case_state(ctx):
    """Shapes of the read-only commands, before and after a turn (raw: the driver only sends get_state)."""
    pi = ctx.pi("state")
    pi.until_type("response", command="get_state")
    for c in ("get_available_models", "get_session_stats", "get_messages"):
        pi.raw({"id": "s-" + c, "type": c})
        pi.until_type("response", command=c)
    pi.prompt(OK)
    pi.settle()
    for c in ("get_state", "get_messages", "get_session_stats", "get_last_assistant_text"):
        pi.raw({"id": "s2-" + c, "type": c})
        pi.until_type("response", command=c)
    pi.finish().save()


@case
def case_resume(ctx):
    """A second process continues the first one's session (`--session-id <sessionId>`)."""
    a = ctx.pi("resume_setup")
    a.prompt("Remember the word banana. Reply with the single word ok.")
    a.settle()
    a.finish().save()
    sid = a.agent_sid
    b = ctx.pi("resume", resume=sid)
    b.prompt("What word did I ask you to remember?")
    b.settle()
    b.finish().save()


@case
def case_kill9(ctx):
    """kill -9 pi mid-turn; then a new process resumes the session."""
    a = ctx.pi("kill9")
    a.prompt("Count slowly from 1 to 50.")
    a.streaming()
    time.sleep(2)
    a.drain()
    a.kill9()
    a.save()
    b = ctx.pi("kill9_resume", resume=a.agent_sid)
    b.until_type("response", command="get_state")
    b.raw({"id": "k-get_messages", "type": "get_messages"})
    b.until_type("response", command="get_messages")
    b.prompt(OK)
    b.settle()
    b.finish().save()


@case
def case_question(ctx):
    """An extension tool asks through ctx.ui.select (extension_ui_request), answered with option 1."""
    if ctx.real:
        print("   question: skipped with --real (needs the scripted ask_choice tool call)")
        return
    ext = os.path.join(ctx.scratch, "femme-ask.ts")
    with open(ext, "w") as f:
        f.write(ASK_EXTENSION)
    pi = ctx.pi("question", extra_args=["-e", ext])
    pi.prompt("Ask me to pick a color.")
    req = pi.until_type("extension_ui_request", method="select")
    pi.answer(req["id"], {"Option": 1}, {"value": req["options"][1]})
    pi.settle()
    pi.finish().save()


@case
def case_error(ctx):
    """No provider configured at all: the prompt is rejected."""
    saved = ctx.env
    empty = os.path.join(ctx.scratch, "pi-home-empty", "agent")
    os.makedirs(empty, exist_ok=True)
    ctx.env = dict(saved, PI_CODING_AGENT_DIR=empty)
    try:
        pi = ctx.pi("error", model=None)
        pi.prompt(OK)
        pi.until_type("response", command="prompt")
        pi.pump(quiet=1.5)
        pi.finish().save()
    finally:
        ctx.env = saved


# --------------------------------------------------------------------------------------------


class Ctx:
    def __init__(self, args):
        self.real = args.real
        self.scratch = os.path.abspath(args.scratch)
        self.out = os.path.abspath(args.out)
        self.cwd = self.scratch
        self.logs = os.path.join(self.scratch, "logs")
        os.makedirs(self.logs, exist_ok=True)
        self.env = dict(os.environ)
        for k in ("HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "FEMME_SID"):
            self.env.pop(k, None)
        self.agent_dir = os.path.join(self.scratch, "pi-home", "agent")
        self.mock = None
        if self.real:
            self.model, self.model2 = None, args.model2
        else:
            self.mock = Mock(self.logs)
            if os.path.isdir(self.agent_dir):
                shutil.rmtree(self.agent_dir)
            write_models_json(self.agent_dir, self.mock.port)
            self.env.update(PI_CODING_AGENT_DIR=self.agent_dir, PI_OFFLINE="1", PI_TELEMETRY="0")
            self.model, self.model2 = "mock/mock-a", "mock/mock-b"
        home = os.path.expanduser("~")
        mangled = "--" + re.sub(r"[/\\:]", "-", self.scratch.lstrip("/")) + "--"
        self.subs = [(self.agent_dir, home + "/.pi/agent"), (mangled, "--work-repo--"),
                     (self.scratch, "/work/repo"), (home, "/home/user")]
        if self.mock:
            self.subs.append(("127.0.0.1:%d" % self.mock.port, "127.0.0.1:" + PORT_PLACEHOLDER))

    def scrub(self, s):
        for a, b in self.subs:
            s = s.replace(a, b)
        return s

    def pi(self, case, model="default", resume=None, extra_args=()):
        launch = {"cwd": "/work/repo", "model": self.model if model == "default" else model, "resume": resume}
        return Pi(self, case, launch, extra_args)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("cases", nargs="*")
    ap.add_argument("--scratch", default=DEFAULT_SCRATCH, help="cwd for pi (a git repo), logs, isolated pi config")
    ap.add_argument("--out", default=FIXTURES)
    ap.add_argument("--real", action="store_true", help="use pi's configured default model instead of the mock")
    ap.add_argument("--model2", default=None, help="with --real: provider/model for the set_model case")
    ap.add_argument("--serve", action="store_true", help="run only the scripted model until killed and print "
                    "the environment for `cargo test --test live_pi -- --ignored`")
    args = ap.parse_args()
    if args.serve:
        ctx = Ctx(args)
        print("PI_CODING_AGENT_DIR=%s PI_OFFLINE=1 PI_TELEMETRY=0 FEMME_PI_MODEL=%s" % (ctx.agent_dir, ctx.model), flush=True)
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            return
    names = args.cases or list(CASES)
    for n in names:
        if n not in CASES:
            sys.exit("unknown case %s (have: %s)" % (n, ", ".join(CASES)))
    ctx = Ctx(args)
    try:
        for n in names:
            print("== case", n, flush=True)
            CASES[n](ctx)
    finally:
        for p in STARTED:
            if p.poll() is None:
                try:
                    os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        print("pi processes started: %d" % RUNS[0])


if __name__ == "__main__":
    main()

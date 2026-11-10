#!/usr/bin/env python3
"""Capture Claude Code stream-json fixtures for femme-agents (prototype P3).

Usage: capture-claude.py [CASE ...] [--out DIR] [--model M]      (no CASE = all cases)
       capture-claude.py --list

Every case spawns the same command line `ClaudeDriver::spawn` builds, in a throwaway git repo,
speaks the protocol over pipes and writes, scrubbed (see `scrub`):
  <out>/<case>.jsonl        every raw stdout line
  <out>/<case>.stdin.jsonl  every line we sent
  <out>/<case>.cmd.json     argv + env of the process (a test compares it with the driver)
Default <out> is crates/femme-agents/fixtures/claude. Haiku and tiny prompts only (about 20 model
turns in total). Every process has a 120 s watchdog. Re-run when the pinned CLI version changes and
diff the fixtures; `UPDATE_GOLDEN=1 cargo test -p femme-agents` then regenerates the event files.
Set FEMME_CAPTURE_SCRATCH to choose the scratch repo (default: $TMPDIR/femme-capture/repo).
"""
import json, os, re, signal, subprocess, sys, tempfile, threading, time, uuid, queue

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.normpath(os.path.join(HERE, "..", "fixtures", "claude"))
SCRATCH = os.environ.get("FEMME_CAPTURE_SCRATCH") or os.path.join(tempfile.gettempdir(), "femme-capture", "repo")
HOME = os.path.expanduser("~")
MODEL = "claude-haiku-5-5"
ENV = {"CLAUDE_CODE_SDK_READS_SESSION_STATE": "1"}
# Keep in sync with ClaudeDriver::spawn (tests/golden_claude.rs checks it against <case>.cmd.json).
BASE = ["claude", "-p", "--input-format=stream-json", "--output-format=stream-json", "--verbose",
        "--include-partial-messages", "--replay-user-messages", "--permission-mode=bypassPermissions",
        "--permission-prompt-tool=stdio"]
STARTED = []  # pids we started, for the final cleanup


def user_msg(text, uid):
    return {"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}, "uuid": uid}


def control(subtype, rid, **fields):
    req = {"subtype": subtype}
    req.update(fields)
    return {"type": "control_request", "request_id": rid, "request": req}


def uid(n):
    return "%08d-0000-4000-8000-%012d" % (n, n)


INIT = control("initialize", "femme-init")


def allow(rid, body):
    return {"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": body}}


class Session:
    """One claude process: raw stdout/stdin recorders plus tiny wait helpers."""

    def __init__(self, case, out, model=MODEL, extra=(), argv=None, cwd=None, env=None):
        self.case, self.out = case, out
        self.lines = []  # (seconds since spawn, raw bytes)
        self.sent = []
        self.q = queue.Queue()
        self.t0 = time.time()
        self.argv = argv or (list(BASE) + (["--model=%s" % model] if model else []) + list(extra))
        self.env = dict(ENV if env is None else env)
        e = dict(os.environ)
        e.pop("CLAUDECODE", None)
        e.update(self.env)
        self.p = subprocess.Popen(self.argv, cwd=cwd or SCRATCH, env=e, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, start_new_session=True)
        STARTED.append(self.p.pid)
        self.stderr = []
        self.dog = threading.Timer(120, self.kill)
        self.dog.daemon = True
        self.dog.start()
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._err, daemon=True).start()

    def _read(self):
        for raw in iter(self.p.stdout.readline, b""):
            raw = raw.rstrip(b"\n")
            self.lines.append((time.time() - self.t0, raw))
            self.q.put(raw)
        self.q.put(None)

    def _err(self):
        for raw in iter(self.p.stderr.readline, b""):
            self.stderr.append(raw.decode("utf-8", "replace"))

    def send(self, obj):
        line = json.dumps(obj, separators=(",", ":"))
        self.sent.append(line)
        try:
            self.p.stdin.write((line + "\n").encode())
            self.p.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass
        return obj

    def wait(self, pred, timeout=90):
        """Read lines until pred(parsed_json) is true; the matching message, or None on timeout/EOF."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                raw = self.q.get(timeout=max(0.05, end - time.time()))
            except queue.Empty:
                break
            if raw is None:
                self.q.put(None)
                return None
            try:
                m = json.loads(raw)
            except Exception:
                continue
            if pred(m):
                return m
        return None

    def wait_result(self, timeout=90):
        return self.wait(lambda m: m.get("type") == "result", timeout)

    def wait_ctl(self, rid, timeout=30):
        return self.wait(lambda m: m.get("type") == "control_response" and m["response"].get("request_id") == rid, timeout)

    def wait_tool_use(self, timeout=60):
        return self.wait(lambda m: m.get("type") == "assistant" and any(b.get("type") == "tool_use" for b in m["message"]["content"]), timeout)

    def wait_text(self, timeout=60):
        return self.wait(lambda m: m.get("type") == "stream_event" and m["event"].get("type") == "content_block_delta"
                         and m["event"]["delta"].get("type") == "text_delta", timeout)

    def session_id(self):
        for _, raw in self.lines:
            m = json.loads(raw)
            if m.get("type") == "system" and m.get("subtype") == "init":
                return m["session_id"]

    def close(self, grace=10):
        self.dog.cancel()
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            self.kill()
        return self.save()

    def kill(self, sig=signal.SIGKILL):
        try:
            os.killpg(self.p.pid, sig)
        except ProcessLookupError:
            pass
        try:
            self.p.wait(timeout=5)
        except Exception:
            pass

    def save(self):
        os.makedirs(self.out, exist_ok=True)
        with open(os.path.join(self.out, self.case + ".jsonl"), "w") as f:
            for _, raw in self.lines:
                f.write(scrub_line(raw.decode("utf-8", "replace")) + "\n")
        with open(os.path.join(self.out, self.case + ".stdin.jsonl"), "w") as f:
            for s in self.sent:
                f.write(scrub_line(s) + "\n")
        argv = [scrub_str(a) for a in self.argv]
        with open(os.path.join(self.out, self.case + ".cmd.json"), "w") as f:
            json.dump({"argv": argv, "env": self.env, "exit": self.p.poll()}, f, indent=1)
            f.write("\n")
        return self


# ---- scrubbing: nothing of the capturing machine/account may reach the fixtures ---------------

EMAIL = re.compile(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+")
FAKE_ACCOUNT = {"email": "user@example.com", "organization": "Example Org", "subscriptionType": "ExamplePlan", "apiProvider": "firstParty"}


def scrub_str(s):
    for real in {SCRATCH, os.path.realpath(SCRATCH)}:
        s = s.replace(real, "/work/repo")
        s = s.replace(real.replace("/", "-").replace("_", "-"), "-work-repo")  # ~/.claude/projects/<mangled cwd>
    s = s.replace(HOME, "/home/user")
    return EMAIL.sub("user@example.com", s)


def scrub_walk(o):
    if isinstance(o, str):
        return scrub_str(o)
    if isinstance(o, list):
        return [scrub_walk(x) for x in o]
    if isinstance(o, dict):
        r = {}
        for k, v in o.items():
            if k == "signature" and isinstance(v, str) and len(v) > 16:
                r[k] = "c2lnbmF0dXJl"  # opaque thinking signature, only its presence matters
            else:
                r[k] = scrub_walk(v)
        return r
    return o


def scrub_init_response(r):
    """The `initialize` response lists the user's commands, agents, account...: keep the shape, fake the content."""
    keep = ("output_style", "available_output_styles", "claude_code_version", "current_permission_mode", "feedback_mode",
            "analytics_disabled", "fast_mode_state", "fast_mode_disabled_reason", "session_state", "capabilities")
    out = {k: r[k] for k in keep if k in r}
    out["commands"] = [{"name": "compact", "description": "Clear conversation history but keep a summary in context", "argumentHint": "<optional custom summarization instructions>"},
                       {"name": "context", "description": "Show current context usage", "argumentHint": ""}]
    out["agents"] = [{"name": "general-purpose", "description": "General-purpose agent for researching complex questions"}]
    out["user_output_styles_dir"] = "/home/user/.claude/output-styles"
    out["models"] = r.get("models", [])[:4]
    out["account"] = dict(FAKE_ACCOUNT)
    out["pid"] = 4242
    return out


def scrub_line(line):
    try:
        m = json.loads(line)
    except Exception:
        return scrub_str(line)
    t, sub = m.get("type"), m.get("subtype")
    if t == "control_response" and isinstance(m["response"].get("response"), dict) and "commands" in m["response"]["response"]:
        m["response"]["response"] = scrub_init_response(m["response"]["response"])
    elif t == "system" and sub == "init":
        for k, v in (("tools", m.get("tools", [])[:8]), ("mcp_servers", []), ("slash_commands", ["compact", "context", "help"]),
                     ("skills", []), ("plugins", []), ("agents", ["general-purpose", "Explore"]), ("plugin_errors", [])):
            if k in m:
                m[k] = v
        if "memory_paths" in m:
            m["memory_paths"] = {"auto": "/home/user/.claude/projects/-work-repo/memory/"}
        if "messaging_socket_path" in m:
            m["messaging_socket_path"] = "/run/user/1000/cc-socks/1.sock"
    elif t == "system" and sub == "commands_changed":
        # the user's own skills and commands
        m["commands"] = scrub_init_response({"commands": []})["commands"]
    elif t == "system" and sub in ("hook_started", "hook_response", "hook_progress"):
        for k in ("output", "stdout", "stderr", "command"):
            if k in m:
                m[k] = ""
    elif t == "assistant" and m.get("local_command_source"):
        # `claude -p /usage`: keep the numbers, drop the usage-habit statistics
        text = "Current session: 52% used · resets Nov 2, 11am (UTC)\nCurrent week (Fable): 1% used · resets Nov 8, 6:59am (UTC)"
        m["message"]["content"] = [{"type": "text", "text": text}]
        m["local_command_source"] = "<local-command-stdout>%s</local-command-stdout>" % text
    elif t == "result" and m.get("local_command"):
        m["result"] = "Current session: 52% used · resets Nov 2, 11am (UTC)\nCurrent week (Fable): 1% used · resets Nov 8, 6:59am (UTC)"
    return json.dumps(scrub_walk(m), separators=(",", ":"), ensure_ascii=False)


# ---- cases -------------------------------------------------------------------------------------

CASES = {}


def case(fn):
    CASES[fn.__name__.replace("case_", "")] = fn
    return fn


def first_prompt(s, text, n=1):
    s.send(INIT)
    s.send(user_msg(text, uid(n)))


def drive(s, answer=None, timeout=100):
    """Answer every can_use_tool request with `answer(request) -> response body` until a result arrives."""
    end = time.time() + timeout
    while time.time() < end:
        m = s.wait(lambda m: m.get("type") in ("control_request", "result"), max(1, end - time.time()))
        if m is None or m["type"] == "result":
            return m
        req = m["request"]
        time.sleep(0.4)
        s.send(allow(m["request_id"], answer(req) if answer else {"behavior": "allow", "updatedInput": req.get("input", {})}))
    return None


def pick(index):
    def f(req):
        inp = req["input"]
        if req.get("tool_name") != "AskUserQuestion":
            return {"behavior": "allow", "updatedInput": inp}
        answers = {q["question"]: (", ".join(o["label"] for o in q["options"][:2]) if q.get("multiSelect") else q["options"][index]["label"])
                   for q in inp["questions"]}
        return {"behavior": "allow", "updatedInput": dict(inp, answers=answers)}
    return f


ASK = "Use the AskUserQuestion tool to ask me whether I prefer A or B (two options, labels A and B), then wait for my answer and tell me what I chose."


@case
def case_ok(out):
    s = Session("ok", out)
    first_prompt(s, "Reply with the single word ok.")
    s.wait_result()
    return s.close()


@case
def case_tool(out):
    s = Session("tool", out)
    first_prompt(s, "Run `echo hi` with your Bash tool and then reply with the single word done.")
    s.wait_result()
    return s.close()


@case
def case_question(out):
    s = Session("question", out)
    first_prompt(s, ASK)
    drive(s, pick(1))
    return s.close()


@case
def case_question_multi(out):
    s = Session("question_multi", out)
    first_prompt(s, "Use the AskUserQuestion tool to ask me which of A, B, C I like, allowing multiple answers (multiSelect true). Then tell me exactly what I chose.")
    drive(s, pick(0))
    return s.close()


@case
def case_question_two(out):
    s = Session("question_two", out)
    first_prompt(s, "In ONE AskUserQuestion call ask me two questions: 'Color?' with options red and blue, and 'Size?' with options small and large. Then tell me exactly what I chose.")
    drive(s, pick(1))
    return s.close()


@case
def case_question_deny(out):
    s = Session("question_deny", out)
    first_prompt(s, ASK)
    drive(s, lambda req: {"behavior": "deny", "message": "The user declined to answer."})
    return s.close()


@case
def case_plan(out):
    s = Session("plan", out)
    first_prompt(s, "Call the EnterPlanMode tool now, then in plan mode write a one-line plan 'create hello.txt' and call ExitPlanMode with it. After approval just say done (do not create anything).")
    drive(s)
    return s.close()


@case
def case_interrupt(out):
    s = Session("interrupt", out)
    first_prompt(s, "Write the numbers from 1 to 400, one per line, nothing else.", 1)
    s.wait_text()
    time.sleep(1.5)
    s.send(control("interrupt", "femme-int-1"))
    s.wait_result(30)
    time.sleep(1.5)
    s.send(user_msg("Reply with the single word ok.", uid(2)))
    s.wait_result(40)
    return s.close()


@case
def case_ask_interrupt(out):
    s = Session("ask_interrupt", out)
    first_prompt(s, ASK)
    s.wait(lambda m: m.get("type") == "control_request", 60)
    time.sleep(1.0)
    s.send(control("interrupt", "femme-int-1"))
    s.wait_result(30)
    time.sleep(1.0)
    return s.close()


@case
def case_queue(out):
    """A second message while a tool runs is folded into the running turn at the tool boundary."""
    s = Session("queue", out)
    first_prompt(s, "Run `sleep 5` with your Bash tool, then reply with the single word first.", 1)
    s.wait_tool_use()
    time.sleep(0.5)
    s.send(user_msg("Also, after that, reply with the single word second.", uid(2)))
    s.wait_result(45)
    s.wait_result(5)
    return s.close()


@case
def case_queue_text(out):
    """A second message while the model streams text only runs as its own turn afterwards."""
    s = Session("queue_text", out)
    first_prompt(s, "Write the numbers from 1 to 300, one per line, nothing else.", 1)
    s.wait_text()
    s.send(user_msg("Now reply with the single word second.", uid(2)))
    s.wait_result(40)
    s.wait_result(30)
    return s.close()


@case
def case_set_model(out):
    s = Session("set_model", out)
    first_prompt(s, "Reply with the single word ok.", 1)
    s.wait_result(40)
    s.send(control("set_model", "femme-model-1", model="claude-haiku-4-5-20251001"))
    s.wait_ctl("femme-model-1")
    time.sleep(0.5)
    s.send(user_msg("Reply with the single word ok.", uid(2)))
    s.wait_result(40)
    return s.close()


@case
def case_set_model_bad(out):
    s = Session("set_model_bad", out)
    s.send(INIT)
    s.wait_ctl("femme-init")
    s.send(control("set_model", "femme-model-1", model="no-such-model-xyz"))
    s.wait_ctl("femme-model-1")
    s.send(control("interrupt", "femme-int-2"))  # interrupting an idle session is a no-op
    s.wait_ctl("femme-int-2")
    return s.close()


@case
def case_resume(out):
    s1 = Session("resume_1", out)
    first_prompt(s1, "Remember the code word pelican. Reply with the single word ok.", 1)
    s1.wait_result(40)
    sid = s1.session_id()
    s1.close()
    s2 = Session("resume_2", out, extra=["--resume=%s" % sid])
    first_prompt(s2, "What was the code word I asked you to remember? Reply with that single word.", 2)
    s2.wait_result(40)
    return s2.close()


@case
def case_session_id(out):
    sid = str(uuid.uuid4())
    s = Session("session_id", out, extra=["--session-id=%s" % sid])
    first_prompt(s, "Reply with the single word ok.", 1)
    s.wait_result(40)
    assert s.session_id() == sid, "--session-id was not honoured"
    return s.close()


@case
def case_kill9(out):
    """kill -9 in the middle of a tool call, then --resume the same session."""
    s1 = Session("kill9_1", out)
    first_prompt(s1, "Run `python3 -c 'import time; time.sleep(40)'` with your Bash tool, then reply with the single word done.", 1)
    s1.wait_tool_use()
    time.sleep(3.0)
    sid = s1.session_id()
    s1.kill()
    s1.close()
    s2 = Session("kill9_2", out, extra=["--resume=%s" % sid])
    first_prompt(s2, "What did I ask you to do in my previous message, and did it finish? Answer in one short sentence.", 2)
    s2.wait_result(60)
    return s2.close()


@case
def case_reinit(out):
    """`initialize` can be sent again at any time; while a question is pending it lists it."""
    s = Session("reinit", out)
    first_prompt(s, ASK)
    req = s.wait(lambda m: m.get("type") == "control_request", 60)
    time.sleep(0.5)
    s.send(control("initialize", "femme-init-2"))
    s.wait_ctl("femme-init-2")
    s.send(allow(req["request_id"], pick(0)(req["request"])))
    s.wait_result(40)
    return s.close()


# A stdio MCP server whose tools ask the user through `elicitation/create`: `ask_form` a form
# (one optional text field), `ask_url` a URL-mode request it confirms with
# `notifications/elicitation/complete` once answered.
ELICIT_MCP = r'''
import json, sys
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def wait_reply(rid):
    for line in sys.stdin:
        m = json.loads(line)
        if m.get("id") == rid and "method" not in m:
            return m
        if m.get("method") == "ping":
            send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
FORM = {"message": "Which release name should I use?",
        "requestedSchema": {"type": "object", "properties": {"name": {"type": "string", "title": "Release name"},
                                                               "draft": {"type": "boolean", "title": "Draft"}}}}
URL = {"mode": "url", "message": "Sign in to Example to continue", "url": "https://example.com/auth?state=abc",
       "elicitationId": "el-1"}
TOOLS = [{"name": "ask_form", "description": "Asks the user for a release name", "inputSchema": {"type": "object", "properties": {}}},
         {"name": "ask_url", "description": "Asks the user to sign in", "inputSchema": {"type": "object", "properties": {}}}]
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": m["params"]["protocolVersion"],
              "capabilities": {"tools": {}}, "serverInfo": {"name": "elicit", "version": "1"}}})
    elif meth == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif meth == "tools/call":
        url = m["params"]["name"] == "ask_url"
        send({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create", "params": URL if url else FORM})
        r = wait_reply("elicit-1")
        if url and r.get("result", {}).get("action") == "accept":
            send({"jsonrpc": "2.0", "method": "notifications/elicitation/complete", "params": {"elicitationId": "el-1"}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "elicitation: " + json.dumps(r.get("result", r.get("error")))}]}})
    elif meth == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
'''


def elicit_session(name, out):
    path = os.path.join(os.path.dirname(SCRATCH), "elicit-mcp.py")
    with open(path, "w") as f:
        f.write(ELICIT_MCP)
    cfg = json.dumps({"mcpServers": {"elicit": {"type": "stdio", "command": "python3", "args": [path]}}})
    return Session(name, out, extra=["--strict-mcp-config", "--mcp-config=" + cfg])


@case
def case_elicit_form(out):
    """An MCP server's form elicitation, accepted with the form's fields left empty."""
    s = elicit_session("elicit_form", out)
    first_prompt(s, "Call the mcp__elicit__ask_form tool once, then reply with exactly what it returned.")
    drive(s, lambda req: {"action": "accept", "content": {}})
    return s.close()


@case
def case_elicit_url(out):
    """A URL-mode elicitation, accepted; the server then confirms it complete."""
    s = elicit_session("elicit_url", out)
    first_prompt(s, "Call the mcp__elicit__ask_url tool once, then reply with exactly what it returned.")
    drive(s, lambda req: {"action": "accept"})
    return s.close()


@case
def case_unknown_ctl(out):
    """A control request FEMME has no answer for (a hook callback we register here only to provoke
    one), refused with an error response: the CLI carries on."""
    s = Session("unknown_ctl", out)
    s.send(control("initialize", "femme-init", hooks={"PreToolUse": [{"matcher": "Bash", "hookCallbackIds": ["h1"]}]}))
    s.send(user_msg("Run `echo hi` with your Bash tool and then reply with the single word done.", uid(1)))
    end = time.time() + 100
    while time.time() < end:
        m = s.wait(lambda m: m.get("type") in ("control_request", "result"), max(1, end - time.time()))
        if m is None or m["type"] == "result":
            break
        time.sleep(0.4)
        s.send({"type": "control_response", "response": {"subtype": "error", "request_id": m["request_id"],
                                                          "error": "FEMME does not handle %s" % m["request"]["subtype"]}})
    return s.close()


@case
def case_error_model(out):
    s = Session("error_model", out, model="claude-nonexistent-9")
    first_prompt(s, "Reply with the single word ok.")
    s.wait_result(40)
    time.sleep(0.5)
    return s.close()


@case
def case_error_resume(out):
    s = Session("error_resume", out, extra=["--resume=00000000-0000-4000-8000-000000000000"])
    first_prompt(s, "Reply with the single word ok.")
    s.wait_result(20)
    time.sleep(1.0)
    return s.close()


@case
def case_usage(out):
    """`claude -p /usage`: a local command (no model turn); the 5-hour figure the scheduler polls."""
    s = Session("usage", out, argv=["claude", "-p", "/usage", "--output-format", "stream-json", "--verbose", "--no-session-persistence"],
                cwd=tempfile.gettempdir(), env={})
    s.wait_result(60)
    return s.close()


def main():
    argv = sys.argv[1:]
    out, names, model, i = FIXTURES, [], None, 0
    while i < len(argv):
        a = argv[i]
        if a == "--out":
            out = argv[i + 1]; i += 1
        elif a == "--list":
            print("\n".join(CASES)); return
        else:
            names.append(a)
        i += 1
    os.makedirs(SCRATCH, exist_ok=True)
    if not os.path.isdir(os.path.join(SCRATCH, ".git")):
        subprocess.run("git init -q && git -c user.name=x -c user.email=x@x commit -q --allow-empty -m init", shell=True, cwd=SCRATCH, check=True)
    try:
        for n in names or list(CASES):
            print("== case", n, flush=True)
            s = CASES[n](out)
            print("   %d lines, %d sent, exit=%s" % (len(s.lines), len(s.sent), s.p.poll()), flush=True)
    finally:
        for pid in STARTED:
            try:
                os.killpg(pid, signal.SIGKILL)
            except Exception:
                pass


if __name__ == "__main__":
    main()

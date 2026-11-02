#!/usr/bin/python3 -I
# A `claude` for femmed's tests. Headless (`-p`): speaks the stream-json protocol the real CLI
# speaks (shapes copied from crates/femme-agents/fixtures/claude). TUI (no `-p`): the bash fake
# from tests/session.rs, reporting through the hooks. Prompts it understands, in either mode:
#   ask    asks a question (options a / b) and waits for the answer
#   ask2   headless only: asks two in one request ("Which?" a / b, "Second?" x / y), waits for both
#   slow   50 lines 200 ms apart (an interrupt or a restart can land inside)
#   quit   exits 0 mid-turn        crash   exits 1 with text on stderr
#   <any>  replies "echo: <any>", with a 100 ms pause mid-turn
import json
import os
import select
import subprocess
import sys
import time
import uuid

FEMMED = "__FEMMED__"
ARGV = sys.argv[1:]


def arg(prefix, default):
    return next((a.split("=", 1)[1] for a in ARGV if a.startswith(prefix)), default)


SID = arg("--resume=", None) or arg("--session-id=", "fake-1")
MODEL = arg("--model=", "claude-haiku-5-5")
HOME = os.environ.get("HOME", "/tmp")

with open(os.path.join(HOME, "claude.pid"), "w") as f:
    f.write(f"{os.getpid()}\n")


# ---- TUI --------------------------------------------------------------------------------------


def hook(event, body):
    subprocess.run([FEMMED, "hook", "claude", event], input=body.encode(), check=False)


def say(s):
    sys.stdout.write(s + "\n")
    sys.stdout.flush()


def tui():
    hook("SessionStart", json.dumps({"session_id": SID, "transcript_path": "/nowhere"}))
    say("fake claude ready")
    for line in sys.stdin:
        line = line.rstrip("\r\n")
        hook("UserPromptSubmit", "{}")
        if line == "ask":
            hook("PermissionRequest", json.dumps({"tool_name": "AskUserQuestion", "tool_input": {
                "questions": [{"question": "Which?", "options": [{"label": "a"}, {"label": "b"}]}]}}))
            answer = sys.stdin.readline().rstrip("\r\n")
            say(f"answered: {answer}")
            hook("PostToolUse", json.dumps({"tool_name": "AskUserQuestion"}))
        elif line in ("quit", "/exit"):
            hook("SessionEnd", json.dumps({"reason": "exit"}))
            sys.exit(0)
        elif line == "crash":
            sys.stderr.write("fake claude: crash\n")
            sys.exit(1)
        elif line == "slow":
            for i in range(1, 51):
                say(f"tick {i}")
                time.sleep(0.2)
        else:
            say(f"echo: {line}")
        hook("Stop", "{}")
    hook("SessionEnd", json.dumps({"reason": "exit"}))


# ---- headless ---------------------------------------------------------------------------------


def out(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def uid():
    return str(uuid.uuid4())


inbox = []  # stdin lines read while a turn runs, in order


def read_line(timeout):
    """One stdin line within `timeout` s (None: wait), parsed; None on timeout or EOF."""
    if inbox:
        return inbox.pop(0)
    if timeout is not None and not select.select([sys.stdin], [], [], timeout)[0]:
        return None
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)  # stdin closed: the daemon wants us gone
    line = line.strip()
    return json.loads(line) if line else {}


def control_ok(rid, response=None):
    out({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": response or {}}})


def system(subtype, **kw):
    out({"type": "system", "subtype": subtype, "session_id": SID, "uuid": uid(), **kw})


def init():
    system("init", cwd=os.getcwd(), model=MODEL, permissionMode="bypassPermissions", tools=["AskUserQuestion", "Bash"],
           claude_code_version="2.1.302")


def lifecycle(u, state):
    out({"type": "command_lifecycle", "command_uuid": u, "state": state, "uuid": uid(), "session_id": SID})


def user_text(text, u=None, replay=False):
    line = {"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": None, "session_id": SID, "uuid": u or uid()}
    if replay:
        line["isReplay"] = True  # `--replay-user-messages`: the echo of a message the host wrote
    out(line)


def usage(n):
    return {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": n}


def result(u, ok=True, n=4):
    r = {"type": "result", "subtype": "success", "is_error": False, "duration_ms": 100, "duration_api_ms": 90,
         "num_turns": 1, "session_id": SID, "total_cost_usd": 0.001, "usage": usage(n), "uuid": uid(),
         "user_message_uuids": [u]}
    if ok:
        r.update(stop_reason="end_turn", terminal_reason="completed", result="")
        out(r)
        lifecycle(u, "completed")
    else:
        r.update(stop_reason=None, terminal_reason="aborted_streaming", duration_ms=0, duration_api_ms=0, result="")
        out(r)
        lifecycle(u, "cancelled")
    system("session_state_changed", state="idle", sdk_host_only=True)


def stream(mid, texts, gap, interruptible, u):
    """Streams `texts` as one text block, `gap` s apart. Returns False when interrupted."""
    out({"type": "stream_event", "event": {"type": "message_start", "message": {
        "model": MODEL, "id": mid, "type": "message", "role": "assistant", "content": [], "stop_reason": None,
        "usage": usage(0)}}, "session_id": SID, "parent_tool_use_id": None, "uuid": uid(), "api_message_id": mid,
        "user_message_uuid": u, "user_message_uuids": [u]})
    out({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
         "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
    got = []
    for i, t in enumerate(texts):
        if i and not pause(gap, interruptible):
            out({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0},
                 "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
            user_text("[Request interrupted by user]")
            return False
        out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": t}},
             "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
        got.append(t)
    out({"type": "assistant", "message": {"model": MODEL, "id": mid, "type": "message", "role": "assistant",
         "content": [{"type": "text", "text": "".join(got)}], "stop_reason": None, "usage": usage(len(got))},
         "parent_tool_use_id": None, "session_id": SID, "uuid": uid(), "user_message_uuid": u, "user_message_uuids": [u]})
    out({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}, "session_id": SID,
         "parent_tool_use_id": None, "uuid": uid()})
    out({"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
         "usage": usage(len(got))}, "session_id": SID, "parent_tool_use_id": None, "uuid": uid(), "api_message_id": mid})
    out({"type": "stream_event", "event": {"type": "message_stop"}, "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
    return True


def pause(secs, interruptible):
    """Sleeps `secs`, serving control requests meanwhile. False when the turn was interrupted."""
    end = time.monotonic() + secs
    while True:
        left = end - time.monotonic()
        if left <= 0:
            return True
        v = read_line(left)
        if v is None:
            return True
        if control(v) == "interrupt" and interruptible:
            return False


def control(v):
    """Serves a control request between turns or mid-turn; queues anything else. Returns the subtype served."""
    if v.get("type") == "control_request":
        rid, req = v.get("request_id", ""), v.get("request", {})
        sub = req.get("subtype")
        if sub == "initialize":
            control_ok(rid, {"session_state": "idle", "claude_code_version": "2.1.302", "current_permission_mode": "bypassPermissions"})
            init()
        elif sub == "interrupt":
            control_ok(rid, {"still_queued": []})
        elif sub == "set_model":
            global MODEL
            MODEL = req.get("model", MODEL)
            control_ok(rid)
            user_text(f"<local-command-stdout>Set model to `{MODEL}`</local-command-stdout>")
        else:
            out({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": f"unknown subtype {sub}"}})
        return sub
    if v:
        inbox.append(v)
    return None


def ask(u, two=False):
    tid, rid = f"toolu_{uid()[:8]}", uid()
    questions = [{"question": "Which?", "header": "Which", "options": [{"label": "a", "description": "Option a"},
                                                                       {"label": "b", "description": "Option b"}], "multiSelect": False}]
    if two:
        questions.append({"question": "Second?", "header": "Second", "options": [{"label": "x"}, {"label": "y"}],
                          "multiSelect": False})
    mid = f"msg_{uid()[:8]}"
    out({"type": "assistant", "message": {"model": MODEL, "id": mid, "type": "message", "role": "assistant",
         "content": [{"type": "tool_use", "id": tid, "name": "AskUserQuestion", "input": {"questions": questions}}],
         "stop_reason": None, "usage": usage(7)}, "parent_tool_use_id": None, "session_id": SID, "uuid": uid(),
         "user_message_uuid": u, "user_message_uuids": [u]})
    out({"type": "control_request", "request_id": rid, "request": {
        "subtype": "can_use_tool", "tool_name": "AskUserQuestion", "display_name": "AskUserQuestion",
        "input": {"questions": questions}, "tool_use_id": tid, "requires_user_interaction": True}})
    while True:
        v = read_line(None)
        if v.get("type") == "control_response" and v.get("response", {}).get("request_id") == rid:
            r = v["response"].get("response", {})
            break
        if control(v) == "interrupt":
            out({"type": "control_cancel_request", "request_id": rid})
            user_text("[Request interrupted by user]")
            return False
    if r.get("behavior") == "allow":
        answers = r.get("updatedInput", {}).get("answers", {})
        given = [(q["question"], answers[q["question"]]) for q in questions if q["question"] in answers]  # in the order asked
        chosen = ", ".join(a for _, a in given) or "nothing"
        said = ", ".join(f'"{q}"="{a}"' for q, a in given)
        out({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid,
             "content": f"Your questions have been answered: {said}. You can now continue with these answers in mind."}]},
             "parent_tool_use_id": None, "session_id": SID, "uuid": uid(),
             "tool_use_result": {"questions": questions, "answers": answers}})
    else:
        chosen = "nothing"
        out({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid,
             "content": "The user declined to answer.", "is_error": True}]},
             "parent_tool_use_id": None, "session_id": SID, "uuid": uid()})
    return stream(f"msg_{uid()[:8]}", ["you chose: ", chosen], 0.0, True, u)


def turn(v):
    u = v.get("uuid") or uid()
    text = "".join(b.get("text", "") for b in v.get("message", {}).get("content", []) if b.get("type") == "text")
    word = text.split("\n", 1)[0].strip()  # the daemon appends standing orders: the first line is the cue
    lifecycle(u, "queued")
    system("session_state_changed", state="running", sdk_host_only=True)
    lifecycle(u, "started")
    init()
    user_text(text, u, replay=True)
    if word == "quit":
        sys.exit(0)
    if word == "crash":
        sys.stderr.write("fake claude: crash\n")
        sys.stderr.flush()
        sys.exit(1)
    if word in ("ask", "ask2"):
        ok = ask(u, word == "ask2")
    elif word == "slow":
        ok = stream(f"msg_{uid()[:8]}", [f"tick {i}\n" for i in range(1, 51)], 0.2, True, u)
    else:
        ok = stream(f"msg_{uid()[:8]}", ["echo: ", text], 0.1, True, u)
    if ok:
        out({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "resetsAt": 1793617200, "rateLimitType": "five_hour",
             "unifiedWindows": {"five_hour": {"utilization": 0.31, "resetsAt": 1793617200}}}, "uuid": uid(), "session_id": SID})
    result(u, ok)


def headless():
    while True:
        v = read_line(None)
        if v.get("type") == "user":
            turn(v)
        else:
            control(v)
            while inbox and inbox[0].get("type") == "user":
                turn(inbox.pop(0))


if "-p" in ARGV:
    headless()
else:
    tui()

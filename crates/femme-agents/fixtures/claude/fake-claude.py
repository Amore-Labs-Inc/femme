#!/usr/bin/python3 -I
# A `claude` for femmed's tests. Headless (`-p`): speaks the stream-json protocol the real CLI
# speaks (shapes copied from crates/femme-agents/fixtures/claude). TUI (no `-p`): the bash fake
# from tests/session.rs, reporting through the hooks. Prompts it understands, in either mode:
#   ask    asks a question (options a / b) and waits for the answer
#   ask2   headless only: asks two in one request ("Which?" a / b, "Second?" x / y), waits for both
#   slow   50 lines 200 ms apart (an interrupt or a restart can land inside); a user message
#          written while it runs is folded in like the CLI does at a tool boundary: echoed at once
#          with isReplay, "echo: <it>" appended to the stream, both uuids in the one result
#   compact  headless only: a big call (150002 tokens read), the compaction, then 50 lines 200 ms apart
#   bgtask headless only: a Bash call run as a foreground task (`task_started`) for 5 s; a
#          `background_tasks` control request moves it to the background (claude 2.1.319): the
#          result says so, the turn ends, and 1 s later the task's notification starts a turn
#   (a `rewind_conversation` control request answers `rewound` with the named message's text)
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


FOLDED = []  # uuids of the messages folded into the running turn (`stream(fold=True)`)


def result(u, ok=True, n=4):
    us = [u] + FOLDED
    FOLDED.clear()
    r = {"type": "result", "subtype": "success", "is_error": False, "duration_ms": 100, "duration_api_ms": 90,
         "num_turns": 1, "session_id": SID, "total_cost_usd": 0.001, "usage": usage(n), "uuid": uid(),
         "user_message_uuids": us}
    if ok:
        r.update(stop_reason="end_turn", terminal_reason="completed", result="")
        # one entry per model the turn used; the window is what the composer's bar is measured against
        r["modelUsage"] = {MODEL: {"inputTokens": 2, "outputTokens": n, "cacheReadInputTokens": 0, "cacheCreationInputTokens": 0,
                                   "contextWindow": 200000, "maxOutputTokens": 64000}}
        out(r)
        for x in us:
            lifecycle(x, "completed")
    else:
        r.update(stop_reason=None, terminal_reason="aborted_streaming", duration_ms=0, duration_api_ms=0, result="")
        out(r)
        for x in us:
            lifecycle(x, "cancelled")
    system("session_state_changed", state="idle", sdk_host_only=True)


def stream(mid, texts, gap, interruptible, u, fold=False):
    """Streams `texts` as one text block, `gap` s apart. Returns False when interrupted.
    With `fold`, a user message that arrives meanwhile joins this turn (see `slow`)."""
    out({"type": "stream_event", "event": {"type": "message_start", "message": {
        "model": MODEL, "id": mid, "type": "message", "role": "assistant", "content": [], "stop_reason": None,
        "usage": usage(0)}}, "session_id": SID, "parent_tool_use_id": None, "uuid": uid(), "api_message_id": mid,
        "user_message_uuid": u, "user_message_uuids": [u]})
    out({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
         "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
    got = []
    texts = list(texts)
    i = 0
    while i < len(texts):
        t = texts[i]
        if i and not pause(gap, interruptible):
            out({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0},
                 "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
            user_text("[Request interrupted by user]")
            return False
        if fold:
            while inbox and inbox[0].get("type") == "user":
                v = inbox.pop(0)
                u2 = v.get("uuid") or uid()
                text2 = "".join(b.get("text", "") for b in v["message"]["content"] if b.get("type") == "text")
                lifecycle(u2, "queued")
                user_text(text2, u2, replay=True)
                lifecycle(u2, "started")
                FOLDED.append(u2)
                texts.append(f"echo: {text2}\n")
        out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": t}},
             "session_id": SID, "parent_tool_use_id": None, "uuid": uid()})
        got.append(t)
        i += 1
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


PROMPTS = {}  # the user messages taken, by uuid: what `rewind_conversation` gives back


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
        elif sub == "rewind_conversation":
            # claude 2.1.321: the conversation goes back to before that message, whose text returns
            target = req.get("target_message_uuid")
            if target in PROMPTS:
                control_ok(rid, {"rewound": True, "targetMessageUuid": target, "prefillText": PROMPTS[target],
                                 "precedingAssistantUuid": None})
            else:
                control_ok(rid, {"rewound": False, "reason": "target_not_found"})
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


def tool_result(tid, text):
    out({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid,
         "content": text, "is_error": False}]}, "parent_tool_use_id": None, "session_id": SID, "uuid": uid()})


def bgtask(u):
    """A foreground task for 5 s, or until a `background_tasks` request. Returns None when the
    turn was ended here (backgrounded), else whether the stream went through."""
    tid, task = f"toolu_{uid()[:8]}", f"task_{uid()[:6]}"
    out({"type": "assistant", "message": {"model": MODEL, "id": f"msg_{uid()[:8]}", "type": "message", "role": "assistant",
         "content": [{"type": "tool_use", "id": tid, "name": "Bash", "input": {"command": "sleep 5", "description": "Sleep"}}],
         "stop_reason": None, "usage": usage(7)}, "parent_tool_use_id": None, "session_id": SID, "uuid": uid(),
         "user_message_uuid": u, "user_message_uuids": [u]})
    system("task_started", task_id=task, run_id="run-1", tool_use_id=tid, description="Sleep", is_backgrounded=False,
           task_type="local_bash")
    end, bg = time.monotonic() + 5, False
    while time.monotonic() < end:
        v = read_line(end - time.monotonic())
        if v is None:
            break
        if v.get("type") == "control_request" and v.get("request", {}).get("subtype") == "background_tasks":
            control_ok(v.get("request_id", ""))
            bg = True
            break
        if control(v) == "interrupt":
            user_text("[Request interrupted by user]")
            return False
    if not bg:
        tool_result(tid, "")
        return stream(f"msg_{uid()[:8]}", ["slept"], 0.0, True, u)
    system("background_tasks_changed", tasks=[{"task_id": task, "run_id": "run-1", "task_type": "local_bash", "description": "Sleep"}])
    system("task_updated", task_id=task, run_id="run-1", patch={"is_backgrounded": True})
    tool_result(tid, f"Command was manually backgrounded by user with ID: {task}. Output is being written to: /tmp/{task}.output.")
    result(u, stream(f"msg_{uid()[:8]}", ["in the background"], 0.0, True, u))
    pause(1.0, False)
    system("task_notification", task_id=task, run_id="run-1", tool_use_id=tid, status="completed", output_file=f"/tmp/{task}.output",
           summary="Sleep")
    u2 = uid()
    result(u2, stream(f"msg_{uid()[:8]}", ["the task printed 42"], 0.0, True, u2))
    return None


def turn(v):
    u = v.get("uuid") or uid()
    text = "".join(b.get("text", "") for b in v.get("message", {}).get("content", []) if b.get("type") == "text")
    PROMPTS[u] = text
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
        ok = stream(f"msg_{uid()[:8]}", [f"tick {i}\n" for i in range(1, 51)], 0.2, True, u, fold=True)
    elif word == "bgtask":
        ok = bgtask(u)
        if ok is None:
            return
    elif word == "compact":
        big = {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 150000, "output_tokens": 1}
        out({"type": "assistant", "message": {"model": MODEL, "id": f"msg_{uid()[:8]}", "type": "message", "role": "assistant",
             "content": [], "stop_reason": None, "usage": big}, "parent_tool_use_id": None, "session_id": SID,
             "uuid": uid(), "user_message_uuid": u, "user_message_uuids": [u]})
        system("compact_boundary", compact_metadata={"trigger": "manual", "pre_tokens": 150002})
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

#!/usr/bin/env python3
"""Capture the REAL hook payloads Claude Code, Codex and Pi send to `femmed hook ...` in PTY mode.

Writes `crates/femme-agents/fixtures/hooks/<agent>-<event>[-<variant>].json`: each file is the
payload exactly as `femmed hook` receives it (the stdin text; for Codex `notify` the last argv
element), scrubbed. `tests/hooks.rs` feeds them to `femme_agents::hooks::parse_hook`. Evidence
goes to `fixtures/hooks/raw/`: every call of a run in order (`<agent>-<run>.calls.jsonl`), the OSC
status reports the TUIs printed, and pi's complete event order from a probe extension.

How it works: a fake `femmed` (a tiny Python recorder, written to a temp dir whose path contains
a space and a single quote, to prove the generated configs quote the path correctly) logs every
call (argv, stdin, FEMME_* env) to a JSONL file. The agents run with the configs the Rust
generators produce (`hooks::claude_settings_json`, `hooks::codex_hook_args` + `codex_notify_args`,
`hooks::pi_extension_source`, dumped by the ignored test `dump_hook_configs`), in a clean
environment with `FEMME_SID` set, in a PTY (the interactive TUI, as femmed runs them) or in print
mode. Every run is cheap and bounded (`RUN_LIMIT` seconds, then the whole process tree is killed).

    python3 crates/femme-agents/scripts/capture-hooks.py claude [--scenario print ...]
        print, stopfailure, permission, question, resume (default), osc7501 (on request);
        claude-haiku-5-5, one-word answers
    python3 crates/femme-agents/scripts/capture-hooks.py codex [--scenario main|approval]
        TUI with gpt-6-luna at low effort; ~/.codex/config.toml lines a run adds are restored
        (from an on-disk backup, `config.toml.femme-capture-bak`; see CodexHomeGuard)
    python3 crates/femme-agents/scripts/capture-hooks.py pi [--scenario print|tui|takeover]
        pi talks to a scripted OpenAI-compatible endpoint served by this script (isolated
        PI_CODING_AGENT_DIR, ~/.pi untouched); takeover needs tui
    options: --cwd DIR (a git repo the agents run in; default: a fresh temp repo)
             --claude-settings FILE|all / --codex-args JSON / --pi-extension FILE (skip the cargo dump)
             --out DIR (default: fixtures/hooks), --raw DIR (unscrubbed logs, PTY output; default temp)
             --keep EVENT (only write fixtures for these events)

Every run is a live agent session (Claude and Codex cost a little). Re-run it when a CLI version
is bumped and `git diff` the fixtures. Stdlib only.
"""
import argparse
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

CRATE = Path(__file__).resolve().parent.parent
RUN_LIMIT = 85  # seconds per live run (every run also sits under `timeout 90` semantics)

FAKE_FEMMED = r'''#!/usr/bin/env python3
# Fake femmed: records each call as one JSON line, prints NOTHING on stdout (Claude feeds
# SessionStart/UserPromptSubmit hook stdout to the model).
import json, os, select, sys, time
def read_stdin():
    try:
        if os.isatty(0):
            return None, "tty"
    except Exception:
        return None, "closed"
    data = b""
    try:
        r, _, _ = select.select([0], [], [], 2.0)
        if not r:
            return None, "no-data-2s"
        while True:
            chunk = os.read(0, 65536)
            if not chunk:
                break
            data += chunk
            r, _, _ = select.select([0], [], [], 2.0)
            if not r:
                return data.decode("utf-8", "replace"), "partial-timeout"
    except OSError as e:
        return None, "error:%s" % e
    return data.decode("utf-8", "replace"), "eof"
text, state = read_stdin()
secret = ("TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "COOKIE")
env, names = {}, []
for k, v in os.environ.items():
    if any(s in k.upper() for s in secret):
        continue
    if k.startswith(("FEMME_", "CLAUDE_PROJECT_DIR", "CODEX_THREAD_ID", "PI_CODING_AGENT")):
        env[k] = v
    if k.startswith(("CLAUDE", "CODEX", "PI_", "XDG_", "FEMME")):
        names.append(k)
rec = {"t": time.time(), "argv": sys.argv[1:], "stdin": text, "stdin_state": state,
       "cwd": os.getcwd(), "env": env, "env_names": sorted(names)}
log = os.environ.get("FAKE_FEMMED_LOG") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "calls.jsonl")
with open(log, "a", encoding="utf-8") as f:
    f.write(json.dumps(rec) + "\n")
'''


# ---------------------------------------------------------------- PTY driver

ANSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[PX^_][^\x1b]*\x1b\\|\x1b[@-Z\\-_]|\x1b[()][0-9A-Za-z]")


def clean_env(**extra):
    """A minimal environment like the holder gives a PTY (none of the caller's CLAUDECODE etc.)."""
    keep = ("HOME", "USER", "LOGNAME", "PATH", "SHELL", "LANG", "LC_ALL", "XDG_RUNTIME_DIR", "TMPDIR")
    env = {k: os.environ[k] for k in keep if k in os.environ}
    env.setdefault("LANG", "C.UTF-8")
    env["TERM"] = "xterm-256color"
    env["COLORTERM"] = "truecolor"
    env.update({k: str(v) for k, v in extra.items()})
    return env


def descendants(pid):
    kids = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat", "rb") as f:
                st = f.read()
            ppid = int(st[st.rindex(b")") + 2 :].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        kids.setdefault(ppid, []).append(int(d))
    out, todo = [], [pid]
    while todo:
        p = todo.pop()
        for c in kids.get(p, []):
            out.append(c)
            todo.append(c)
    return out


def _alive(p):
    try:
        with open(f"/proc/{p}/stat", "rb") as f:
            st = f.read()
        return st[st.rindex(b")") + 2 :][:1] != b"Z"
    except OSError:
        return False


class Pty:
    """An interactive program in a PTY. Answers the terminal queries TUIs wait for."""

    def __init__(self, argv, env, cwd, rows=40, cols=120, log_path=None, osc7501=False):
        self.osc7501 = osc7501  # answer the OSC 7501 program-status probe (Claude, Pi)
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.chdir(cwd)
                os.execvpe(argv[0], argv, env)
            finally:
                os._exit(127)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.raw = b""
        self.log = open(log_path, "wb") if log_path else None
        self.status = None

    def _answer(self, data):
        w = lambda b: os.write(self.fd, b)
        if b"\x1b[6n" in data:
            w(b"\x1b[1;1R")
        if b"\x1b[c" in data or b"\x1b[0c" in data:
            w(b"\x1b[?62;22c")
        if b"\x1b]10;?" in data:
            w(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\")
        if b"\x1b]11;?" in data:
            w(b"\x1b]11;rgb:0000/0000/0000\x1b\\")
        if b"\x1b[?u" in data:
            w(b"\x1b[?0u")
        if self.osc7501 and b"\x1b]7501;?" in data:
            w(b"\x1b]7501;?\x1b\\")

    def pump(self, secs, until=None):
        """Read output for up to `secs`; True as soon as `until(self)` holds."""
        end = time.time() + secs
        while True:
            if until and until(self):
                return True
            left = end - time.time()
            if left <= 0:
                return False
            r, _, _ = select.select([self.fd], [], [], min(left, 0.1))
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    data = b""
                if not data:
                    self.reap()
                    return bool(until and until(self))
                self.raw += data
                if self.log:
                    self.log.write(data)
                    self.log.flush()
                self._answer(data)

    def text(self, tail=None):
        b = self.raw if tail is None else self.raw[-tail:]
        return ANSI.sub(b"", b).decode("utf-8", "replace")

    def squashed(self, tail=None):
        """`text()` without any whitespace (TUIs position words with cursor moves, not spaces)."""
        return re.sub(r"\s+", "", self.text(tail))

    def send(self, s):
        os.write(self.fd, s.encode() if isinstance(s, str) else s)

    def type(self, s, delay=0.01):
        for ch in s:
            self.send(ch)
            time.sleep(delay)

    def reap(self):
        if self.status is None:
            try:
                pid, st = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.status = st
            except ChildProcessError:
                self.status = -1
        return self.status

    def alive(self):
        return self.reap() is None

    def kill(self, grace=2.0):
        """TERM the child and all its descendants, then KILL what is left."""
        pids = [self.pid] + descendants(self.pid)
        for sig in (signal.SIGTERM, signal.SIGKILL):
            for p in pids:
                try:
                    os.kill(p, sig)
                except ProcessLookupError:
                    pass
            end = time.time() + grace
            while time.time() < end and any(_alive(p) for p in pids):
                self.reap()
                time.sleep(0.05)
        self.reap()
        try:
            os.close(self.fd)
        except OSError:
            pass
        if self.log:
            self.log.close()


# ---------------------------------------------------------------- recorder log helpers


class Calls:
    """The fake femmed's JSONL log."""

    def __init__(self, path):
        self.path = Path(path)

    def all(self):
        if not self.path.exists():
            return []
        out = []
        for line in self.path.read_text(encoding="utf-8").splitlines():
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
        return out

    def payloads(self):
        """[(agent, event, payload_text, payload_json_or_None, record)]"""
        out = []
        for r in self.all():
            a = r.get("argv") or []
            if len(a) < 3 or a[0] != "hook":
                continue
            text = r.get("stdin") or ""
            if not text.strip() and len(a) > 3:
                text = a[-1]  # Codex notify: the JSON is the last argv element
            try:
                j = json.loads(text)
            except ValueError:
                j = None
            out.append((a[1], a[2], text, j, r))
        return out

    def has(self, event, **fields):
        for _, ev, _, j, _ in self.payloads():
            if ev == event and isinstance(j, dict) and all(j.get(k) == v for k, v in fields.items()):
                return True
        return False

    def count(self, event):
        return sum(1 for _, ev, _, _, _ in self.payloads() if ev == event)


def wait_for(p, secs, cond):
    """Pump the PTY until cond() holds."""
    return p.pump(secs, lambda _p: cond())


# ---------------------------------------------------------------- Claude

CLAUDE_MODEL = "claude-haiku-5-5"


def claude_tui(ctx, name, extra, steps, osc7501=False):
    """Run the interactive `claude` in a PTY with the hooks settings; `steps(p, calls)` drives it."""
    calls = Calls(ctx.raw / f"claude-{name}.calls.jsonl")
    calls.path.unlink(missing_ok=True)
    env = clean_env(FEMME_SID=f"cap-claude-{name}", FAKE_FEMMED_LOG=calls.path)
    argv = ["claude", "--model", CLAUDE_MODEL, "--settings", str(ctx.claude_settings)] + extra
    print(f"[claude:{name}] {' '.join(argv[:1] + argv[3:])}", flush=True)
    p = Pty(argv, env, ctx.cwd, log_path=ctx.raw / f"claude-{name}.pty.log", osc7501=osc7501)
    t0 = time.time()
    try:
        # First-run dialogs. Both default to the safe answer ("No, exit"), and no hook runs before
        # the workspace trust dialog is accepted.
        for _ in range(3):
            dialog = lambda: "Itrustthisfolder" in p.squashed(6000) or "Yes,Iaccept" in p.squashed(6000)
            if not wait_for(p, 25, lambda: calls.has("SessionStart") or dialog()) or calls.has("SessionStart"):
                break
            p.pump(0.5)
            p.send("\x1b[B")  # down to "Yes, I trust this folder" / "Yes, I accept"
            p.pump(0.3)
            p.send("\r")
            p.raw = b""
        p.pump(2.5)  # let the prompt box settle
        steps(p, calls, t0)
    finally:
        p.kill()
        print(f"[claude:{name}] {time.time() - t0:.1f}s, {len(calls.payloads())} hook calls", flush=True)
    return calls


def submit(p, text):
    p.type(text)
    p.pump(0.4)
    p.send("\r")


def claude_print(ctx, name="print", model=CLAUDE_MODEL,
                 prompt="Run `echo hi` with your Bash tool, then run `ls /nonexistent-femme-dir` with your Bash tool "
                        "(two separate calls), then reply with the single word done."):
    """Print mode under bypassPermissions: a tool call that works and one that fails, then exit
    (SessionEnd reason `other`). With a model that does not exist the API error ends the turn
    (StopFailure)."""
    calls = Calls(ctx.raw / f"claude-{name}.calls.jsonl")
    calls.path.unlink(missing_ok=True)
    env = clean_env(FEMME_SID=f"cap-claude-{name}", FAKE_FEMMED_LOG=calls.path)
    argv = ["claude", "-p", "--model", model, "--settings", str(ctx.claude_settings),
            "--permission-mode", "bypassPermissions", prompt]
    print(f"[claude:{name}] claude -p --model {model} ...", flush=True)
    try:
        r = subprocess.run(argv, env=env, cwd=ctx.cwd, stdin=subprocess.DEVNULL, capture_output=True, timeout=RUN_LIMIT)
        print(f"[claude:{name}] exit {r.returncode}: {(r.stdout + r.stderr).decode()[:120]!r}", flush=True)
    except subprocess.TimeoutExpired:
        print(f"[claude:{name}] timed out", flush=True)
    return calls


def claude_permission(ctx):
    """Manual permission mode: a Write permission dialog answered after the 6 s notification
    delay, then a foreground Bash call interrupted with Esc, then /exit. (Bash `echo` is
    auto-allowed even in manual mode, so the dialog comes from the Write tool.)"""

    def steps(p, calls, t0):
        submit(p, "Use your Write tool to create the file perm-test.txt containing the word hi, then reply "
                  "with the single word done.")
        wait_for(p, 30, lambda: calls.has("PermissionRequest"))
        wait_for(p, 9, lambda: calls.has("Notification", notification_type="permission_prompt"))
        p.pump(0.5)
        p.send("\r")  # 1. Yes
        wait_for(p, 30, lambda: calls.has("Stop"))
        p.pump(1.5)
        submit(p, "Run `python3 -c 'import time; time.sleep(20)'` with your Bash tool in the foreground, then "
                  "reply with the single word done.")
        wait_for(p, 30, lambda: calls.count("PreToolUse") >= 2)
        p.pump(2.0)
        p.send("\x1b")  # Esc: interrupt the running tool
        p.pump(4.0)
        submit(p, "/exit")
        wait_for(p, 10, lambda: calls.has("SessionEnd"))
        p.pump(1.0)
        (Path(ctx.cwd) / "perm-test.txt").unlink(missing_ok=True)

    return claude_tui(ctx, "permission", ["--permission-mode", "manual"], steps)


def claude_question(ctx):
    """Plan mode: an AskUserQuestion dialog, then an ExitPlanMode approval, then /exit."""

    prompt = ("This is a UI test. First use the AskUserQuestion tool to ask me 'A or B?' with the two "
              "options A and B. After I answer, call ExitPlanMode with the plan 'Reply with the single "
              "word ok.' After I approve, reply with the single word ok.")

    def steps(p, calls, t0):
        submit(p, prompt)
        wait_for(p, 40, lambda: calls.has("PreToolUse", tool_name="AskUserQuestion"))
        p.pump(8.0)  # what else fires while the dialog is up (Notification? PermissionRequest?)
        p.send("\r")  # first option
        p.pump(1.5)
        if "Submit" in p.squashed(3000):
            p.send("\r")
        wait_for(p, 40, lambda: calls.has("PreToolUse", tool_name="ExitPlanMode") or calls.has("Stop"))
        p.pump(8.0)
        if not calls.has("Stop"):
            p.send("\r")  # approve the plan
            wait_for(p, 30, lambda: calls.has("Stop"))
        p.pump(1.0)
        submit(p, "/exit")
        wait_for(p, 10, lambda: calls.has("SessionEnd"))
        p.pump(1.0)

    return claude_tui(ctx, "question", ["--permission-mode", "plan"], steps)


def claude_resume(ctx, sid):
    """`claude --resume <sid>` under bypassPermissions (how femmed runs takeover): an
    AskUserQuestion dialog (still asked under bypass?), the 60 s idle notification, /clear, /exit."""

    def steps(p, calls, t0):
        submit(p, "Use the AskUserQuestion tool to ask me 'A or B?' with the two options A and B, then reply "
                  "with the single word ok.")
        wait_for(p, 30, lambda: calls.has("PreToolUse", tool_name="AskUserQuestion"))
        p.pump(2.0)
        p.send("\r")  # first option
        p.pump(1.5)
        if "Submit" in p.squashed(3000):
            p.send("\r")
        wait_for(p, 30, lambda: calls.has("Stop"))
        wait_for(p, max(5, RUN_LIMIT - 10 - (time.time() - t0)), lambda: calls.has("Notification", notification_type="idle_prompt"))
        submit(p, "/clear")
        wait_for(p, 5, lambda: calls.has("SessionStart", source="clear"))
        p.pump(1.0)
        submit(p, "/exit")
        wait_for(p, 5, lambda: calls.count("SessionEnd") >= 2)
        p.pump(0.5)

    return claude_tui(ctx, "resume", ["--resume", sid, "--permission-mode", "bypassPermissions"], steps)


OSC7501 = re.compile(rb"\x1b\]7501;([^\x07\x1b]*)(?:\x07|\x1b\\)")


def osc7501_states(raw):
    """Decode `ESC ] 7501 ; state=..:app=..[:kind=..][:title=<b64>][:msg=<b64>] ST` sequences."""
    import base64
    out = []
    for m in OSC7501.finditer(raw):
        d = {}
        for part in m.group(1).decode("utf-8", "replace").split(":"):
            k, _, v = part.partition("=")
            if k in ("title", "msg"):
                try:
                    v = base64.b64decode(v).decode("utf-8", "replace")
                except ValueError:
                    pass
            d[k] = v
        out.append(d)
    return out


def claude_osc7501(ctx):
    """Not a hook: answer Claude's OSC 7501 (program status) probe and record what the TUI then
    emits, including around an Esc interrupt (which fires no hook). Written to
    `claude-osc7501-tui.jsonl` (one decoded sequence per line)."""

    def steps(p, calls, t0):
        submit(p, "Run `python3 -c 'import time; time.sleep(15)'` with your Bash tool in the foreground, then "
                  "reply with the single word done.")
        wait_for(p, 30, lambda: b"time.sleep(15)" in p.raw[-20000:] and p.raw.count(b"\x1b]7501;") >= 2)
        p.pump(4.0)
        p.send("\x1b")  # Esc while the tool runs
        p.pump(3.0)
        submit(p, "Reply with the single word ok.")
        wait_for(p, 30, lambda: calls.count("Stop") >= 1)
        p.pump(1.0)
        submit(p, "/exit")
        wait_for(p, 10, lambda: calls.has("SessionEnd"))
        p.pump(1.0)

    calls = claude_tui(ctx, "osc7501", ["--permission-mode", "bypassPermissions"], steps, osc7501=True)
    raw = (ctx.raw / "claude-osc7501.pty.log").read_bytes()
    write_evidence(ctx, "claude-osc7501-tui.jsonl", osc7501_states(raw))
    return calls


CLAUDE_EVENTS = ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PostToolBatch", "Notification", "UserPromptSubmit",
                 "UserPromptExpansion", "SessionStart", "SessionEnd", "Stop", "StopFailure", "SubagentStart", "SubagentStop",
                 "PreCompact", "PostCompact", "PreModelSwitch", "PostModelSwitch", "PermissionRequest", "PermissionDenied",
                 "Setup", "TeammateIdle", "TaskCreated", "TaskCompleted", "Elicitation", "ElicitationResult", "ConfigChange",
                 "InstructionsLoaded", "CwdChanged", "DirectoryAdded", "MessageDisplay"]


def claude_all_events_settings(ctx):
    """Exploration config: every hook event 2.1.302 knows (minus Worktree*/FileChanged), exec form."""
    hooks = {e: [{"hooks": [{"type": "command", "command": str(ctx.fake_femmed), "args": ["hook", "claude", e], "timeout": 10}]}]
             for e in CLAUDE_EVENTS}
    path = ctx.raw / "claude-all-events.json"
    path.write_text(json.dumps({"hooks": hooks}, indent=1))
    return path


def variant_claude(event, j):
    for k in {"Notification": "notification_type", "SessionStart": "source", "SessionEnd": "reason",
              "StopFailure": "error"}.get(event, "tool_name"), "tool_name":
        if isinstance(j, dict) and isinstance(j.get(k), str):
            return j[k]
    return None


def capture_claude(ctx, scenarios):
    runs = []
    if "print" in scenarios:
        runs.append(claude_print(ctx))
    if "stopfailure" in scenarios:
        runs.append(claude_print(ctx, "stopfailure", model="claude-nonexistent-5-5", prompt="Reply with the single word ok."))
    if "permission" in scenarios:
        runs.append(claude_permission(ctx))
    if "question" in scenarios:
        runs.append(claude_question(ctx))
    if "osc7501" in scenarios:
        runs.append(claude_osc7501(ctx))
    if "resume" in scenarios:
        sid = ctx.claude_resume
        for c in runs:
            for _, ev, _, j, _ in c.payloads():
                if ev == "SessionStart" and isinstance(j, dict):
                    sid = sid or j.get("session_id")
        if sid:
            runs.append(claude_resume(ctx, sid))
        else:
            print("[claude:resume] skipped: no session id from an earlier scenario", flush=True)
    for c in runs:
        write_fixtures(ctx, c, variant_claude, c.path.name.split(".")[0].split("-", 1)[1])


# ---------------------------------------------------------------- Codex

#: The cheapest model `model/list` offers, at low effort.
CODEX_MODEL_ARGS = ["-m", "gpt-6-luna", "-c", 'model_reasoning_effort="low"']
#: In-process app-server (hooks are children of the TUI and inherit FEMME_SID), OSC 9 notifications
#: in the PTY whatever the focus, no update banner.
CODEX_TUI_ARGS = ["--no-daemon", "-c", "tui.notifications=true", "-c", 'tui.notification_method="osc9"',
                  "-c", 'tui.notification_condition="always"', "-c", "check_for_update_on_startup=false"]
OSC9 = re.compile(rb"\x1b\]9;([^\x07\x1b]*)(?:\x07|\x1b\\)")
#: Lines a TUI run may add to ~/.codex/config.toml (folder or hook trust, tui flags).
CODEX_OURS = re.compile(r"^\s*(\[tui\]|\[hooks\.state.*\]|\[projects\..*\]|screen_reader_detection_done\s*=.*|"
                        r"trusted_hash\s*=.*|trust_level\s*=.*|enabled\s*=.*)?\s*$")


CODEX_BACKUP = ".femme-capture-bak"


class CodexHomeGuard:
    """Undo what a TUI run adds to ~/.codex (config.toml, hooks.json: the user's real files).

    Before the run each existing file is copied to `<file>.femme-capture-bak`, on disk, so a crash of
    this script cannot lose the original; a backup left by an earlier crashed run stops the capture
    (it holds the user's file, and overwriting it with an already-modified copy would lose it).
    After the run the file is put back, by atomic rename, only when the run changed nothing but
    adding lines that match CODEX_OURS (and no other project's): a removed or changed line was
    someone else's edit (another process may be writing there too) and must not be rolled back.
    Then the backup is deleted. In every other case the file is left as it is, the backup stays next
    to it, and the script says so."""

    def __init__(self, cwd):
        self.cwd = str(cwd)

    def __enter__(self):
        home = Path.home() / ".codex"
        files = [home / "config.toml", home / "hooks.json"]
        for f in files:
            bak = f.with_name(f.name + CODEX_BACKUP)
            if bak.exists():
                raise RuntimeError(f"{bak} exists: an earlier capture died before restoring {f}. Compare the two, "
                                   f"put the right content back in {f.name}, delete the backup, run again.")
        self.snap = {}
        for f in files:
            if f.exists():
                bak = f.with_name(f.name + CODEX_BACKUP)
                shutil.copy2(f, bak)
                self.snap[f] = (f.read_text(), bak)
            else:
                self.snap[f] = (None, None)
        return self

    def __exit__(self, *exc):
        for f, (before, bak) in self.snap.items():
            now = f.read_text() if f.exists() else None
            if now == before:
                if bak:
                    bak.unlink(missing_ok=True)
                continue
            # ponytail: a line-set diff (order and repeated lines ignored); parse the TOML if a run ever
            # needs to add a key to a table the user already has.
            old, new = (before or "").splitlines(), (now or "").splitlines()
            added = [l for l in new if l not in old]
            removed = [l for l in old if l not in new]
            foreign = [l for l in added if l.strip().startswith("[projects.") and self.cwd not in l]
            if before is not None and now is not None and not removed and not foreign \
                    and all(CODEX_OURS.match(l) for l in added):
                tmp = f.with_name(f.name + ".femme-capture-tmp")
                tmp.write_text(before)
                shutil.copymode(bak, tmp)
                os.replace(tmp, f)
                bak.unlink(missing_ok=True)
                print(f"  restored {f} (the run added {[l for l in added if l.strip()]})", flush=True)
            else:
                kept = f"; the original is in {bak}" if bak else ""
                print(f"  WARNING: {f} changed in ways this script does not undo; left as is: added {added}, "
                      f"removed {removed}{kept}", flush=True)
        return False


def codex_tui(ctx, name, flags, steps):
    """Run the interactive `codex` in a PTY with the generated hook args; `steps(p, calls)` drives it."""
    calls = Calls(ctx.raw / f"codex-{name}.calls.jsonl")
    calls.path.unlink(missing_ok=True)
    env = clean_env(FEMME_SID=f"cap-codex-{name}", FAKE_FEMMED_LOG=calls.path)
    trust = ["-c", "projects={%s={trust_level=\"trusted\"}}" % json.dumps(str(ctx.cwd))]  # this session only
    argv = ["codex"] + flags + CODEX_TUI_ARGS + CODEX_MODEL_ARGS + ctx.codex_args + trust
    print(f"[codex:{name}] codex {' '.join(flags)} ... ({len(argv)} args)", flush=True)
    t0 = time.time()
    with CodexHomeGuard(ctx.cwd):
        p = Pty(argv, env, ctx.cwd, log_path=ctx.raw / f"codex-{name}.pty.log")
        try:
            # The composer takes keys at once, but a startup dialog may pop up over it seconds later
            # and take Enter, digits and letters: the hooks review dialog, raised by the user's own
            # untrusted hooks (ours are trusted by hooks.state). Esc skips it and writes nothing.
            wait_for(p, 30, lambda: "AskCodextodoanything" in p.squashed(8000))
            if wait_for(p, 15, lambda: "Hooksneedreview" in p.squashed(8000)):
                p.pump(1.0)
                print(f"[codex:{name}] 'Hooks need review' dialog -> Esc", flush=True)
                p.send("\x1b")
                p.pump(2.0)
            steps(p, calls)
        finally:
            osc9 = [m.decode("utf-8", "replace") for m in OSC9.findall(p.raw)]
            p.kill()
            print(f"[codex:{name}] {time.time() - t0:.1f}s, {len(calls.payloads())} hook calls, OSC 9: {osc9}", flush=True)
    write_evidence(ctx, f"codex-osc9-{name}.jsonl", [{"text": t} for t in osc9])
    return calls


def codex_first_prompt(p, calls, text):
    """Submit the first prompt and make sure the composer took it (UserPromptSubmit). If not, a
    dialog has the keys: back out with Esc and stop, before more keys change the user's config."""
    submit(p, text)
    if not wait_for(p, 25, lambda: calls.has("UserPromptSubmit")):
        for _ in range(3):
            p.send("\x1b")
            p.pump(0.5)
        raise RuntimeError("codex did not take the first prompt (a dialog?): " + p.squashed(1500)[-300:])


def codex_main(ctx):
    """FEMME's own mode (--dangerously-bypass-approvals-and-sandbox): `echo hi`, /clear, `sleep 30`
    interrupted with Esc, Shift+Tab plan mode and a question (request_user_input), the answer typed
    into the composer, /quit."""

    def steps(p, calls):
        asked = lambda: any(isinstance(j, dict) and str(j.get("tool_name", "")).startswith("request_user_input")
                            for _, ev, _, j, _ in calls.payloads() if ev == "PreToolUse")
        codex_first_prompt(p, calls, "Run `echo hi` with your shell tool, then reply with the single word done.")
        wait_for(p, 45, lambda: calls.has("Stop"))
        p.pump(2.0)
        submit(p, "/clear")
        p.pump(2.5)
        n = calls.count("PreToolUse")
        submit(p, "Run `sleep 30` with your shell tool, then reply with the single word done.")
        wait_for(p, 35, lambda: calls.count("PreToolUse") > n)
        p.pump(1.5)
        p.send("\x1b")  # Esc: Interrupt (the command goes on as a background terminal)
        wait_for(p, 10, lambda: calls.has("Interrupt"))
        p.pump(2.0)
        p.send("\x1b[Z")  # Shift+Tab: plan mode
        p.pump(1.5)
        stops = calls.count("Stop")
        submit(p, "Use your question tool (request_user_input) to ask me exactly one question, 'Pick a color', "
                  "with the options red and blue. After I answer, reply with only the word ok. Do not write a plan.")
        wait_for(p, 45, asked)
        wait_for(p, 8, lambda: calls.count("Stop") > stops)  # the async question ends the turn
        stops, prompts = calls.count("Stop"), calls.count("UserPromptSubmit")
        submit(p, "red")
        if not wait_for(p, 8, lambda: calls.count("UserPromptSubmit") > prompts):
            p.send("1")  # the question panel took the keys instead of the composer
            p.pump(0.5)
            p.send("\r")
        wait_for(p, 40, lambda: calls.count("Stop") > stops)
        p.pump(2.0)
        if "Implementthisplan" in p.squashed(4000):
            p.send("\x1b")
            p.pump(1.0)
        submit(p, "/quit")
        wait_for(p, 15, lambda: not p.alive() and calls.has("SessionEnd"))

    return codex_tui(ctx, "main", ["--dangerously-bypass-approvals-and-sandbox"], steps)


def codex_approval(ctx):
    """`-a on-request -s read-only`: a command asking for escalated permissions opens the approval
    overlay, Enter approves it, then SIGTERM to codex (which fires no SessionEnd)."""

    def steps(p, calls):
        # Ask for escalation up front: a sandboxed command may fail (bwrap) instead of asking.
        codex_first_prompt(p, calls, "Run `touch femme-approval-test.txt` with your shell tool and request "
                                     "escalated permissions for it (sandbox_permissions require_escalated, "
                                     "justification: approval test). Then reply with the single word done.")
        if wait_for(p, 60, lambda: calls.has("PermissionRequest") or "Wouldyouliketorun" in p.squashed(6000)):
            p.pump(2.0)
            p.send("\r")  # 1. Yes, proceed
        wait_for(p, 40, lambda: calls.has("Stop"))
        p.pump(2.0)
        os.kill(p.pid, signal.SIGTERM)
        wait_for(p, 8, lambda: not p.alive())
        p.pump(2.0)
        (Path(ctx.cwd) / "femme-approval-test.txt").unlink(missing_ok=True)

    return codex_tui(ctx, "approval", ["-a", "on-request", "-s", "read-only"], steps)


CODEX_TITLE_PROMPT = "Generate a concise, single-line task title"


def variant_codex(event, j):
    j = j if isinstance(j, dict) else {}
    if event == "SessionStart":
        return j.get("source")
    if event == "SessionEnd":
        return j.get("reason")
    if event in ("PreToolUse", "PostToolUse", "PermissionRequest"):
        return j.get("tool_name")
    if event == "Stop":
        return "question" if j.get("last_assistant_message") is None else None
    if event == "notify":
        first = (j.get("input-messages") or [""])[0]
        if isinstance(first, str) and first.startswith(CODEX_TITLE_PROMPT):
            return "title"  # Codex's internal title-generation turn
        return "question" if j.get("last-assistant-message") is None else None
    return None


def capture_codex(ctx, scenarios):
    (ctx.out / "raw" / "codex-calls.jsonl").unlink(missing_ok=True)
    runs = []
    if "main" in scenarios:
        runs.append(("main", codex_main(ctx)))
    if "approval" in scenarios:
        runs.append(("approval", codex_approval(ctx)))
    for name, c in runs:
        write_fixtures(ctx, c, variant_codex, name)


# ---------------------------------------------------------------- Pi
#
# pi has no credentials on the capture host, so it talks to a scripted OpenAI-compatible endpoint
# served here (provider `mock`, model `mock-a`) through an isolated PI_CODING_AGENT_DIR (~/.pi is not
# touched). Everything else is the real pi: TUI, agent loop, extensions, sessions, signals.

#: Test-only tool whose execute() opens a blocking ctx.ui.select (the TUI runs load it with a 2nd -e).
PI_ASK_EXTENSION = r'''// @ts-nocheck
import { Type } from "typebox";

export default function (pi) {
  pi.registerTool({
    name: "ask_choice",
    label: "Ask choice",
    description: "Ask the user to pick one of several options. Returns the picked option.",
    parameters: Type.Object({ question: Type.String(), options: Type.Array(Type.String()) }),
    async execute(_id, params, _signal, _onUpdate, ctx) {
      const choice = await ctx.ui.select(params.question, params.options);
      return { content: [{ type: "text", text: choice ?? "(cancelled)" }], details: {} };
    },
  });
}
'''

#: Test-only probe: appends EVERY extension event (not only the hooked ones) to $FEMME_PROBE_LOG,
#: so the raw evidence shows the complete order, and a new pi version's new events show up.
PI_PROBE_EXTENSION = r"""// @ts-nocheck
import { appendFileSync } from "node:fs";

const LOG = process.env.FEMME_PROBE_LOG;
const EVENTS = ["session_start", "session_shutdown", "session_before_switch", "session_before_fork",
  "session_before_compact", "session_compact", "session_compact_failed", "session_before_tree", "session_tree",
  "input", "before_agent_start", "agent_start", "turn_start", "turn_end", "message_start", "message_end",
  "tool_call", "tool_result", "tool_execution_start", "tool_execution_end", "agent_end", "agent_before_settle",
  "agent_settled", "ui_prompt_start", "ui_prompt_end", "model_select", "thinking_level_select", "user_bash",
  "resources_discover", "mcp_servers_change", "project_trust"];
const FIELDS = ["reason", "previousSessionFile", "targetSessionFile", "turnIndex", "aborted", "kind", "title",
  "toolName", "toolCallId", "messageType", "isError", "outcome", "continue"];
let n = 0;

function rec(type, event, ctx) {
  if (!LOG) return;
  const get = (f) => { try { const v = f(); return v === undefined ? null : v; } catch { return "<stale ctx>"; } };
  const r = { n: ++n, t: Date.now(), type };
  if (ctx) Object.assign(r, { mode: get(() => ctx.mode), idle: get(() => ctx.isIdle()),
    session_id: get(() => ctx.sessionManager.getSessionId()) });
  for (const k of FIELDS) if (event && k in event && typeof event[k] !== "object") r[k] = event[k];
  if (event?.message?.role) r.role = event.message.role;
  if (event?.message?.stopReason) r.stopReason = event.message.stopReason;
  if (Array.isArray(event?.messages)) r.messages = event.messages.map((m) => m.role + (m.stopReason ? ":" + m.stopReason : ""));
  try { appendFileSync(LOG, JSON.stringify(r) + "\n"); } catch {}
}

export default function (pi) {
  for (const ev of EVENTS) pi.on(ev, (event, ctx) => { rec(ev, event, ctx); });
}
"""

PI_OK = "reply with the single word ok"
PI_ASK = "pick a color for me using the ask_choice tool"
PI_SLOW = "count slowly from 1 to 40"


def pi_script(req):
    """What the mock model answers to one chat-completions request: [("text", s) | ("sleep", secs) |
    ("tool", call_id, name, json_args)]."""
    def text(m):
        c = m.get("content")
        if isinstance(c, list):
            return "".join(x.get("text", "") for x in c if isinstance(x, dict))
        return c if isinstance(c, str) else ""

    msgs = req.get("messages") or []
    last = msgs[-1] if msgs else {}
    users = [text(m) for m in msgs if m.get("role") == "user"]
    asked = (users[-1] if users else "").lower()
    if last.get("role") == "tool":
        return [("text", "You picked %s." % text(last).strip())]
    if "count slowly" in asked:
        return [s for i in range(1, 41) for s in (("text", "%d\n" % i), ("sleep", 0.3))]
    if "pick a color" in asked:
        return [("tool", "call_ask_1", "ask_choice", '{"question":"Pick a color","options":["red","blue"]}')]
    return [("text", "ok")]


class PiMock:
    """Scripted OpenAI-compatible endpoint (POST .../chat/completions, SSE) on 127.0.0.1."""

    def __init__(self):
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("content-length") or 0))
                try:
                    req = json.loads(body)
                except ValueError:
                    req = {}
                PiMock.serve(self, req)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    @staticmethod
    def serve(h, req):
        h.send_response(200)
        h.send_header("content-type", "text/event-stream")
        h.send_header("cache-control", "no-cache")
        h.end_headers()

        def chunk(delta=None, finish=None, usage=None):
            c = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": 1700000000,
                 "model": req.get("model", "mock"),
                 "choices": [] if usage else [{"index": 0, "delta": delta or {}, "finish_reason": finish}]}
            if usage:
                c["usage"] = usage
            h.wfile.write(b"data: " + json.dumps(c).encode() + b"\n\n")
            h.wfile.flush()

        try:
            chunk({"role": "assistant", "content": ""})
            tool = False
            for st in pi_script(req):
                if st[0] == "sleep":
                    time.sleep(st[1])
                elif st[0] == "text":
                    chunk({"content": st[1]})
                else:
                    tool = True
                    chunk({"tool_calls": [{"index": 0, "id": st[1], "type": "function",
                                           "function": {"name": st[2], "arguments": st[3]}}]})
            chunk(finish="tool_calls" if tool else "stop")
            chunk(usage={"prompt_tokens": 1000, "completion_tokens": 5, "total_tokens": 1005})
            h.wfile.write(b"data: [DONE]\n\n")
            h.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # pi aborted the request

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()


def pi_env(ctx, name, **extra):
    return clean_env(FEMME_SID=f"cap-pi-{name}", FAKE_FEMMED_LOG=ctx.raw / f"pi-{name}.calls.jsonl",
                     FEMME_PROBE_LOG=ctx.raw / f"pi-{name}.probe.jsonl", PI_CODING_AGENT_DIR=ctx.pi_dir,
                     PI_OFFLINE="1", PI_TELEMETRY="0", PI_SKIP_VERSION_CHECK="1", **extra)


def pi_argv(ctx, tui):
    argv = ["pi", "--model", "mock/mock-a", "-e", str(ctx.pi_extension)]
    return argv + (["-e", str(ctx.pi_ask)] if tui else []) + ["-e", str(ctx.pi_probe)]


def pi_print(ctx):
    """`pi -p`: session_start, agent_start, turn_start, agent_settled, session_shutdown."""
    calls = Calls(ctx.raw / "pi-print.calls.jsonl")
    calls.path.unlink(missing_ok=True)
    print("[pi:print] pi -p ...", flush=True)
    try:
        r = subprocess.run(pi_argv(ctx, tui=False) + ["-p", PI_OK], env=pi_env(ctx, "print"), cwd=ctx.cwd,
                           stdin=subprocess.DEVNULL, capture_output=True, timeout=RUN_LIMIT)
        print(f"[pi:print] exit {r.returncode}: {(r.stdout + r.stderr).decode()[:120]!r}", flush=True)
    except subprocess.TimeoutExpired:
        print("[pi:print] timed out", flush=True)
    time.sleep(0.5)  # the detached femmed children may still be writing
    pi_evidence(ctx, "print", b"")
    return calls


def pi_tui(ctx, name, extra, steps):
    """The pi TUI in a PTY (PI_PROGRAM_STATUS=1: OSC 7501 reports on); `steps(p, calls)` drives it."""
    calls = Calls(ctx.raw / f"pi-{name}.calls.jsonl")
    calls.path.unlink(missing_ok=True)
    (ctx.raw / f"pi-{name}.probe.jsonl").unlink(missing_ok=True)
    print(f"[pi:{name}] pi {' '.join(extra)} ...", flush=True)
    p = Pty(pi_argv(ctx, tui=True) + extra, pi_env(ctx, name, PI_PROGRAM_STATUS="1"), ctx.cwd,
            log_path=ctx.raw / f"pi-{name}.pty.log")
    t0 = time.time()
    try:
        wait_for(p, 30, lambda: calls.has("session_start"))
        p.pump(1.5)  # let the editor take focus
        steps(p, calls)
    finally:
        p.kill()
        time.sleep(0.5)  # the detached femmed children may still be writing
        st = p.status
        code = os.waitstatus_to_exitcode(st) if st not in (None, -1) else st
        print(f"[pi:{name}] {time.time() - t0:.1f}s, exit {code}, {len(calls.payloads())} hook calls", flush=True)
    pi_evidence(ctx, name, p.raw)
    return calls


def pi_evidence(ctx, name, raw):
    """Every pi event in order (from the probe) and the OSC 7501 reports, scrubbed, into <out>/raw/."""
    probe = Calls(ctx.raw / f"pi-{name}.probe.jsonl").all()
    t0 = probe[0]["t"] if probe else 0
    write_evidence(ctx, f"pi-events-{name}.jsonl", [dict(r, t=r["t"] - t0) for r in probe])
    if raw:
        write_evidence(ctx, f"pi-osc7501-{name}.jsonl", osc7501_states(raw))


def pi_session(ctx):
    """A prompt, a prompt whose tool opens a select dialog (Enter picks "red"), then /quit."""

    def steps(p, calls):
        submit(p, PI_OK)
        wait_for(p, 40, lambda: calls.count("agent_settled") >= 1)
        submit(p, PI_ASK)
        if wait_for(p, 40, lambda: calls.has("ui_prompt_start")):
            p.pump(1.0)
            p.send("\r")
        wait_for(p, 40, lambda: calls.count("agent_settled") >= 2)
        p.pump(0.5)
        submit(p, "/quit")
        wait_for(p, 10, lambda: not p.alive())

    return pi_tui(ctx, "tui", [], steps)


def pi_takeover(ctx, session_file):
    """`pi --session <file>` (femmed's takeover): a slow answer, SIGTERM to pi in the middle of it."""

    def steps(p, calls):
        submit(p, PI_SLOW)
        wait_for(p, 30, lambda: calls.has("turn_start"))
        p.pump(1.5)
        os.kill(p.pid, signal.SIGTERM)
        wait_for(p, 10, lambda: not p.alive())

    return pi_tui(ctx, "takeover", ["--session", session_file], steps)


def capture_pi(ctx, scenarios):
    mock = PiMock()
    ctx.pi_dir = ctx.raw / "pi-agent"
    ctx.pi_dir.mkdir(parents=True, exist_ok=True)
    model = {"id": "mock-a", "name": "Mock A", "reasoning": False, "input": ["text"], "contextWindow": 128000,
             "maxTokens": 4096, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}
    (ctx.pi_dir / "models.json").write_text(json.dumps({"providers": {"mock": {
        "baseUrl": f"http://127.0.0.1:{mock.port}/v1", "api": "openai-completions", "apiKey": "mock",
        "models": [model]}}}, indent=2))
    ctx.pi_ask = ctx.fake_femmed.parent / "femme-test-ask.ts"
    ctx.pi_ask.write_text(PI_ASK_EXTENSION)
    ctx.pi_probe = ctx.fake_femmed.parent / "femme-test-probe.ts"
    ctx.pi_probe.write_text(PI_PROBE_EXTENSION)
    cwd = str(ctx.cwd)
    ctx.extra_scrub += [(str(ctx.pi_dir), os.path.expanduser("~/.pi/agent")),
                        ("--" + re.sub(r"[/\\:]", "-", cwd.lstrip("/")) + "--", "--work-repo--"),
                        (f"127.0.0.1:{mock.port}", "127.0.0.1:47123")]
    (ctx.out / "raw" / "pi-calls.jsonl").unlink(missing_ok=True)
    runs = []
    try:
        if "print" in scenarios:
            runs.append(("print", pi_print(ctx)))
        if scenarios & {"tui", "takeover"}:
            tui = pi_session(ctx)
            runs.append(("tui", tui))
            files = [j.get("session_file") for _, ev, _, j, _ in tui.payloads()
                     if ev == "session_start" and isinstance(j, dict)]
            if "takeover" in scenarios and files and files[0]:
                runs.append(("takeover", pi_takeover(ctx, files[0])))
    finally:
        mock.close()
    for name, c in runs:
        write_fixtures(ctx, c, lambda event, j, name=name: name, name)


# ---------------------------------------------------------------- fixtures


class Ctx:
    pass


def scrubber(ctx):
    home = os.path.expanduser("~")
    cwd = str(ctx.cwd)
    mangled = re.sub(r"[^A-Za-z0-9]", "-", cwd)
    reps = ctx.extra_scrub + [(str(ctx.fake_femmed), "/usr/local/bin/femmed"), (cwd, "/work/repo"),
                              (mangled, "-work-repo"), (os.path.realpath(cwd), "/work/repo"),
                              (str(ctx.raw), "/work/raw"), (str(ctx.tmp), "/tmp/femme-hooks"), (home, "/home/user")]
    user = os.environ.get("USER") or os.path.basename(home)

    def scrub(s):
        for a, b in reps:
            s = s.replace(a, b)
        s = s.replace(user, "user")
        s = re.sub(r"/tmp/claude-\d+/", "/tmp/claude/", s)  # Claude's scratchpad dir carries the capturing uid
        s = re.sub(r'(command\\*"\s*:\s*\\*")rtk ', r"\1", s)  # an RTK hook on the capturing machine rewrote `ls` to `rtk ls`
        s = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", s)
        return s

    return scrub


def write_fixtures(ctx, calls, variant, run):
    """One fixture per (agent, event, variant), the first payload seen wins; every call of the run
    (time, argv, stdin, FEMME_SID) goes to `<out>/raw/<agent>-<run>.calls.jsonl`."""
    scrub = scrubber(ctx)
    recs = calls.all()
    if recs:
        agent = (recs[0].get("argv") or ["", "x"])[1]
        rows = [{"run": run, "t": round(r["t"] - recs[0]["t"], 3), "argv": r.get("argv"), "stdin": r.get("stdin"),
                 "stdin_state": r.get("stdin_state"),
                 "env": {"FEMME_SID": (r.get("env") or {}).get("FEMME_SID")}} for r in recs]
        write_evidence(ctx, f"{agent}-{run}.calls.jsonl", rows)
    for agent, event, text, j, _ in calls.payloads():
        if ctx.keep and event not in ctx.keep:
            continue
        v = variant(event, j)
        name = f"{agent}-{event}" + (f"-{v}" if v else "") + ".json"
        path = ctx.out / re.sub(r"[^A-Za-z0-9_.-]", "_", name)
        if path.name in ctx.written:
            continue
        ctx.written.add(path.name)
        path.write_text(scrub(text.rstrip("\n")) + "\n", encoding="utf-8")
        print(f"  wrote {path.relative_to(CRATE) if path.is_relative_to(CRATE) else path}", flush=True)


def write_evidence(ctx, name, rows):
    """Scrubbed JSONL into `<out>/raw/` (call order, OSC reports, pi's full event order)."""
    d = ctx.out / "raw"
    d.mkdir(parents=True, exist_ok=True)
    scrub = scrubber(ctx)
    (d / name).write_text("".join(scrub(json.dumps(r, ensure_ascii=False)) + "\n" for r in rows), encoding="utf-8")
    print(f"  wrote raw/{name} ({len(rows)} lines)", flush=True)


def dump_configs(ctx):
    """Ask the Rust generators for the configs (ignored test `dump_hook_configs`)."""
    d = ctx.raw / "configs"
    d.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, FEMME_HOOKS_DUMP=str(d), FEMME_HOOKS_FEMMED=str(ctx.fake_femmed))
    subprocess.run(["cargo", "test", "-q", "-p", "femme-agents", "--test", "hooks", "--", "--ignored", "--exact",
                    "dump_hook_configs"], cwd=CRATE, env=env, check=True)
    return d


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("agent", choices=["claude", "codex", "pi", "all"])
    ap.add_argument("--scenario", action="append", help="subset of scenarios (default: all)")
    ap.add_argument("--cwd", help="git repo to run the agents in (default: fresh temp repo)")
    ap.add_argument("--out", default=str(CRATE / "fixtures" / "hooks"))
    ap.add_argument("--raw", help="keep raw logs here (default: a temp dir, printed)")
    ap.add_argument("--claude-settings", help="settings file, or 'all' for every hook event (exploration)")
    ap.add_argument("--claude-resume", help="session id for the resume scenario (default: from the print run)")
    ap.add_argument("--codex-args", help="JSON list of extra codex args")
    ap.add_argument("--pi-extension")
    ap.add_argument("--keep", action="append", help="only write fixtures for these event names")
    a = ap.parse_args()

    ctx = Ctx()
    ctx.out = Path(a.out)
    ctx.out.mkdir(parents=True, exist_ok=True)
    ctx.raw = Path(a.raw or tempfile.mkdtemp(prefix="femme-hooks-raw-"))
    ctx.raw.mkdir(parents=True, exist_ok=True)
    ctx.keep = set(a.keep or [])
    ctx.written = set()
    ctx.extra_scrub = []
    tmp = ctx.tmp = Path(tempfile.mkdtemp(prefix="femme-hooks-"))
    bindir = tmp / "bin dir" / "it's here"  # space + quote: the configs must quote the path
    bindir.mkdir(parents=True)
    ctx.fake_femmed = bindir / "femmed"
    ctx.fake_femmed.write_text(FAKE_FEMMED)
    ctx.fake_femmed.chmod(0o755)
    if a.cwd:
        ctx.cwd = Path(a.cwd).resolve()
    else:
        ctx.cwd = tmp / "repo"
        ctx.cwd.mkdir()
        subprocess.run(["git", "init", "-q"], cwd=ctx.cwd, check=True)
        subprocess.run(["git", "-c", "user.name=x", "-c", "user.email=x@x", "commit", "-q", "--allow-empty", "-m", "init"],
                       cwd=ctx.cwd, check=True)
    need_dump = (a.agent in ("claude", "all") and not a.claude_settings) or \
                (a.agent in ("codex", "all") and not a.codex_args) or (a.agent in ("pi", "all") and not a.pi_extension)
    cfg = dump_configs(ctx) if need_dump else None
    if a.claude_settings == "all":
        ctx.claude_settings = claude_all_events_settings(ctx)
    else:
        ctx.claude_settings = Path(a.claude_settings).resolve() if a.claude_settings else (cfg / "claude-settings.json" if cfg else None)
    ctx.claude_resume = a.claude_resume
    ctx.codex_args = json.loads(a.codex_args) if a.codex_args else (json.loads((cfg / "codex-args.json").read_text()) if cfg else None)
    ctx.pi_extension = Path(a.pi_extension).resolve() if a.pi_extension else (cfg / "femme-hooks.ts" if cfg else None)
    print(f"raw logs: {ctx.raw}", flush=True)

    def stop(*_):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)  # once: let the cleanup (kill, ~/.codex restore) finish
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    agents = ["claude", "codex", "pi"] if a.agent == "all" else [a.agent]
    for ag in agents:
        if ag == "claude":
            capture_claude(ctx, set(a.scenario or ["print", "stopfailure", "permission", "question", "resume"]))
        elif ag == "codex":
            capture_codex(ctx, set(a.scenario or ["main", "approval"]))
        else:
            capture_pi(ctx, set(a.scenario or ["print", "tui", "takeover"]))
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()

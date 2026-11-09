#!/usr/bin/env python3
"""Summarises how the user uses Claude Code, from the transcripts in ~/.claude/projects (GBs of
jsonl), so the scout reads a page instead. Stdlib only; files are streamed line by line and the
big lines that carry nothing we count (tool results, attachments) are skipped before JSON parsing.

  digest.py [--root ~/.claude/projects] [--days N] [--samples 60] > digest.md

Progress goes to stderr every 25 files.
"""
import argparse
import collections
import datetime as dt
import json
import os
import random
import re
import sys
import time

CMD = re.compile(r"<command-name>/?([^<\s]+)</command-name>")
ARGS = re.compile(r"<command-args>([^<]*)</command-args>")
WORD = re.compile(r"[a-zA-Z][a-zA-Z'-]{2,}")
STOP = set("""the and for that this with you are but not was have has had what when where which who how why
can could would should will just like then than them they their there here from into onto about also its it's
i'm don't dont doesn't can't isn't it'll you're let's yes yeah okay ok all any some one two out get got make
made use used using way more most much very only even still now new old see look want need know think does did
done too our ours your yours his her him she he we us me my mine be is am an as at by if in of on or so to up
no do go it a i please thanks thank sure same other each every then them those these after before again over
into been being because while just let content image pasted text lines""".split())
PLACEHOLDER = re.compile(r"\[(?:Image|Pasted text) #\d+[^\]]*\]")
# Lines that are not a human turn: tool output, injected reminders, task notifications.
NOT_HUMAN = ("<task-notification", "<system-reminder", "<local-command", "Caveat:", "[Request interrupted",
             "<bash-", "<command-message", "This session is being continued")


def walk(root):
    for dirpath, _, files in os.walk(root):
        for f in files:
            if f.endswith(".jsonl"):
                yield os.path.join(dirpath, f)


def project_of(root, path):
    rel = os.path.relpath(path, root).split(os.sep)
    return rel[0]


def human_text(msg):
    c = msg.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        parts = [x.get("text", "") for x in c if isinstance(x, dict) and x.get("type") == "text"]
        if parts and not any(isinstance(x, dict) and x.get("type") == "tool_result" for x in c):
            return "\n".join(parts)
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.expanduser("~/.claude/projects"))
    ap.add_argument("--days", type=int, default=0, help="only files modified in the last N days (0: all)")
    ap.add_argument("--samples", type=int, default=60, help="sample human prompts to print")
    a = ap.parse_args()

    files = sorted(walk(a.root))
    if a.days:
        cutoff = time.time() - a.days * 86400
        files = [f for f in files if os.path.getmtime(f) >= cutoff]
    total_bytes = sum(os.path.getsize(f) for f in files)

    cmds, cmd_args = collections.Counter(), collections.defaultdict(collections.Counter)
    tools, skills, agents, mcp = (collections.Counter() for _ in range(4))
    models, efforts, entry, versions, perm = (collections.Counter() for _ in range(5))
    words, months = collections.Counter(), collections.Counter()
    proj = collections.defaultdict(lambda: collections.Counter())
    prompts = []  # (timestamp, project, text)
    sessions = set()
    t0, done_bytes, bad = time.time(), 0, 0

    for i, path in enumerate(files, 1):
        p = project_of(a.root, path)
        sub = os.sep + "subagents" + os.sep in path
        proj[p]["files"] += 1
        try:
            fh = open(path, "rb")
        except OSError:
            continue
        with fh:
            for raw in fh:
                # Cheap filters before json: only user and assistant turns, never tool results.
                if b'"type":"user"' not in raw and b'"type":"assistant"' not in raw:
                    continue
                if b'"tool_result"' in raw and b"<command-name>" not in raw:
                    continue
                try:
                    d = json.loads(raw)
                except ValueError:
                    bad += 1
                    continue
                msg = d.get("message") if isinstance(d.get("message"), dict) else {}
                sid = d.get("sessionId")
                if sid:
                    sessions.add(sid)
                if d.get("type") == "assistant":
                    if sub:
                        proj[p]["subagent_turns"] += 1
                    m = msg.get("model")
                    if m and m != "<synthetic>":
                        models[m] += 1
                    if d.get("effort"):
                        efforts[d["effort"]] += 1
                    for c in msg.get("content") or []:
                        if isinstance(c, dict) and c.get("type") == "tool_use":
                            n = c.get("name", "?")
                            tools[n] += 1
                            inp = c.get("input") or {}
                            if n == "Skill":
                                skills[inp.get("skill") or inp.get("command") or "?"] += 1
                            elif n in ("Agent", "Task"):
                                agents[inp.get("subagent_type") or "general-purpose"] += 1
                            elif n.startswith("mcp__"):
                                mcp[n.split("__")[1]] += 1
                    continue
                if sub or d.get("isMeta") or d.get("isSidechain"):
                    continue
                text = human_text(msg)
                if not text:
                    continue
                entry[d.get("entrypoint", "?")] += 1
                if d.get("version"):
                    versions[d["version"]] += 1
                if d.get("permissionMode"):
                    perm[d["permissionMode"]] += 1
                ts = d.get("timestamp", "")
                for c in CMD.findall(text):
                    cmds["/" + c] += 1
                    am = ARGS.search(text)
                    if am and am.group(1).strip():
                        cmd_args["/" + c][am.group(1).strip()[:40]] += 1
                    proj[p]["commands"] += 1
                if ("<command-name>" in text or text.lstrip().startswith(NOT_HUMAN) or text.lstrip().startswith("<")
                        or "continued from a previous conversation" in text[:400]):
                    continue
                proj[p]["prompts"] += 1
                months[ts[:7]] += 1
                for w in WORD.findall(PLACEHOLDER.sub(" ", text).lower()[:2000]):
                    if w not in STOP:
                        words[w] += 1
                prompts.append((ts, p, text.strip()))
        done_bytes += os.path.getsize(path)
        if i % 25 == 0 or i == len(files):
            el = time.time() - t0
            print(f"digest: {i}/{len(files)} files, {done_bytes >> 20}/{total_bytes >> 20} MiB, {el:.0f}s",
                  file=sys.stderr, flush=True)

    out = []
    w = out.append
    w(f"# Claude Code usage digest ({dt.date.today()})\n")
    w(f"{len(files)} transcript files, {total_bytes >> 20} MiB, {len(sessions)} sessions, "
      f"{len(prompts)} human prompts, {sum(cmds.values())} slash commands. Root: {a.root}. "
      f"Unparsable lines: {bad}.\n")
    w("Slash commands are what the user typed (`<command-name>`); prompts exclude tool results, "
      "reminders and task notifications; subagent transcripts count only toward tools/models.\n")

    def table(title, counter, n=40):
        w(f"\n## {title}\n")
        if not counter:
            w("(none)\n")
            return
        for k, v in counter.most_common(n):
            w(f"- {v:6d}  {k}")
        w("")

    table("Slash commands (transcripts)", cmds, 80)
    hist = collections.Counter()
    hpath = os.path.join(os.path.dirname(a.root.rstrip("/")), "history.jsonl")
    try:
        with open(hpath, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                try:
                    t = json.loads(line).get("display", "")
                except ValueError:
                    continue
                if t.startswith("/") and len(t) > 1:
                    hist[t.split()[0]] += 1
    except OSError:
        pass
    table(f"Slash commands typed at the interactive prompt ({hpath}; catches /btw and the like)", hist, 60)
    w("\n## Slash command arguments (top per command)\n")
    for c, _ in cmds.most_common(30):
        if cmd_args[c]:
            w(f"- {c}: " + ", ".join(f"`{k}` x{v}" for k, v in cmd_args[c].most_common(6)))
    w("")
    table("Tools called (main + subagents)", tools, 60)
    table("Skills invoked", skills, 40)
    table("Subagent types", agents, 30)
    table("MCP servers used", mcp, 30)
    table("Models (assistant turns)", models)
    table("Effort (assistant turns)", efforts)
    table("Permission mode at prompt", perm)
    table("Entrypoint (cli = typed in the terminal, sdk-cli = FEMME/headless)", entry)
    table("Claude Code versions seen", versions, 15)
    w("\n## Prompts per month\n")
    for k in sorted(months):
        w(f"- {k}: {months[k]}")
    w("\n## Projects (dir name = cwd with / as -)\n")
    for k, c in sorted(proj.items(), key=lambda kv: -kv[1]["prompts"])[:40]:
        w(f"- {k}: {c['prompts']} prompts, {c['commands']} commands, {c['files']} files, "
          f"{c['subagent_turns']} subagent turns")
    table("Most frequent prompt words (stopwords removed)", words, 150)

    w("\n## Sample prompts (25 most recent + random others; first 300 chars)\n")
    prompts.sort()
    recent = prompts[-25:]
    rest = prompts[:-25]
    random.seed(0)
    pick = random.sample(rest, min(len(rest), max(0, a.samples - len(recent))))
    for ts, p, text in sorted(pick) + recent:
        one = " ".join(text.split())[:300]
        w(f"- [{ts[:10]} {p}] {one}")
    w(f"\nGrep a full transcript for evidence: `grep -l '<word>' {a.root}/*/*.jsonl`.")
    print("\n".join(out))
    print(f"digest: done in {time.time() - t0:.0f}s", file=sys.stderr)


if __name__ == "__main__":
    main()

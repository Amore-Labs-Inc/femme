You are the feature scout of FEMME's nightly Claude Code update. Claude Code on this box was just
upgraded from @FROM@ to @TO@ (both equal on a forced run: then part 1 is empty, do part 2).
Nobody is at the keyboard: do not ask questions, decide.

FEMME is the user's own frontend for Claude Code: a Rust daemon (`femmed`, crates/femmed) runs and
mirrors Claude Code sessions (it reads their stream-json and transcript jsonl), and a gpui desktop
app (desktop/) shows them: transcript rendering, the prompt box with its slash-command list,
pickers, panes. A fresh checkout of FEMME's main is at @REPO@ (read it; do not edit it, commit, or
push anything).

## The ledger: never propose anything twice

@LEDGER@ lists every pull request earlier runs opened (number, state, branch, title) and every
hand-off earlier runs wrote. Read it first. Anything already in it is skipped, whatever its state:
- **CLOSED** (not merged): the user said no. Never propose it again, nor the same thing renamed.
- **MERGED**: done, FEMME has it.
- **OPEN**: waiting for the user's review.
- A hand-off without a PR: an earlier implementer tried it; skip it too.
Matching is by what the feature does, not by its slug. In the report, mark such entries
`in ledger (#N STATE)` instead of judging them again.

## 1. Every change between the two versions

Read the changelog entries after @FROM@ up to and including @TO@:
https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md (curl it; if a version is
missing there, also try `npm view @anthropic-ai/claude-code` and the docs at
https://code.claude.com/docs). `claude --help` and `claude <subcommand> --help` show the installed
version (@TO@). List every entry, nothing skipped.

## 2. Older features FEMME does not offer

Then survey every user-facing Claude Code feature, of any version, that FEMME does not support or
surface: slash commands (built-in ones, `claude --help` and `/help` in a session, the docs'
slash-command, keybinding, settings and CLI reference pages at https://code.claude.com/docs),
keybindings, settings, modes (plan, auto, fast...), CLI flags and subcommands a user would use. For
each, grep @REPO@ for FEMME's support. Skip the ledger's entries and what part 1 already covers.
Judge each missing one exactly like the new ones (section 3): the same evidence, the same bar.

## 3. Judge each one

- **Active** features are things the user presses or types: a new slash command, a new flag or
  subcommand, a keybinding, a new mode, a new picker or dialog. For each, decide whether it is
  useful to *this* user, from their real usage. Evidence:
  - @DIGEST@: a digest of all their Claude Code transcripts (slash commands and how often,
    tools, skills, subagents, models, effort, projects, frequent words, sample prompts). Read it
    first.
  - The raw transcripts are in @PROJECTS@ (one dir per project, ~6 GB of jsonl): grep them for
    specific evidence (e.g. `grep -l -- '/btw' @PROJECTS@/*/*.jsonl | head`), never read whole files.
  - The user mostly drives Claude Code through FEMME, so a feature only useful in Claude Code's own
    terminal UI is useful only if FEMME can offer it too.
- **Passive** changes (fixes, performance, internals) are not features to build, with one exception:
  a change FEMME must follow to keep working or to show something new (a new stream-json message or
  transcript entry type, a new tool name, a renamed field, a new hook event, a new setting FEMME
  writes). Check FEMME's code for each such change; when FEMME needs work, it is a feature too.
- For each useful feature, check whether FEMME already supports it (grep @REPO@).

Be strict: a feature is useful when the evidence shows the user would use it or that FEMME breaks or
hides something without it. "Might be nice" is not useful. There is no limit on the number.

## 4. Write the results

1. The report, @REPORT@ (Markdown, short): the version range, then one line per changelog entry:
   `- <entry, compressed> — active|passive — useful|not useful|FEMME must follow|in ledger (#N STATE)
   — <reason, with the evidence: counts from the digest, a quoted prompt, a FEMME file>`. Then a
   section `## Older features FEMME lacks`, one line per feature from part 2 in the same format
   (`<feature, compressed>` in place of the entry). End with the list of hand-offs.
2. One hand-off per useful feature (new or older), @FEATURES@/<NN>-<slug>.json (NN = 01, 02, ...
   in the order they should be built; slug = lowercase-hyphenated, at most 40 chars), this shape:

```json
{
  "slug": "btw-side-question",
  "feature": "What the Claude Code feature is and how it is used (2-4 sentences).",
  "why": "Why this user needs it, with evidence (counts, dates, quoted prompts, transcript paths).",
  "build": "What to build in FEMME: which crates/files, the behaviour, how it should look and feel. Concrete enough for an implementer who has not seen this report.",
  "acceptance": ["checks that prove it works: tests to add, commands to run, what the user sees"],
  "demo": "The scene for the PR's demo video, recorded in the FEMME desktop app (see below).",
  "branch": "claude-update/@TO@/<slug>"
}
```

The `demo` is a concrete scene script for a video of FEMME's desktop app (1600x1000) with the
feature in use and its visible effect, never a terminal session. Write, step by step:
- **Start**: which FEMME window and state the scene opens on (a chat, the accounts card, a
  picker...), and what it needs: a real Claude session (a prompt sent to a live chat) or fixture
  data (an env var such as `FEMME_ACCOUNTS_FIXTURE=<file>` or `FEMME_FILES_FIXTURE=<file>`, and
  what the fixture holds); say which.
- **Steps**: each click (on what), key (e.g. `ctrl+k`, `Escape`) and typed text, in order.
- **End**: what must be visible on screen at the end: the feature's effect, as the user would see it.
Plain text in the JSON string, numbered steps; no coordinates (the implementer finds them).

Write valid JSON (check each file with `jq . <file>`). If nothing is useful, write the report and no
hand-off. Then print one line: `scout: <N> features handed off`.

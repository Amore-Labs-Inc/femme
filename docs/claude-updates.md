# Nightly Claude Code updates

Optional (`CLAUDE_UPDATES=1` in `.env`, then `bootstrap.sh`): every night at 00:45, done by 09:00

The run fits in `BUDGET_MIN` minutes (495: 00:45 to 09:00). Stages get what is left (the scout and
each implementer at most 3 h); a feature with under 45 min of its own is left for a rerun. The last
45 min are a fix pass: main moves at night (other sessions push), so every pull request the night
opened that conflicts with main gets it merged in by a Fable chat (`claude-update/fix.md`), tested
and pushed. In the morning each one is mergeable as it stands.
`femme-claude-update.timer` runs `server/claude-update.sh`:

1. `mise outdated claude`: with no newer Claude Code it logs one line to `upgrade.log` and stops
   (no upgrade, no clone, no claude). Otherwise `mise upgrade claude`, and on.
2. A fresh clone of main in `~/femme-updates/<version>/repo` (the three newest runs are kept).
3. `claude-update/digest.py` boils `~/.claude/projects` down to a page: slash commands, tools,
   models, effort, projects, sample prompts (about a minute).
4. The **scout** (Opus, effort max) reads the changelog between the two versions, and surveys
   every older user-facing Claude Code feature (slash commands, keys, settings, modes, flags) that
   FEMME lacks. It judges each against that usage and FEMME's code, and writes `<version>.md` plus
   one hand-off per useful feature, `<version>/features/NN-<slug>.json`; the hand-off's `demo` is
   a scene script for FEMME's desktop app (start state, clicks and keys, the effect at the end).
   The **ledger**, `<version>/ledger.md`, lists every `claude-update/*` PR with its state and every
   earlier hand-off: the scout proposes none of them again (a PR closed unmerged means no).
5. One **implementer** per hand-off (Fable, effort medium), one after another, each in its own
   worktree on `claude-update/<version>/<slug>`: plan, build, test, small commits, a demo video
   of FEMME with the feature in use (`claude-update/record-desktop.sh`), push, `gh pr create`.
   It never merges; a pre-push hook in the clone only lets `claude-update/*` branches out.

The video lands in `<state>/pr-videos/<branch, '/' as '-'>.mp4` (8 MiB at most), where the
desktop app finds it beside the PR. `record-desktop.sh <scene.sh> <video.mp4>`, run in the
worktree after `cargo build -p femmed -p femme-holder` and a desktop `cargo build`, plays the
scene against a throwaway femmed and the desktop in Xvfb (1600x1000) and records it; the scene
uses `env_desktop`, `start`, `click X Y`, `dclick`, `key`, `type`, `pause`, `shot`. The
implementer checks frames of the video before the PR; with no working FEMME video the PR body
opens with `⚠ No FEMME demo: <reason>`. `record.sh` (a vhs tape, an asciinema cast, a script, or
a video) is only for a short terminal add-on; `tools.sh` installs vhs, ttyd, asciinema and agg
into `~/.local/bin`.

## Watching and rerunning

`<state>` is `~/.local/state/femme`. Everything is under `<state>/reports/claude-updates/`:
`upgrade.log` (the nightly "no pending update" lines), `<version>.log` (stage lines: `scout
start`, `feature 2/5 <slug> start`, `pr opened <url>`, exit codes), `<version>.md` (the scout's
report), `<version>/ledger.md`, `<version>/scout.jsonl` and `<version>/<slug>.jsonl` (each claude
run's stream), `<version>/<slug>.plan.md`.

```sh
systemctl --user start femme-claude-update     # the 00:45 run, now
server/claude-update.sh --from 2.1.300 --to 2.1.302   # a range, no upgrade; open PRs are skipped
server/claude-update.sh --force                # run even with no pending update (testing)
DRY=1 server/claude-update.sh --from X --to Y  # everything but claude: prints commands and prompts
```

A lock keeps runs from overlapping; one failing feature does not stop the others; under
`MIN_FREE_GB` (25) of free disk the loop stops. `SCOUT_MODEL` and `IMPL_MODEL` override the models.

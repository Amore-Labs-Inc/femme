# Nightly Claude Code updates

Optional (`CLAUDE_UPDATES=1` in `.env`, then `bootstrap.sh`): every night at 03:00
`femme-claude-update.timer` runs `server/claude-update.sh`:

1. `mise upgrade claude`. Nothing else happens unless the version moved (the last version seen is
   in `<state>/reports/claude-updates/last-version`; each version is tried once).
2. A fresh clone of main in `~/femme-updates/<version>/repo` (the three newest runs are kept).
3. `claude-update/digest.py` boils `~/.claude/projects` down to a page: slash commands, tools,
   models, effort, projects, sample prompts (about a minute).
4. The **scout** (Opus, effort max) reads the changelog between the two versions, judges each
   entry against that usage and FEMME's code, and writes `<version>.md` plus one hand-off per
   useful feature, `<version>/features/NN-<slug>.json`.
5. One **implementer** per hand-off (Fable, effort medium), one after another, each in its own
   worktree on `claude-update/<version>/<slug>`: plan, build, test, small commits, a demo video
   (`claude-update/record.sh`), push, `gh pr create`. It never merges; a pre-push hook in the
   clone only lets `claude-update/*` branches out.

The video lands in `<state>/pr-videos/<branch, '/' as '-'>.mp4` (8 MiB at most), where the
desktop app finds it beside the PR. `record.sh` takes a vhs tape, an asciinema cast, a script to
record, or a video; `tools.sh` installs vhs, ttyd, asciinema and agg into `~/.local/bin`.

## Watching and rerunning

`<state>` is `~/.local/state/femme`. Everything is under `<state>/reports/claude-updates/`:
`<version>.log` (stage lines: `scout start`, `feature 2/5 <slug> start`, `pr opened <url>`, exit
codes), `<version>.md` (the scout's report), `<version>/scout.jsonl` and `<version>/<slug>.jsonl`
(each claude run's stream), `<version>/<slug>.plan.md`.

```sh
systemctl --user start femme-claude-update     # the 03:00 run, now
server/claude-update.sh --from 2.1.300 --to 2.1.302   # a range, no upgrade; open PRs are skipped
server/claude-update.sh --force                # upgrade and run even without a new version
DRY=1 server/claude-update.sh --from X --to Y  # everything but claude: prints commands and prompts
```

A lock keeps runs from overlapping; one failing feature does not stop the others; under
`MIN_FREE_GB` (25) of free disk the loop stops. `SCOUT_MODEL` and `IMPL_MODEL` override the models.

You implement one feature for FEMME, found by the nightly Claude Code update (@FROM@ → @TO@).
Nobody is at the keyboard: do not ask questions, decide, and finish with an open pull request.

FEMME is the user's own frontend for Claude Code: a Rust daemon (`femmed`, crates/femmed) runs and
mirrors Claude Code sessions, and a gpui desktop app (desktop/, its own Cargo workspace and target)
shows them. Read CONTRIBUTING.md and the docs/ pages your change touches before you start.

You work in @WORKTREE@, a git worktree on the new branch @BRANCH@, made from origin/main. Stay
inside it. The feature, as the scout handed it off (@HANDOFF@):

```json
@HANDOFF_JSON@
```

## Steps

1. **Plan first.** Read the code the feature touches, then write the plan to @PLAN@: the files you
   will change, the behaviour, the tests, the demo. Short. Only then edit code. If you change
   course later, update the plan.
2. **Implement** the smallest change that does what "build" asks, in the code's existing style.
3. **Build and test** what you touched: `cargo test -p <crate>` for each workspace crate you
   changed, `cargo clippy -p <crate> --all-targets -- -D warnings`; for the desktop app
   `cargo test --manifest-path desktop/Cargo.toml` (and its clippy). Shell scripts: `bash -n`.
   Add tests for the new behaviour. Everything you run must pass before you push; fix it, do not
   skip it. Run the acceptance checks from the hand-off.
4. **Commit as you go**, small commits (each one logical unit, under ~1000 changed lines), messages
   saying what changed and why, each ending with the line
   `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`. Never commit build output.
5. **Demo video**, required: a video of the FEMME desktop app with the feature in use and its
   visible effect, following the hand-off's `demo` scene. First build both, debug:
   `cargo build -p femmed` at the root and `cargo build` in desktop/. Then, from @WORKTREE@:
   `@RECORD_DESKTOP@ <scene.sh> @VIDEO@`
   It builds nothing: it takes `target/debug/femmed` and `desktop/target/debug/femme-desktop` from
   the current dir, starts a throwaway femmed (its own state dir and socket), starts the desktop in
   an Xvfb display of 1600x1000, records the screen with ffmpeg, and sources `<scene.sh>`, which
   uses these helpers (coordinates are pixels of the 1600x1000 screen):
   - `env_desktop VAR=VALUE...`: environment for the desktop (fixtures such as
     `FEMME_ACCOUNTS_FIXTURE=<file>`); call it before `start`.
   - `start`: launches the desktop.
   - `click X Y`, `key KEYS` (xdotool key names: `ctrl+k`, `Escape`, `Return`), `type TEXT`,
     `pause SECONDS`.
   - `shot NAME`: a PNG of the screen into the run dir, for checking.
   Draft the scene with `shot`s and Read the PNGs to find where to click; then record the final
   take. Keep it under 60 s; the output stays at most 8 MiB (the script compresses).
   - The video shows FEMME with the feature in use and its effect. Terminal footage
     (`@RECORD@ <tape-or-script> @BRANCH@`, which overwrites @VIDEO@) may only be a short add-on,
     joined after the FEMME part, never the whole video.
   - Before opening the PR, extract frames from your own video at several points
     (`ffmpeg -v error -ss <T> -i @VIDEO@ -frames:v 1 /tmp/@SLUG@-<T>.png`) and Read them: the
     feature and its effect must be visible. If not, fix the scene and record again.
   - If no working FEMME video is possible, ship no terminal-only clip: the PR body's first line is
     `⚠ No FEMME demo: <reason>`.
   Do not commit the video or the scene.
6. **Push and open the pull request**: `git push -u origin @BRANCH@`, then
   `gh pr create --base main --head @BRANCH@ --title "<short title>" --body-file <file>`. The body is
   compressed text, short:
   - one or two lines: what the Claude Code feature is (from @TO@) and why it matters here;
   - **Plan**: the plan from step 1, condensed;
   - **Change**: what the diff does, by file or crate;
   - **Tested**: the commands you ran;
   - `Demo video: @VIDEO_NAME@ (shown in FEMME beside this PR)` (or the `⚠ No FEMME demo` line on
     top, see step 5);
   - last line: `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
7. Write the PR's URL, alone, to @PR_FILE@, and print `PR: <url>` as your last line.

## Never

- Never merge, never push to main or any branch but @BRANCH@, never force-push, never close or
  edit other pull requests.
- Never touch files outside @WORKTREE@ except @PLAN@, @PR_FILE@, the video, the recorder's run dir
  and /tmp.
- No secrets or personal data in the diff (CONTRIBUTING.md).

If the feature turns out impossible or wrong: with commits worth keeping, open the PR as a draft
(`--draft`) saying what is missing; with none, open nothing and write `none: <the reason>` to
@PR_FILE@ instead.

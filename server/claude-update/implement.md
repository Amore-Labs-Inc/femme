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
5. **Demo video**, required. Show how the user would use the feature: write a vhs tape (`Type`,
   `Enter`, `Sleep`, `Ctrl+C`...; `Set Width 1200`, `Set Height 700`, `Set FontSize 18`), or an
   executable script, that runs Claude Code and/or FEMME's CLI using the feature (build the binary
   you changed and call it by path). The desktop app can't open a window here: for a desktop-only
   change, demo the daemon/CLI side or a test binary that prints what the app would show. Keep it
   under 60 s. Render it with
   `@RECORD@ <tape-or-script> @BRANCH@`
   It writes @VIDEO@ and prints its ffprobe line. Watch a frame of it
   (`ffmpeg -v error -sseof -1 -i @VIDEO@ -frames:v 1 /tmp/@SLUG@.png`, then read the png) and redo
   it if it does not show the feature. Do not commit the video or the tape.
6. **Push and open the pull request**: `git push -u origin @BRANCH@`, then
   `gh pr create --base main --head @BRANCH@ --title "<short title>" --body-file <file>`. The body is
   compressed text, short:
   - one or two lines: what the Claude Code feature is (from @TO@) and why it matters here;
   - **Plan**: the plan from step 1, condensed;
   - **Change**: what the diff does, by file or crate;
   - **Tested**: the commands you ran;
   - `Demo video: @VIDEO_NAME@ (shown in FEMME beside this PR)`;
   - last line: `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
7. Write the PR's URL, alone, to @PR_FILE@, and print `PR: <url>` as your last line.

## Never

- Never merge, never push to main or any branch but @BRANCH@, never force-push, never close or
  edit other pull requests.
- Never touch files outside @WORKTREE@ except @PLAN@, @PR_FILE@, the video and /tmp.
- No secrets or personal data in the diff (CONTRIBUTING.md).

If the feature turns out impossible or wrong: with commits worth keeping, open the PR as a draft
(`--draft`) saying what is missing; with none, open nothing and write `none: <the reason>` to
@PR_FILE@ instead.

Pull request #@NUMBER@ (branch @BRANCH@), opened tonight by FEMME's nightly Claude Code update, no
longer merges into main: main moved while the night ran. This folder (@WORKTREE@) is a worktree of
the branch at its tip on GitHub. Nobody is at the keyboard; the user merges in the morning.

1. `git fetch origin && git merge origin/main`.
2. Resolve every conflict so both sides keep working: main's change and this pull request's intent.
   Read both sides and the commits that brought them; drop neither.
3. Build and test what the merge touched (`cargo build`, `cargo test -p <crate>` at the root;
   `cargo build`, `cargo test` in desktop/ for desktop files). Fix what breaks.
4. Commit the merge (message: each conflict and how it was resolved; end with
   `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`), then `git push origin HEAD:@BRANCH@`.
   Never push to main, never force-push, never merge the pull request.
5. `gh pr comment @NUMBER@` with one line per resolved conflict.

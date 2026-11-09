#!/usr/bin/env bash
# pr-merge.sh <number>: merges a pull request into main even when it conflicts with main. A
# conflicting one gets main merged in by Claude (server/claude-update/fix.md: resolve, test, push),
# then it is merged; if another merge made it conflict again meanwhile, again (3 tries). femmed
# starts it as the transient unit femme-pr-merge-<number> when FEMME's "Merge & deploy" meets a
# conflict, so it outlives a femmed restart (a deploy). The merge lands on main and deploy.sh ships
# it. Log: <state>/reports/pr-merge/<number>.log.
set -uo pipefail
# Braces: read whole before running, so deploy.sh resetting ~/femme can't rewrite it mid-run.
{
n=${1:?usage: pr-merge.sh <number>}
case $n in *[!0-9]*) echo "pr-merge: not a number: $n" >&2; exit 2 ;; esac
here=$(cd "$(dirname "$0")" && pwd)
export PATH="$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
state=${FEMME_STATE:-$HOME/.local/state/femme}
log=$state/reports/pr-merge/$n.log
mkdir -p "${log%/*}"
say() { printf '%s %s\n' "$(date '+%F %T')" "$*" | tee -a "$log"; }
cd "$HOME/femme" || exit 1
base=$HOME/femme-work/pr-fix repo=$HOME/femme-work/pr-fix/repo wt=$HOME/femme-work/pr-fix/$n

# OPEN's mergeability (asking for one PR makes GitHub work it out; it can take a few tries), else
# its state (MERGED, CLOSED).
status() {
  local s
  for _ in 1 2 3 4 5 6 7 8 9; do
    s=$(gh pr view "$n" --json state,mergeable -q 'if .state != "OPEN" then .state else .mergeable end' 2>>"$log")
    [ "$s" = UNKNOWN ] || [ -z "$s" ] || break
    sleep 10
  done
  echo "${s:-UNKNOWN}"
}

for try in 1 2 3; do
  s=$(status)
  say "#$n try $try: $s"
  case $s in
    MERGED) exit 0 ;;
    CLOSED) say "#$n was closed: nothing to merge"; exit 1 ;;
    MERGEABLE)
      if gh pr merge "$n" --squash --delete-branch >>"$log" 2>&1; then say "#$n merged"; exit 0; fi
      say "#$n: merge refused, looking again"; continue ;;
    CONFLICTING) ;;
    *) say "#$n: GitHub cannot say whether it merges ($s)"; exit 1 ;;
  esac
  branch=$(gh pr view "$n" --json headRefName -q .headRefName)
  case $branch in ''|-*) say "#$n has no branch"; exit 1 ;; esac
  mkdir -p "$base"
  [ -d "$repo/.git" ] || git clone -q --reference-if-able "$HOME/femme" --dissociate "$(git remote get-url origin)" "$repo" >>"$log" 2>&1
  git -C "$repo" fetch -q origin >>"$log" 2>&1
  if [ -e "$wt/.git" ]; then
    git -C "$wt" checkout -q -B "$branch" "origin/$branch" && git -C "$wt" reset -q --hard "origin/$branch"
  else
    git -C "$repo" worktree add -q -B "$branch" "$wt" "origin/$branch"
  fi >>"$log" 2>&1 || { say "#$n: no worktree for $branch"; exit 1; }
  prompt=$(sed -e "s|@NUMBER@|$n|g" -e "s|@BRANCH@|$branch|g" -e "s|@WORKTREE@|$wt|g" "$here/claude-update/fix.md")
  say "#$n: conflicts with main, Claude resolves them in $wt"
  (cd "$wt" && RUSTC_WRAPPER=$(mise which sccache 2>/dev/null) CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
    timeout 1h claude -p "$prompt" --model "${FIX_MODEL:-fable}" --effort medium --dangerously-skip-permissions \
    --output-format stream-json --verbose </dev/null >"${log%.log}.$try.jsonl" 2>&1)
  say "#$n: Claude exit $?"
  rm -rf "$wt/target" "$wt/desktop/target"  # pushed; the build output is not needed
done
say "#$n: still not merged after 3 tries"
exit 1
}

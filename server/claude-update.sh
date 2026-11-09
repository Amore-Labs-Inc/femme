#!/usr/bin/env bash
# Runs on the VPS from femme-claude-update.timer (03:00). Upgrades Claude Code; when the version
# moved, a scout (Opus, effort max) reads what changed and hands off each feature useful to this
# user, then one implementer per feature (Fable, effort medium) builds it in a fresh clone and opens
# a pull request with a demo video. docs/claude-updates.md has the whole picture.
#   claude-update.sh                     upgrade, then scout + implement if the version changed
#   claude-update.sh --from X --to Y     no upgrade: run for that range (a rerun skips features
#                                        whose PR is already open)
#   claude-update.sh --force             upgrade, and run even when the version did not change
#   DRY=1 claude-update.sh ...           everything but claude: prints its commands and prompts
# Progress: <state>/reports/claude-updates/<version>.log; claude's own output beside it in <version>/.
set -uo pipefail
# Braces: bash reads the whole block before running it, so deploy.sh resetting ~/femme during the
# hours this runs can't rewrite it under our feet.
{
here=$(cd "$(dirname "$0")" && pwd)
lib=$here/claude-update
export PATH="$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
state=${FEMME_STATE:-$HOME/.local/state/femme}
reports=$state/reports/claude-updates
work=${FEMME_UPDATES_DIR:-$HOME/femme-updates}
dry=${DRY:-0}
scout_model=${SCOUT_MODEL:-opus} impl_model=${IMPL_MODEL:-fable}
min_free_gb=${MIN_FREE_GB:-25}  # a feature needs a fresh target/ (several GB): stop below this
mkdir -p "$reports" "$work"

from='' to='' force=0
while [ $# -gt 0 ]; do
  case $1 in
    --from) from=$2; shift ;;
    --to) to=$2; shift ;;
    --force) force=1 ;;
    *) echo "usage: claude-update.sh [--from X --to Y] [--force]" >&2; exit 2 ;;
  esac
  shift
done

exec 9>"$reports/.lock"
flock -n 9 || { echo "claude-update: another run is going (lock $reports/.lock)" >&2; exit 0; }

log=$reports/upgrade.log  # until the version is known
say() { printf '%s %s\n' "$(date '+%F %T')" "$*" | tee -a "$log"; }
version() { claude --version 2>/dev/null | awk '{print $1}'; }
cd "$HOME" || exit 1  # mise reads the config of the current dir: none of a project's

# 1. Upgrade. The last version a run saw is kept, so an upgrade made elsewhere (bootstrap.sh's
# `mise install`) is still noticed.
if [ -z "$to" ]; then
  before=$(cat "$reports/last-version" 2>/dev/null || version)
  if [ "$dry" = 1 ]; then say "dry: would run: mise upgrade claude"
  else
    mise upgrade claude >>"$log" 2>&1; say "mise upgrade claude: exit $?"
  fi
  from=${from:-$before} to=$(version)
  [ -n "$to" ] || { say "claude --version failed after the upgrade"; exit 1; }
  [ "$dry" = 1 ] || echo "$to" > "$reports/last-version"  # before the run: a version is tried once, not every night
  if [ "$from" = "$to" ] && [ "$force" = 0 ]; then say "claude $to: no new version"; exit 0; fi
fi
[ -n "$from" ] || { echo "claude-update: --to needs --from" >&2; exit 2; }
run=$reports/$to
log=$reports/$to.log
mkdir -p "$run/features"
say "run $from -> $to (dry=$dry, pid $$)"

# Older runs' clones go (each holds a checkout and build output); the three newest stay for a look.
find "$work" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' | sort -rn | tail -n +4 | cut -d' ' -f2- \
  | while read -r d; do [ "$d" = "$work/$to" ] || { rm -rf "$d"; say "removed old run $d"; }; done

"$lib/tools.sh" >>"$log" 2>&1; say "tools.sh: exit $?"

# 2. A fresh clone of main. Borrowing ~/femme's objects makes it fast; --dissociate copies them so
# the clone does not depend on that checkout.
repo=$work/$to/repo
url=${FEMME_REPO_URL:-$(git -C "$here/.." remote get-url origin)}
if [ -d "$repo/.git" ]; then git -C "$repo" fetch -q origin; else
  git clone -q --reference-if-able "$here/.." --dissociate "$url" "$repo" || { say "clone of $url failed"; exit 1; }
fi
git -C "$repo" fetch -q origin main
# A guard on the implementers' pushes: only their own claude-update/ branches leave this clone.
cat > "$repo/.git/hooks/pre-push" <<'EOF'
#!/bin/sh
while read -r _ _ remote_ref _; do
  case $remote_ref in refs/heads/claude-update/*) ;; *) echo "pre-push: only claude-update/* branches" >&2; exit 1 ;; esac
done
EOF
chmod +x "$repo/.git/hooks/pre-push"
say "clone $repo at $(git -C "$repo" rev-parse --short origin/main)"

# Fills a prompt's @NAME@ placeholders from NAME=value arguments.
render() {
  local text kv
  text=$(cat "$1"); shift
  for kv in "$@"; do text=${text//@${kv%%=*}@/${kv#*=}}; done
  printf '%s\n' "$text"
}
# claude <dir> <logfile> <timeout> <prompt> <args...>: headless, its stream into <logfile>.
claude_run() {
  local dir=$1 out=$2 limit=$3 prompt=$4; shift 4
  local cmd=(timeout "$limit" claude -p "$prompt" "$@" --dangerously-skip-permissions --output-format stream-json --verbose)
  if [ "$dry" = 1 ]; then
    say "dry: in $dir: $(printf '%q ' "${cmd[@]:0:4}") <prompt> ${cmd[*]:5} > $out"
    printf '%s\n' "----- prompt -----" "$prompt" "----- end prompt -----" | tee -a "$log"
    return 0
  fi
  (cd "$dir" && "${cmd[@]}" </dev/null >"$out" 2>&1)
}

# 3. Scout.
say "digest start"
python3 -I "$lib/digest.py" >"$run/digest.md" 2>>"$log"; say "digest: exit $?, $(wc -c <"$run/digest.md") bytes"
say "scout start ($scout_model, effort max)"
prompt=$(render "$lib/scout.md" FROM="$from" TO="$to" REPO="$repo" DIGEST="$run/digest.md" \
  PROJECTS="$HOME/.claude/projects" REPORT="$reports/$to.md" FEATURES="$run/features")
claude_run "$repo" "$run/scout.jsonl" 3h "$prompt" --model "$scout_model" --effort max
say "scout: exit $?"
if [ "$dry" = 1 ] && ! ls "$run"/features/*.json >/dev/null 2>&1; then
  # A stand-in hand-off, so a dry run walks the feature loop too.
  jq -n --arg b "claude-update/$to/dry-example" '{slug:"dry-example", feature:"(dry run)", why:"(dry run)",
    build:"(dry run)", acceptance:["(dry run)"], demo:"(dry run)", branch:$b}' >"$run/features/01-dry-example.json"
fi

# 4. One implementer per hand-off, one after another; a failing one does not stop the rest.
shopt -s nullglob
features=("$run"/features/*.json)
n=${#features[@]} i=0 opened=0
say "scout handed off $n features"
for f in "${features[@]}"; do
  i=$((i + 1))
  slug=$(jq -r '.slug // empty' "$f" 2>/dev/null | tr -cd 'a-z0-9-' | cut -c1-40)
  [ -n "$slug" ] || { say "feature $i/$n: $f is not a hand-off, skipped"; continue; }
  branch=claude-update/$to/$slug  # never what the hand-off says: the push guard expects this shape
  prfile=$run/$slug.pr
  if grep -q '^https://' "$prfile" 2>/dev/null; then say "feature $i/$n $slug: PR already open $(cat "$prfile")"; continue; fi
  free=$(df --output=avail -BG "$work" | tail -n1 | tr -dc 0-9)
  if [ "$free" -lt "$min_free_gb" ]; then say "stop: ${free}G free on disk, under $min_free_gb G; features $i-$n wait for a rerun"; break; fi
  say "feature $i/$n $slug start"
  wt=$work/$to/$slug
  if [ ! -d "$wt" ]; then
    git -C "$repo" worktree add -q -B "$branch" "$wt" origin/main >>"$log" 2>&1 || { say "feature $i/$n $slug: worktree failed"; continue; }
  fi
  video=$state/pr-videos/${branch//\//-}.mp4
  prompt=$(render "$lib/implement.md" FROM="$from" TO="$to" WORKTREE="$wt" BRANCH="$branch" SLUG="$slug" \
    HANDOFF="$f" HANDOFF_JSON="$(jq . "$f")" PLAN="$run/$slug.plan.md" PR_FILE="$prfile" \
    RECORD="$lib/record.sh" VIDEO="$video" VIDEO_NAME="${video##*/}")
  # sccache shares compiled dependencies between the features' fresh target/ dirs; no debug info
  # and no incremental cache keep each target/ a few GB.
  RUSTC_WRAPPER=$(mise which sccache 2>/dev/null) CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
    claude_run "$wt" "$run/$slug.jsonl" 3h "$prompt" --model "$impl_model" --effort medium
  rc=$?
  say "feature $i/$n $slug: implementer exit $rc"
  [ "$dry" = 1 ] && continue
  pr=$(cat "$prfile" 2>/dev/null)
  case $pr in https://*) ;; *) pr=$(cd "$repo" && gh pr list --head "$branch" --state open --json url -q '.[0].url' 2>/dev/null) ;; esac
  case $pr in
    https://*) echo "$pr" >"$prfile"; opened=$((opened + 1)); say "feature $i/$n $slug: pr opened $pr" ;;
    *) say "feature $i/$n $slug: no PR (${pr:-no reason given})" ;;
  esac
  if [ -s "$video" ]; then say "feature $i/$n $slug: video $(du -h "$video" | cut -f1) ${video##*/}"
  else say "feature $i/$n $slug: video MISSING (${video##*/})"; fi
  rm -rf "$wt/target" "$wt/desktop/target"  # the branch is pushed; its build output is not needed
done
say "done: $n features, $opened PRs opened"
exit
}

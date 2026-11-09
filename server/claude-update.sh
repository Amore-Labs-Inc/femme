#!/usr/bin/env bash
# Runs on the VPS from femme-claude-update.timer (00:45). Only when mise has a newer Claude Code:
# upgrades it, then a scout (Opus, effort max) reads what changed, and which older features FEMME
# still lacks, and hands off each one useful to this user; one implementer per feature (Fable,
# effort medium) builds it in a fresh clone and opens a pull request with a demo video of FEMME.
# docs/claude-updates.md has the whole picture.
#   claude-update.sh                     if an update is pending: upgrade, scout, implement
#   claude-update.sh --from X --to Y     no upgrade: run for that range (a rerun skips features
#                                        whose PR is already open)
#   claude-update.sh --force             run even with no pending update (testing)
#   DRY=1 claude-update.sh ...           everything but claude: prints its commands and prompts
# The whole run fits in BUDGET_MIN minutes (default 495: 00:45 to 09:00). The last 45 are kept for a
# pass that merges main into every pull request the night opened that conflicts with it, so each is
# mergeable in the morning; a feature is not started with under 45 minutes of its own left.
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
started=$(date +%s)
end=$((started + ${BUDGET_MIN:-495} * 60)) fix_reserve=$((45 * 60))
# Seconds a stage may take: <cap>, cut to what is left before <end - reserve>.
budget() { local left=$((end - ${2:-0} - $(date +%s))); [ "$left" -lt "$1" ] && echo "$left" || echo "$1"; }
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

# 1. Upgrade, only when mise knows a newer claude: no pending update, no run (--force runs anyway).
if [ -z "$to" ]; then
  before=$(version)
  outdated=$(mise outdated claude --json 2>>"$log") || { say "mise outdated claude failed"; exit 1; }
  latest=$(jq -r '.claude.latest // empty' <<<"$outdated" 2>/dev/null)
  if [ -z "$latest" ] && [ "$force" = 0 ]; then say "claude ${before:-?}: no pending update"; exit 0; fi
  if [ -n "$latest" ]; then say "claude ${before:-?}: $latest pending"
  else say "claude ${before:-?}: no pending update, --force"; fi
  if [ "$dry" = 1 ]; then say "dry: would run: mise upgrade claude"
  else
    mise upgrade claude >>"$log" 2>&1; say "mise upgrade claude: exit $?"
  fi
  from=${from:-$before} to=$(version)
  [ -n "$to" ] || { say "claude --version failed after the upgrade"; exit 1; }
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

# The ledger: everything earlier runs proposed, so the scout never proposes it again. Every
# claude-update/* pull request with its state (CLOSED unmerged = the user said no), then the
# hand-offs of earlier runs (some never became a PR).
ledger=$run/ledger.md
{
  echo "# Pull requests from earlier runs (claude-update/* branches)"
  echo
  if prs=$(cd "$repo" && gh pr list --state all --search 'head:claude-update' --limit 500 \
      --json number,title,headRefName,state 2>>"$log"); then
    jq -r '.[] | select(.headRefName | startswith("claude-update/"))
      | "- #\(.number) \(.state) \(.headRefName) — \(.title)"' <<<"$prs"
  else
    echo "(gh pr list failed: the PR list is unknown tonight; check open PRs yourself with gh)"
  fi
  echo
  echo "# Hand-offs of earlier runs (<version>/<slug>: feature)"
  echo
  for f in "$reports"/*/features/*.json; do
    [ "${f%/features/*}" = "$run" ] && continue
    v=${f%/features/*}; v=${v##*/}
    jq -r --arg v "$v" '"- \($v)/\(.slug // "?"): \((.feature // "") | gsub("\\s+"; " ") | .[0:200])"' "$f" 2>/dev/null
  done
} >"$ledger"
say "ledger: $(grep -c '^- #' "$ledger") PRs, $(grep -c '^- [0-9]' "$ledger") earlier hand-offs"

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
  PROJECTS="$HOME/.claude/projects" REPORT="$reports/$to.md" FEATURES="$run/features" LEDGER="$ledger")
claude_run "$repo" "$run/scout.jsonl" "$(budget 10800 "$fix_reserve")s" "$prompt" --model "$scout_model" --effort max
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
  limit=$(budget 10800 "$fix_reserve")
  if [ "$limit" -lt 2700 ]; then say "stop: $((limit / 60)) min left before the fix pass; features $i-$n wait for a rerun"; break; fi
  say "feature $i/$n $slug start ($((limit / 60)) min)"
  wt=$work/$to/$slug
  if [ ! -d "$wt" ]; then
    git -C "$repo" worktree add -q -B "$branch" "$wt" origin/main >>"$log" 2>&1 || { say "feature $i/$n $slug: worktree failed"; continue; }
  fi
  video=$state/pr-videos/${branch//\//-}.mp4
  prompt=$(render "$lib/implement.md" FROM="$from" TO="$to" WORKTREE="$wt" BRANCH="$branch" SLUG="$slug" \
    HANDOFF="$f" HANDOFF_JSON="$(jq . "$f")" PLAN="$run/$slug.plan.md" PR_FILE="$prfile" \
    RECORD="$lib/record.sh" RECORD_DESKTOP="$lib/record-desktop.sh" VIDEO="$video" VIDEO_NAME="${video##*/}")
  # sccache shares compiled dependencies between the features' fresh target/ dirs; no debug info
  # and no incremental cache keep each target/ a few GB.
  RUSTC_WRAPPER=$(mise which sccache 2>/dev/null) CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
    claude_run "$wt" "$run/$slug.jsonl" "${limit}s" "$prompt" --model "$impl_model" --effort medium
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

# 5. Main moves during the night (other sessions push): every pull request this run opened that no
# longer merges into it gets main merged in, its conflicts resolved, tested and pushed.
fixed=0
for prfile in "$run"/*.pr; do
  [ "$dry" = 1 ] && break
  url=$(cat "$prfile"); num=${url##*/} slug=${prfile##*/}; slug=${slug%.pr}
  state=UNKNOWN  # GitHub works it out when asked, which can take a few tries
  for _ in 1 2 3 4 5 6; do
    state=$(cd "$repo" && gh pr view "$num" --json state,mergeable -q 'if .state != "OPEN" then .state else .mergeable end' 2>/dev/null)
    [ "$state" = UNKNOWN ] || [ -z "$state" ] || break
    sleep 10
  done
  [ "$state" = CONFLICTING ] || { say "pr #$num $slug: ${state:-unknown}"; continue; }
  limit=$(budget 3600)
  if [ "$limit" -lt 600 ]; then say "pr #$num $slug: conflicts, no time left to fix it"; continue; fi
  branch=claude-update/$to/$slug wt=$work/$to/$slug
  git -C "$repo" fetch -q origin
  if [ -d "$wt" ]; then git -C "$wt" checkout -q -B "$branch" "origin/$branch" && git -C "$wt" reset -q --hard "origin/$branch"
  else git -C "$repo" worktree add -q -B "$branch" "$wt" "origin/$branch"; fi >>"$log" 2>&1 \
    || { say "pr #$num $slug: worktree for the fix failed"; continue; }
  say "pr #$num $slug: conflicts with main, fixing ($((limit / 60)) min)"
  prompt=$(render "$lib/fix.md" NUMBER="$num" BRANCH="$branch" WORKTREE="$wt")
  RUSTC_WRAPPER=$(mise which sccache 2>/dev/null) CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 \
    claude_run "$wt" "$run/$slug.fix.jsonl" "${limit}s" "$prompt" --model "$impl_model" --effort medium
  state=$(cd "$repo" && sleep 20 && gh pr view "$num" --json mergeable -q .mergeable 2>/dev/null)
  say "pr #$num $slug: fix exit $?, now ${state:-unknown}"
  [ "$state" = MERGEABLE ] && fixed=$((fixed + 1))
  rm -rf "$wt/target" "$wt/desktop/target"
done
say "done: $n features, $opened PRs opened, $fixed fixed after main moved, $(( ($(date +%s) - started) / 60 )) min"
exit
}

#!/usr/bin/env bash
# Runs on the VPS from femme-deploy.timer. When main moves on GitHub (a PR merged), check it out
# here and rerun bootstrap.sh, which rebuilds the board and restarts the services. The result
# shows on the board until you close it.
set -uo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
# Braces: bash reads the whole block before running it, so the reset below can rewrite this file.
{
  state=$HOME/.local/state/femme
  mkdir -p "$state"
  report() {  # ok(true/false/null) message
    jq -nc --argjson ok "$1" --arg m "$2" '{ok:$ok, message:$m, at:now}' > "$state/deploy.tmp" && mv "$state/deploy.tmp" "$state/deploy.json"
  }
  git fetch -q origin main || exit 0  # offline: next tick tries again
  new=$(git rev-parse origin/main)
  [ -s "$state/deployed" ] || git rev-parse HEAD > "$state/deployed"  # first run: what sync.sh put here is live
  [ "$new" = "$(cat "$state/deployed")" ] && exit 0
  echo "$new" > "$state/deployed"  # before deploying: a broken commit is tried once, not every tick
  git reset -q --hard "$new"
  what=$(git log -1 --format='%h %s')
  report null "deploying $what"
  if out=$(HERDR_VERSION=$(herdr --version 2>/dev/null | cut -d' ' -f2) ./server/bootstrap.sh 2>&1); then
    report true "deployed $what"
  else
    report false "$what failed to deploy (full log: journalctl --user -u femme-deploy)
$(tail -n 5 <<<"$out")"
  fi
  echo "$out"
  exit
}

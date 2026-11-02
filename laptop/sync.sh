#!/usr/bin/env bash
# Laptop -> VPS: this repo, agent + herdr config. Safe to rerun.
# Project folders sync on demand, when you fork into them (sync-dir.sh).
# Never copies credentials: log in on the VPS once (see README).
set -euo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
. ./.env
r() { rsync -az --mkpath "$@"; }

r --delete --exclude=board/target --exclude=tui/target ./ "$HOST:femme/"  # bootstrap.sh builds the board and the TUI there

cd ~
r -R --ignore-missing-args \
  .claude/{CLAUDE.md,rules,skills,agents,hooks,settings.json,keybindings.json,plugins} \
  .config/herdr/{config.toml,plugins,plugins.json} .config/mise/config.toml \
  "$HOST:"

# Claude's first-run flags, folder trust and the MCP servers that work headless.
# Merged into the VPS copy, so its own login (oauthAccount) stays.
jq --arg mcp "${CLAUDE_MCP:-}" '{theme, hasCompletedOnboarding, lastOnboardingVersion,
    projects: (.projects // {} | map_values({hasTrustDialogAccepted})),
    mcpServers: (.mcpServers // {} | with_entries(select(.key as $k | $mcp | split(" ") | index($k))))}' ~/.claude.json \
  | ssh "$HOST" 'f=~/.claude.json; [ -s $f ] || echo {} > $f; jq -s ".[0] * .[1]" $f - > $f.tmp && mv $f.tmp $f'

# The VPS runs the laptop's herdr version (the laptop's client talks to it); without herdr here, the one in .env.
v=$(herdr --version 2>/dev/null | cut -d' ' -f2) || v=
v=${v:-${HERDR_VERSION:?herdr is not installed here: set HERDR_VERSION in .env}}
ssh "$HOST" "HERDR_VERSION=$v ~/femme/server/bootstrap.sh"

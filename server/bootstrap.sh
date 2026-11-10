#!/usr/bin/env bash
# Runs on the VPS (sync.sh copies this repo to ~/femme first). Safe to rerun.
set -euo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
. ./.env
export PATH="$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
# FEMMED (docs/plan-desktop.md): 0 the herdr-based setup; 1 the Rust daemon (femmed) runs beside it, for the
# desktop app; 2 femmed replaces the board, the worker and herdr (M1 cutover).
femmed=${FEMMED:-0}

# Toolchains + claude/codex/pi/gh from the synced ~/.config/mise/config.toml
command -v mise >/dev/null || curl -fsSL https://mise.run | sh
grep -q 'mise activate' ~/.bashrc || echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc
mise install -y || echo "WARN: some mise tools failed, see above" >&2
# Without a mise list from the laptop, the two the board and worker need: Claude Code (its own installer)
# and the GitHub CLI (Ubuntu's package).
command -v claude >/dev/null || curl -fsSL https://claude.ai/install.sh | bash || echo "WARN: Claude Code did not install" >&2
command -v gh >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq gh; } || echo "WARN: gh did not install" >&2
# Optional (MOSH=1): a phone terminal over mosh (docs/notifications.md); UDP 60000-61000 must reach the server.
[ "${MOSH:-0}" != 1 ] || command -v mosh-server >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq mosh; } || echo "WARN: mosh did not install" >&2

# Every Claude here runs as `claude --dangerously-skip-permissions` (same mode, no dialog): nobody is
# at the keyboard to answer prompts. sync.sh copies the laptop's settings over this file first.
mkdir -p ~/.claude; f=~/.claude/settings.json; [ -s $f ] || echo {} > $f
# Every session writes ~/.claude/debug/<session>.txt, so a turn that "hangs" has a timestamped record
# (DEBUG_SDK is Claude's own switch; DEBUG=1 would leak into make and Django). A stream silent for
# 60 s is retried, instead of the default 180 s.
jq '.permissions.defaultMode = "bypassPermissions" | .skipDangerousModePermissionPrompt = true
  | .env.DEBUG_SDK = "1" | .env.CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS = "60000"' $f > $f.tmp && mv $f.tmp $f

mkdir -p ~/.local/bin
# A macOS laptop's folders live under /Users/<you>; synced folders keep their path here, so /Users points at /home.
[ -e /Users ] || sudo ln -s /home /Users
# Linked laptops (laptop/systemd/femme-link.service) forward their sshd (for `pc`) and, without femmed,
# their herdr socket for the board into this dir. sshd replaces a stale socket on reconnect and drops
# a dead link (lid shut) in ~45 s.
mkdir -p ~/.config/herdr/remote
if [ "$femmed" != 2 ]; then
  # Same herdr version as the laptop: the laptop's client talks to this server.
  want=${HERDR_VERSION:?pass HERDR_VERSION}
  [ "$(herdr --version 2>/dev/null)" = "herdr $want" ] || {
    curl -fsSL -o ~/.local/bin/herdr "https://github.com/herdrdev/herdr/releases/download/v$want/herdr-linux-$(uname -m)"
    chmod +x ~/.local/bin/herdr
  }
  # Non-interactive ssh (`ssh agentbox herdr ...`) has no ~/.local/bin on PATH.
  sudo ln -sf ~/.local/bin/herdr /usr/local/bin/herdr
  # Integrations need each agent's config dir, which only appears after its first run.
  mkdir -p ~/.codex ~/.pi/agent ~/.config/opencode
  for a in claude codex pi opencode; do herdr integration install "$a" || echo "WARN: $a integration failed" >&2; done

  mkdir -p ~/.config/herdr  # absent when the laptop has no herdr config to sync
  grep -q '^\[experimental\]' ~/.config/herdr/config.toml 2>/dev/null \
    || printf '\n[experimental]\n# single-user box: keep screen history across reboots\npane_history = true\n' >> ~/.config/herdr/config.toml
  # Instinct's terminals run in a herdr session of their own (server/systemd/herdr-instinct.service), so a
  # laptop can list them as a machine tab of their own; the board lists them the same way as a
  # laptop's, tagged instinct, through this link.
  ln -sfn ~/.config/herdr/sessions/instinct/herdr.sock ~/.config/herdr/remote/instinct.sock
fi
printf 'StreamLocalBindUnlink yes\nClientAliveInterval 15\nClientAliveCountMax 3\n' | sudo tee /etc/ssh/sshd_config.d/20-femme-link.conf >/dev/null
if sudo sshd -t; then sudo systemctl reload ssh; else sudo rm -f /etc/ssh/sshd_config.d/20-femme-link.conf; fi

# `pc` (pc.sh): agents here build and test on the laptop, in the same synced folder, over that
# same link (it also forwards the laptop's sshd as <hostname>.ssh; pc-setup.sh on the laptop).
[ -f ~/.ssh/id_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f ~/.ssh/id_ed25519 -C femme-vps
pc_host=${PC_HOST:-$(find ~/.config/herdr/remote -maxdepth 1 -type s -name '*.sock' -printf '%f\n' 2>/dev/null | sed 's/\.sock$//')}
case $pc_host in
  '') echo "NOTE: no linked laptop yet, so no 'Host pc': run pc-setup.sh on it (or set PC_HOST in .env)" >&2 ;;
  *$'\n'*) echo "WARN: several laptops are linked: set PC_HOST in .env to the one pc should use" >&2 ;;
  *) { [ -f ~/.ssh/config ] && sed '/^# femme pc start$/,/^# femme pc end$/d' ~/.ssh/config
       printf '# femme pc start\nHost pc\n  ProxyCommand nc -U %s/.config/herdr/remote/%s.ssh\n  User %s\n  IdentityFile ~/.ssh/id_ed25519\n  IdentitiesOnly yes\n  ControlMaster auto\n  ControlPath ~/.ssh/cm-pc\n  ControlPersist 10m\n  ServerAliveInterval 15\n  ServerAliveCountMax 2\n  ConnectTimeout 5\n  StrictHostKeyChecking accept-new\n# femme pc end\n' "$HOME" "$pc_host" "$(id -un)"
     } > ~/.ssh/config.new && mv ~/.ssh/config.new ~/.ssh/config && chmod 600 ~/.ssh/config ;;
esac
install -m755 server/pc.sh ~/.local/bin/pc
# `claude` typed in a FEMME shell pane opens a daemon session (claude-shim.sh); anywhere else it is the real claude.
install -m755 server/claude-shim.sh ~/.local/bin/claude
# `claude-deepseek`: Claude Code on DeepSeek through OpenGateway, once ~/.config/opengateway/key exists.
install -m755 server/claude-deepseek.sh ~/.local/bin/claude-deepseek
# The same rule for every agent: Claude reads ~/.claude/rules, Codex its global ~/.codex/AGENTS.md.
mkdir -p ~/.claude/rules && cp server/rules/pc.md ~/.claude/rules/femme-pc.md
{ [ -f ~/.codex/AGENTS.md ] && sed '/^<!-- femme pc start -->$/,/^<!-- femme pc end -->$/d' ~/.codex/AGENTS.md
  printf '<!-- femme pc start -->\n'; cat server/rules/pc.md; printf '<!-- femme pc end -->\n'
} > ~/.codex/AGENTS.new && mv ~/.codex/AGENTS.new ~/.codex/AGENTS.md

units="herdr-server herdr-instinct herdr-notify"
case $femmed in 1) units="$units femmed" ;; 2) units=femmed ;; esac
if [ -n "${BOARD_URL:-}" ] && [ "${BOARD_PUBLIC:-0}" = 1 ]; then
  # Optional (BOARD_PUBLIC=1): the board on the internet, for agents that call the API from elsewhere.
  # Caddy terminates HTTPS for the board (automatic certificate for BOARD_URL's host).
  command -v caddy >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq caddy; }
  # encode: the browser terminal polls the screen several times a second; compressed it is ~5x smaller.
  # timeouts: the board takes one request at a time, so a client trickling a body may hold it 60 s, not forever.
  # max_size: a 20 MB upload, base64'd. Headers: HSTS; no Referer, the magic link's token is in its URL.
  sudo tee /etc/caddy/Caddyfile >/dev/null <<EOF
{
	servers {
		timeouts {
			read_header 10s
			read_body 60s
		}
	}
}
${BOARD_URL#https://} {
	encode zstd gzip
	request_body {
		max_size 30MB
	}
	header {
		Strict-Transport-Security "max-age=31536000"
		Referrer-Policy no-referrer
		X-Content-Type-Options nosniff
		X-Frame-Options DENY
	}
	reverse_proxy 127.0.0.1:8787
}
EOF
  sudo systemctl reload-or-restart caddy
fi
if [ "$femmed" != 0 ] || [ -n "${BOARD_URL:-}" ]; then
  # Built here, not on the laptop, so a merge to main can deploy itself (deploy.sh).
  [ -x ~/.cargo/bin/cargo ] || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --no-modify-path
fi
# Four cores shared by every agent: one build may take three, never all four (once three builds at
# once ran the box to load 28 and femmed's watchdog killed it). An existing config is the user's.
[ -e ~/.cargo/config.toml ] || { mkdir -p ~/.cargo && printf '[build]\njobs = 3\n' > ~/.cargo/config.toml; }
if [ "$femmed" != 0 ]; then
  # femmed and the per-session holder. Copy then rename: a running binary can't be written over, and
  # holders already running keep the old binary open until their session ends (plan §5, upgrade by rename).
  ~/.cargo/bin/cargo build -q --release -p femmed -p femme-holder
  for b in femmed femme-holder; do cp "target/release/$b" ~/.local/bin/$b.new && mv ~/.local/bin/$b.new ~/.local/bin/$b; done
  # Non-interactive ssh (`ssh agentbox femmed proxy`, the desktop app's fallback transport) has no ~/.local/bin on PATH.
  sudo ln -sf ~/.local/bin/femmed /usr/local/bin/femmed
  mkdir -p ~/.local/state/femme
fi
if [ "$femmed" != 2 ] && [ -n "${BOARD_URL:-}" ]; then
  ~/.cargo/bin/cargo build -q --release --manifest-path board/Cargo.toml
  # Copy then rename: the running binary can't be written over.
  cp board/target/release/femme-board ~/.local/bin/femme-board.new
  mv ~/.local/bin/femme-board.new ~/.local/bin/femme-board
  units="$units femme-board femme-worker"
fi
if [ -n "${BOARD_URL:-}" ]; then
  # cswap: the board's accounts window and femme-cswap's auto-rotation. Its own venv, pinned.
  # Ubuntu ships python3 without venv's ensurepip.
  python3 -c 'import ensurepip' 2>/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq python3-venv; }
  ~/.local/share/cswap/bin/cswap --version >/dev/null 2>&1 \
    || { python3 -m venv --clear ~/.local/share/cswap && ~/.local/share/cswap/bin/pip install -q claude-swap==0.27.2; }
  ln -sf ~/.local/share/cswap/bin/cswap ~/.local/bin/cswap
  # The box's own login becomes account 1 (fails harmlessly before the first /login).
  cswap list --json | jq -e '.accounts | length > 0' >/dev/null || cswap add || echo "WARN: cswap add failed; log in with claude, then rerun" >&2
  # AWS CLI v2 for the agents (the instance role signs it in) and the board's accounts window. User-local, no sudo.
  aws --version >/dev/null 2>&1 || { t=$(mktemp -d) && curl -fsSL -o "$t/aws.zip" "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" \
    && unzip -q "$t/aws.zip" -d "$t" && "$t/aws/install" -i ~/.local/share/aws-cli -b ~/.local/bin --update >/dev/null && rm -rf "$t"; }
  # Optional (ACCOUNT_ROTATION=1): move to another stored Claude account when the active one passes CAP.
  [ "${ACCOUNT_ROTATION:-0}" != 1 ] || units="$units femme-cswap femme-fable-cap"
fi

mkdir -p ~/.config/systemd/user
for u in $units; do cp "server/systemd/$u.service" ~/.config/systemd/user/; done
[ "$femmed" = 0 ] || cp server/systemd/femme-sessions.slice ~/.config/systemd/user/
# Only the timer is started here: deploy.sh runs this script, and starting its own service would wait on itself.
# Optional (AUTODEPLOY=1): anyone who can merge to main can then run code on this box.
[ "${AUTODEPLOY:-0}" != 1 ] || { cp server/systemd/femme-deploy.service server/systemd/femme-deploy.timer ~/.config/systemd/user/; units="$units femme-deploy.timer"; }
# Optional (CLAUDE_UPDATES=1): at 03:00, upgrade Claude Code and open a PR per useful new feature
# (server/claude-update.sh, docs/claude-updates.md).
[ "${CLAUDE_UPDATES:-0}" != 1 ] || { cp server/systemd/femme-claude-update.service server/systemd/femme-claude-update.timer ~/.config/systemd/user/; units="$units femme-claude-update.timer"; }
# Hourly: housekeeping, and a warning (journal, ~/.local/state/femme/box-health.json, a GitHub issue
# labelled box-health) when disk, load or swap run high or a recording leaks its desktop (server/box-health.sh).
cp server/systemd/femme-box-health.service server/systemd/femme-box-health.timer ~/.config/systemd/user/
units="$units femme-box-health.timer"
# 01:15: every herdr pane's Claude that still runs an older binary than the night's upgrade is restarted
# in place (`/exit`, `claude --resume`) once it has sat idle (server/pane-restart.sh).
cp server/systemd/femme-pane-restart.service server/systemd/femme-pane-restart.timer ~/.config/systemd/user/
units="$units femme-pane-restart.timer"
systemctl --user daemon-reload
# femmed replaces the board, the worker and herdr: an upgraded box still has their units enabled.
[ "$femmed" != 2 ] || systemctl --user disable --now femme-board femme-worker herdr-server herdr-instinct herdr-notify 2>/dev/null || true
systemctl --user enable --now $units
if [ "$femmed" != 2 ]; then
  # The plugin runs from this checkout (deploy.sh keeps it at main). sync.sh just copied the laptop's
  # plugins.json over ours, which points femme at the laptop's checkout path: relink when it differs.
  if [ "$(herdr plugin list --json 2>/dev/null | jq -r '.result.plugins[] | select(.plugin_id == "femme") | .plugin_root')" != "$PWD/plugin" ]; then
    herdr plugin unlink femme >/dev/null 2>&1 || true
    herdr plugin link "$PWD/plugin" >/dev/null || echo "WARN: could not link the femme plugin from $PWD/plugin" >&2
  fi
fi
systemctl --user restart $(printf '%s\n' $units | grep -vE '^(herdr-server|herdr-instinct|femme-deploy.timer|femme-claude-update.timer|femme-box-health.timer)$')
[ "$femmed" = 2 ] || herdr server reload-config >/dev/null 2>&1 || true
systemctl --user is-active $units

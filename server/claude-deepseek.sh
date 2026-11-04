#!/bin/sh
# Claude Code on DeepSeek V4.1 Flash UltraFast through OpenGateway (docs:
# https://opengateway.ai/docs/coding-agents/claude-code). This terminal only: the cswap accounts and
# ~/.claude settings are untouched. Key: ~/.config/opengateway/key (one line, chmod 600). Installed as
# ~/.local/bin/claude-deepseek by server/bootstrap.sh.
key_file=$HOME/.config/opengateway/key
[ -s "$key_file" ] || { echo "claude-deepseek: put your OpenGateway API key in $key_file" >&2; exit 1; }
m=${OPENGATEWAY_MODEL:-deepseek/deepseek-v4.1-flash-ultrafast}
ANTHROPIC_BASE_URL=https://apis.opengateway.ai
ANTHROPIC_AUTH_TOKEN=$(cat "$key_file")
ANTHROPIC_MODEL=$m ANTHROPIC_DEFAULT_OPUS_MODEL=$m ANTHROPIC_DEFAULT_SONNET_MODEL=$m
ANTHROPIC_DEFAULT_HAIKU_MODEL=$m CLAUDE_CODE_SUBAGENT_MODEL=$m
export ANTHROPIC_BASE_URL ANTHROPIC_AUTH_TOKEN ANTHROPIC_MODEL ANTHROPIC_DEFAULT_OPUS_MODEL \
  ANTHROPIC_DEFAULT_SONNET_MODEL ANTHROPIC_DEFAULT_HAIKU_MODEL CLAUDE_CODE_SUBAGENT_MODEL
# A cswap login's OAuth token would win over the gateway key.
unset ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN
# The real claude, not the FEMME pane shim (which would open a daemon session instead).
exec "$HOME/.local/share/mise/installs/claude/latest/claude" "$@"

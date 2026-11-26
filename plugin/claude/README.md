# FEMME for Claude Code

This plugin connects Claude Code on your laptop to your FEMME box, the server where FEMME
runs your Claude Code, Codex and Pi agents. Claude can then see and drive the chats on the
box, and you can hand the chat you are in over to the box.

## Install

In Claude Code:

    /plugin marketplace add Amore-Labs-Inc/femme
    /plugin install femme@femme

When it asks for **Box SSH host**, give the host you `ssh` to (default `agentbox`). Change it
later with `/plugin` or `claude plugin configure femme`.

## What Claude can do then

- List the chats on the box, read one, start a new one in a folder, or send a message to one
  (the `femme` MCP server, which is `femmed mcp` run on the box over ssh).
- Send this chat to the box and keep going there: ask "send this chat to my box"
  (the `send-to-box` skill). Your project folder goes to the same path on the box, kept in
  two-way sync when `mutagen` is installed (copied once otherwise), and the conversation
  opens as a FEMME chat you can follow in the desktop or phone app.

## Requirements

- FEMME installed on the box (`~/.local/bin/femmed` there).
- SSH key login to the box with no password prompt: `ssh <host> true` must just work.
- `rsync` on the laptop; `mutagen` for live folder sync (optional).
- Same home path on both machines for sending chats (folders and the session file go to the
  same absolute path on the box).

# Notifications

FEMME pushes a notification to your phone when an agent needs you (it is waiting for an answer) or is done. It uses [ntfy.sh](https://ntfy.sh), free and with no account.

1. Install the **ntfy** app (iOS or Android).
2. Subscribe to the `NTFY_TOPIC` from your `.env`. `install.sh` made a random one.

Anyone who knows the topic can read your notifications, so keep it secret. To use your own ntfy server instead, change the URL in `server/notify.sh`.

## A terminal on the phone (optional)

For when you want to watch or type into the agents yourself. [mosh](https://mosh.org) keeps the session through wifi/cellular switches and sleep, and echoes your typing before the server answers.

1. `MOSH=1` in `.env`, then `./laptop/sync.sh` (installs mosh on the server). On AWS, `./laptop/provision.sh` again opens UDP 60000-61000 in the security group; on your own server, open that range in its firewall.
2. Install **Moshi** (iOS) or **Blink Shell**. Generate a key in the app and copy the public key. On the laptop: `ssh <HOST> 'cat >> ~/.ssh/authorized_keys'`, paste, Ctrl-D.
3. Add the host in the app: `<your user>@<IP from .env>`, mosh on, command `herdr` (the multiplexer: every pane as it is) or `femme-tui` (the agent list, one agent's screen and a prompt box, made for a narrow screen; see [TUI](tui.md)).

SSH stays key-only; mosh starts over SSH and only then moves to UDP, so this opens nothing a key does not already open.


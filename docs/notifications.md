# Notifications

FEMME pushes a notification to your phone when an agent needs you (it is waiting for an answer) or is done. It uses [ntfy.sh](https://ntfy.sh), free and with no account.

1. Install the **ntfy** app (iOS or Android).
2. Subscribe to the `NTFY_TOPIC` from your `.env`. `install.sh` made a random one.

Anyone who knows the topic can read your notifications, so keep it secret. To use your own ntfy server instead, set `NTFY_URL` in `.env` (and change the URL in `server/notify.sh`).

## Claude's own pushes

Claude Code has a `PushNotification` tool ("Push when Claude decides" in `/config`): Claude tells your phone in its own words that a long task is done or that it needs a decision. Claude Code itself only delivers it through Remote Control to the Claude app, so FEMME sends it through ntfy instead: to `NTFY_TOPIC` and to every phone paired with the FEMME app that gets "needs you" pushes for that session (a muted session and quiet hours stay silent). The title is the session's label or its folder, and a tap opens the session.

- A FEMME chat: every `PushNotification` call is sent.
- A Claude in a terminal that FEMME mirrors: only when Claude Code answers that its own push was not sent for lack of Remote Control. When it held the push back because you are at that terminal, or you turned the setting off, FEMME sends nothing.

The chat shows the call as `🔔 push · <message>` with `sent to your phone`, or `not sent:` and the reason.

## A terminal on the phone (optional)

For when you want to watch or type into the agents yourself. [mosh](https://mosh.org) keeps the session through wifi/cellular switches and sleep, and echoes your typing before the server answers.

1. `MOSH=1` in `.env`, then `./laptop/sync.sh` (installs mosh on the server). On AWS, `./laptop/provision.sh` again opens UDP 60000-61000 in the security group; on your own server, open that range in its firewall.
2. Install **Moshi** (iOS) or **Blink Shell**. Generate a key in the app and copy the public key. On the laptop: `ssh <HOST> 'cat >> ~/.ssh/authorized_keys'`, paste, Ctrl-D.
3. Add the host in the app: `<your user>@<IP from .env>`, mosh on, command `herdr`.

SSH stays key-only; mosh starts over SSH and only then moves to UDP, so this opens nothing a key does not already open.


# Troubleshooting

Start with `./doctor.sh`: it checks the laptop, the server, the services, the sign-ins and the board, and prints the fix for each problem. Logs of a service on the server: `ssh <alias> journalctl --user -u <service> -n 50` (services: `herdr-server`, `herdr-notify`, `femme-board`, `femme-worker`, `femme-cswap`, `femme-fable-cap`, `femme-deploy`).

## Common errors

| You see | Why | Do |
|---|---|---|
| `doctor.sh`: herdr versions differ | the server runs the laptop's herdr version, and the laptop updated | `./laptop/sync.sh` |
| Claude in a server terminal asks you to log in | the server has no Claude login yet, or it expired | `ssh <alias>`, run `claude`, then `/login` |
| Board card: `could not clone owner/repo` | the server's GitHub CLI is not signed in, or has no access to that repo | `ssh <alias> 'gh auth login && gh auth setup-git'` |
| No sign-in email | no `RESEND_API_KEY`, or `MAIL_FROM` is not a sender your Resend domain allows | the link is in `journalctl --user -u femme-board -n 20` on the server; fix `.env`, then `./laptop/sync.sh` |
| `this link has expired or was already used` | sign-in links work once, for 15 minutes | ask for a new one (one a minute) |
| `femme-board` restarts over and over | `BOARD_EMAIL` or `BOARD_URL` is empty | set both in `.env`, then `./laptop/sync.sh` |
| Public board: no HTTPS, certificate errors | the DNS A record does not point at the server yet, a proxy sits in front, or ports 80/443 are closed | fix the record (DNS only), open the ports, then `./laptop/sync.sh` |
| `laptop not linked` (drop box, `pc`) | the laptop is asleep, or `femme-link` is not running | wake it; `./laptop/laptop-setup.sh` once; on Linux `systemctl --user status femme-link`, on macOS `~/Library/Logs/femme-link.log` |
| `pc: ... is not shared from the laptop` | that folder is not in sync | on the laptop: `sh laptop/sync-dir.sh <folder>` |
| A file shows a conflict, neither side changes | both sides edited it; Mutagen pauses it instead of overwriting | `mutagen sync list`, keep the right copy, delete the other |
| Agents pause with `paused at N% of the 5-hour window` | the worker leaves the rest of the window to you (`CAP` in `.env`) | wait for the reset, raise `CAP`, or turn on account rotation |
| `rsync is too old (macOS ships openrsync)` | macOS's built-in rsync lacks options FEMME uses | `brew install rsync`, open a new terminal, run it again |
| `./install.sh`: `must run Ubuntu 24.04 on x86_64 or arm64` | another OS or release | use Ubuntu 24.04 |

## When a turn takes forever

It is rarely the network. A long silence is, in order of likelihood: the model thinking (`effortLevel: xhigh` thinks for minutes with nothing on screen), an auto-compaction (about 2 minutes, at 310k tokens), a prompt queued behind a running turn (Esc stops the turn first), or a usage cap (the accounts window shows it). Every session writes `~/.claude/debug/<session>.txt` (`bootstrap.sh` sets `DEBUG_SDK=1`), so the next long wait has a timestamped record. What the last hour's requests waited for, by session and time:

```sh
find ~/.claude/debug -name '*.txt' -mmin -60 -exec awk '/API REQUEST|first byte|engine\] turn .* end|autocompact|api_error|[Rr]etry|watchdog/ {
  print substr(FILENAME, length(FILENAME) - 11, 8), substr($0, 12, 8), substr($0, index($0, "] ") + 2, 110) }' {} + | sort -k2
```

A stream with no bytes for 60 s (`CLAUDE_BYTE_STREAM_IDLE_TIMEOUT_MS`) is retried once, instead of after the default 180 s; the API pings every few seconds while the model thinks, so a minute of silence is a real stall.

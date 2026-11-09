# Claude accounts

The board's **accounts** page (the ⋯ button) lists every Claude account stored with [cswap](https://pypi.org/project/claude-swap/), with each account's 5-hour and 7-day usage.

- **+ add**: open the sign-in link, log in on claude.ai with the new account, paste the code it shows into the board. The board signs in a separate config dir, runs `cswap add` from there, and leaves the VPS's live login alone. Never run `/logout`: it drops the stored accounts.
- Click an account to switch the VPS to it (`cswap switch`).
- A claude.ai sign-in ends about 30 days after it was made, used or not (`loginExpiresAt` in `cswap list --json`, since Claude Code 2.1.318). The card says so from 10 days before (`sign-in ends in 7 d`, amber), turns red under two days and once it has ended, and the red row's **sign in again** runs the + add flow: `cswap add` refreshes the stored account in place. The femme menu's Accounts entry shows a red dot while any row is red. `femme-fable-cap.service` holds an account whose sign-in has ended out of rotation (and moves the box off it) until it is signed in again.
- Optional, `ACCOUNT_ROTATION=1` in `.env`: `femme-cswap.service` runs `cswap auto --threshold $CAP`: when the active account passes `CAP` it switches to the account with the most left, so agents only pause once every account is past `CAP` (keep it 50-99). `femme-fable-cap.service` does the same for the Fable weekly limit at `FABLE_CAP` (96%): an account past it is held out of rotation (`cswap disable`) until its week resets, and the box moves to the account with the most left. If you use several accounts, following your provider's terms is up to you.
- `bootstrap.sh` installs cswap and saves the VPS's current login as account 1.
- **AWS**: the page also shows who the server's AWS CLI runs as, if anyone (see [AWS](aws.md)). `bootstrap.sh` installs the AWS CLI.

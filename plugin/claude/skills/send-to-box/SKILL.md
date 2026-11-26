---
name: send-to-box
description: Send this chat to the user's FEMME box and keep going there. Use when the user asks to send, move, hand off or continue this conversation on their box or server.
---

Run this one command from the current working directory:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/send-to-box.sh" "${CLAUDE_SESSION_ID}" "${user_config.host}"
```

It syncs (or copies) this project folder to the same path on the box, copies this conversation there, and opens it as a FEMME chat. It prints the new chat's id.

Then tell the user, in one or two lines: the FEMME chat id it printed, and that this conversation now continues on the box, where they can follow it in the FEMME desktop or phone app. If the script fails, show its error line and stop.

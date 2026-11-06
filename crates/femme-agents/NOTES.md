# femme-agents: protocol notes (prototype P3)

What the installed CLIs really do, verified 2026-11-02 against `claude` 2.1.302, `codex` 0.163.0
and `pi` 1.3.0 (this VPS). Fixtures live in `fixtures/<agent>/`: `<case>.jsonl` is the agent's raw
stdout, `<case>.stdin.jsonl` what the capture script sent, `<case>.events.json` the golden
`AgentEvent` list (reviewed by eye). Capture scripts are in `scripts/`; re-run them when the CLI
version is bumped and diff the fixtures.


## At a glance

| | Claude 2.1.302 | Codex 0.163.0 | Pi 1.3.0 (scripted model, see Pi) |
|---|---|---|---|
| Handshake | `initialize` control request, answered 1.0-3.4 s after spawn (median 1.3 s, user SessionStart hooks run first); session id only at the first turn's `system/init`, so `--session-id` is chosen by us | `initialize` + `initialized`, then `thread/start` or `thread/resume` | none; driver sends `get_state` |
| Questions | `can_use_tool` for `AskUserQuestion` always, `ExitPlanMode` only in plan mode; needs `--permission-prompt-tool stdio` (plan's flags give no `AskUserQuestion` at all) | `item/tool/requestUserInput`, needs `--enable default_mode_request_user_input` | only extension dialogs (`extension_ui_request`), no permission prompts |
| Interrupt | `interrupt` control request; `result` `aborted_*`; session stays usable | `turn/interrupt`; `turn/completed` `interrupted`; late tool completion after it | `clear_queue` then `abort`; `agent_settled{aborted}` before the `abort` response |
| Steer / Queue | no native queue: a mid-turn message is folded in at a tool boundary or runs next; driver holds `Queue`, `Steer` writes at once | `turn/steer` (needs `expectedTurnId`); second `turn/start` is injected like a steer; driver holds `Queue` | `prompt` + `streamingBehavior:"steer"`; native `follow_up` merges into one run; driver holds `Queue` |
| `set_model` | control request, no body; `user` echo "Set model to `x`"; bad model = error response | no request: remembered and sent as `model` on the next `turn/start` | `{provider, modelId}`, confirmed by the response only |
| Resume | `--resume <id>` keeps id and context; `--session-id` only for new sessions | `thread/resume` returns full history, same item ids | `--session-id <id>` (never a prefix) |
| `kill -9` then resume | recovers (live test); lost background task gives an empty `result` | not exercised beyond `thread/resume` | recovers; partial reply is lost |
| Rate limits in stream | yes: `rate_limit_event`, `utilization` is a fraction | yes: `account/rateLimits/updated` (free plan: one monthly window) | no |
| End of turn | `result` | `turn/completed` | `agent_settled` (not `agent_end`) |


## Audit follow-up: what a driver now promises

An independent audit replayed the drivers with scratch tests and found 21 problems: prompts and
answers lost on a daemon restart, a Codex handshake that wedges, ordering, status/docs mismatches.
Each fix has a regression test in `tests/regress_{claude,codex,pi,misc}.rs` (and `tests/hooks.rs`);
the part of each file above its `NEW API` marker uses only calls the crate had before and fails
on an assertion against the unfixed crate (bar a few pins of behaviour that was already right, or
doc-only, see below), the part below needs the new API.

**Re-attach.** The agent outlives the daemon. A fresh driver gets `Driver::attach(&Launch,
&Attach, &mut Out)` instead of `on_start`, then the daemon replays the spool from
`resume_offset()`. `Attach { resume_offset, agent_sid, model, answers, epoch }` is what the daemon
persisted: the offset (a previous `resume_offset()`), the ids from `Started`/`ModelChanged`, the
answers from `pending_answers()`, and the number of attaches so far. `resume_offset()` reports the
offset it was given until a line is replayed. Lines are complete (no `\n` inside; debug builds
assert it), so the offset is always a line start. A replayed line writes nothing to stdin except
in two Codex cases (finish a handshake the old daemon died in; refuse an unknown server request
again). Claude resumes with `Attach.agent_sid` for `tui()` too.

**Held prompts.** `Queue`/`InterruptThenSend`/early `Steer` prompts live in driver memory.
`held_prompts()` lists them in the order they will go out (`HeldPrompt { text, how, queued_at_ms }`);
persisting them is the daemon's job (it calls `prompt()` again after `attach`). A prompt that will
never run is announced once, with its text: `AgentEvent::PromptDropped { text, how, reason }`,
`Exited` on `on_exit` (everything written but unacknowledged, then everything held), `Refused(msg)`
when the agent drops it itself: Claude `command_lifecycle` `cancelled`/`discarded`/`refused` or
`session_state_changed idle` for a command it acknowledged and never ended; Codex `turn/start`
error; Pi `clear_queue` (steers it drops on interrupt) or a refused `prompt`.

**Answers.** Claude holds the answers of a multi-question request until the last one is in; Codex and
Pi write each at once and forget it; Pi never echoes an answer at all. `pending_answers()` returns
`(request_id, Answer)` for answered questions a replay can still reach; `Attach.answers` hands them
back. A replayed question with such an id is not asked again (no `QuestionAsked`, no `Blocked`, no
`QuestionResolved`), `answer()` refuses it with `UnknownRequest`, and a request that is only part
answered asks the rest. An answer is final: a second `answer()` for a part is refused too.

**Request ids.** Codex and Pi number their own requests. The first process keeps `n` / `f<n>`;
after an attach the ids carry `Attach.epoch` (>= 1): Codex `(epoch << 32) + n`, Pi `f<epoch>-<n>`.
Codex matches a response to a request **by id only**: a replayed response of an earlier
incarnation (`turn/start`, rate-limit read) is ignored, not read by its shape. Exceptions, only for
a driver that did not start the process: a `thread/start`/`thread/resume` result while the thread is
unknown, and an `initialize` result while neither thread nor handshake is known, which means the old
daemon died inside the handshake: the driver sends `initialized` and starts or resumes the thread
(ponytail: if the old daemon had already sent `thread/start`, this asks for a second, empty thread;
the later reply wins, and no prompt ever ran in either).

**Codex unknown server requests** (any method but `item/tool/requestUserInput` and the approvals)
are answered with JSON-RPC -32601 `femme does not handle <method>` (and still emitted as `Raw`);
a question request without questions with -32602. Codex used to wait for them forever.

**Claude.**
* `queued` frames stay in the resume offset until the message starts (during a running turn the
  anchor is that turn's start): a restart replays them and the new driver still knows the CLI is
  busy. `session_state_changed idle` clears commands the CLI acknowledged and never ended; a
  message of ours the CLI has not mentioned yet is never cleared as stale.
* A `result` with `is_error` and no turn (resuming an unknown session, golden `error_resume`) is an
  `Event` item `claude failed before it ran a turn: <message>`, not a made-up `TurnStarted` +
  `TurnEnded`: `TurnEnded` always has its `TurnStarted`. The empty background-task `result` stays
  silent.
* A `tool_result` for a call we do not hold (late, or never seen) adds only its `ToolResult` item:
  it no longer overwrites the call with a blank name, calls `activity()` or opens a turn.
* Question outcomes: `Answered` with what the user chose, also a deliberate Cancel/Deny/"Keep
  planning" (live and on replay, through the echoed control response); `Failed` with
  `QuestionItem.reason` (new, additive) when it was never answered (turn interrupted, process
  exited, request withdrawn or replaced). An empty multi-select pick is `Bad`.
* A repeated `can_use_tool` request id with the same `tool_use_id` is ignored (an answered question
  is not reopened); a new call under a used id replaces the old one. A second `content_block_start`
  at an index nobody stopped fails the first block, so it cannot pin the resume offset.

**Pi.** `interrupt()` (and `InterruptThenSend`) answers every open dialog with `cancelled: true`
before `clear_queue` + `abort`, otherwise the extension stays blocked on it. Usage sums
saturate. A `tool_execution_end` for an unknown call adds only its result. Pi's own steer queue is
cleared on interrupt (announced as `PromptDropped`); the driver's held prompts are kept.

**Ordering, interrupt, exit (all three).** Held `InterruptThenSend` prompts keep the order they
were given (behind earlier ones, ahead of `Queue`); prompts still held when a turn is interrupted
are **kept** and run after it; `interrupt()` while nothing runs is `Ok` and writes nothing; after
`on_exit` `prompt`/`interrupt`/`answer`/`set_model` return `Error::State` and write nothing.
Prompts the agent turns down because the run already ended (Codex `turn/steer` error, a Pi `prompt`
refused while a run it had not reported goes on) run next, ahead of the held ones, in the order
given.
`TurnEnded` is never emitted without an open turn (Codex duplicate `turn/completed`).

**Docs pinned by the captures** (they could not fail before; they fail if the behaviour changes):
`Status(Working)` comes before `TurnStarted` when work was queued before the turn (Claude's `queued`
frame, every Claude capture but `usage`), after it everywhere else; `ItemStarted.preview` is the
tool's name (Claude, Pi), the command line (Codex command execution) or the text so far (Codex
text), empty for Claude/Pi text and thinking, never more than 512 bytes (`lib.rs`).

**Test infrastructure.** `golden()` fails on a missing golden unless `UPDATE_GOLDEN=1` (it used to
create it, so a new case passed against itself). Fixtures are scrubbed of the capture host's
multiplexer name, the command rewriter in front of hook shells, scratch paths and the account plan
name; `fixtures_name_no_machine_tool_or_account` scans every fixture file for them, and the capture
scripts scrub them (`capture-claude.py` account, `capture-codex.py` userAgent terminal token,
`capture-hooks.py` `/tmp/claude-<uid>/` and `rtk `). `scripts/capture-hooks.py` no longer overwrites
`~/.codex/config.toml`/`hooks.json` with a snapshot: it copies each to `<file>.femme-capture-bak`
first, puts the file back (atomic rename) only when the run added nothing but lines it owns and
removed none, deletes the backup then, and otherwise leaves both and says so; a backup left by a
crashed run stops the next capture (`the_capture_script_backs_up_and_only_restores_what_it_added`).

**Left as it is, on purpose.** Held prompts are not persisted by this crate; Pi's `given` map is not
pruned; a Claude approval refused with `Cancel` and with `Option(1)` look alike on replay
(`Option(1)`); `Attach.model` is unused by Codex (it keeps the model on its side); a Codex `Stop`
hook cannot tell that a question is still open (below); `is_interrupt: true` has never been seen
(below).


## Codex

`codex app-server` (codex-cli 0.163.0), driver `src/codex.rs`, fixtures `fixtures/codex/`
(`scripts/capture-codex.py`, model `gpt-6-luna` + `-c model_reasoning_effort="low"` for the
captures only), method index `fixtures/codex/schema.json` (`codex app-server generate-json-schema`,
stable API, minified, 376 KB).

**Wire.** One JSON object per line, JSON-RPC without the `"jsonrpc"` member: requests
`{id, method, params?}`, responses `{id, result}` / `{id, error: {code, message, data?}}`,
notifications `{method, params}` (server ones also carry `emittedAtMs`). Server-request ids are
integers starting at 0 in every process. `codex app-server` has no `-a`, `-s`, `-m` or
`--dangerously-bypass-approvals-and-sandbox` (only `-c`, `--enable/--disable`, `--listen`, ...);
stdio is the default transport.

**Handshake.** `{"id":1,"method":"initialize","params":{"clientInfo":{"name":"femme","title":"FEMME","version":..},"capabilities":{"optOutNotificationMethods":[..]}}}`
→ `{userAgent, codexHome, platformFamily, platformOs}`; then `{"method":"initialized"}` and
`thread/start {cwd, approvalPolicy:"never", sandbox:"danger-full-access", model?}` →
`{thread:{id (UUIDv7), sessionId, path (rollout file), turns:[], ..}, model, approvalPolicy, sandbox:{type:"dangerFullAccess"}, reasoningEffort, ..}`
followed by a `thread/started` notification. A prompt before the thread response is buffered.
`optOutNotificationMethods` works without `experimentalApi`; we drop `turn/diff/updated` (whole
turn diff after every edit: O(n²)), `turn/moderationMetadata` (~2 KiB per model call), output
deltas, `remoteControl/status/changed` (carries the installation id), `account/updated`, hooks,
read-state and MCP startup status (`codex::OPT_OUT`).

**Approvals off.** Defaults without overrides are `on-request` + read-only. FEMME passes both
`-c approval_policy="never" -c sandbox_mode="danger-full-access"` on the command line (also covers
the TUI) and the same in the thread params. Verified: shell commands and file writes produce no
`item/commandExecution/requestApproval` / `item/fileChange/requestApproval`. Should one arrive
anyway it becomes an Accept/Decline question (`<itemId>:approval`, answer `{"decision":"accept"|"decline"|"cancel"}`).
Not handled (→ `Raw`, the turn waits until interrupted): `item/permissions/requestApproval`,
`mcpServer/elicitation/request`, `item/tool/call` (only for client-registered dynamic tools).

**Turn stream** (`ok`): `turn/start {threadId, input:[{type:"text",text}]}` → `{turn:{id, status:"inProgress"}}`,
`thread/status/changed` active, `turn/started {threadId, turn}`, `item/started|completed`
userMessage (UUIDv7 id, `content:[{type:"text",text,text_elements}]`), `item/started` agentMessage
(`msg_...`, `phase: final_answer|commentary`), `item/agentMessage/delta {itemId, delta}`,
`item/completed {item:{text}}`, `thread/tokenUsage/updated`, `account/rateLimits/updated`,
`thread/status/changed` idle, `turn/completed {turn:{id, status, error, durationMs}}`.
Items: `commandExecution` (`exec-<uuid>`, `command` is the full `/bin/bash -lc '...'`, `cwd`,
`aggregatedOutput`, `exitCode`, `status`) → ToolCall `commandExecution {command, cwd}` + ToolResult
(output, `[exit code N]` appended and `is_error` when non-zero); `fileChange`
(`changes:[{path, kind:{type:add|delete|update}, diff}]`) → ToolCall `fileChange {changes}` +
result listing `<kind> <path>`; `reasoning` → Thinking (summary parts joined by a blank line);
`plan` → Plan; `mcpToolCall` / `dynamicToolCall` / `webSearch` / `collabAgentToolCall` and any
unknown type → ToolCall named after it; `contextCompaction`, review mode, hook prompts → Event.
`turn/plan/updated` → Plan item `<turnId>:plan` (checklist). Fast commands send `item/started`
only when they are already done.

**Usage / rate limits.** `thread/tokenUsage/updated {tokenUsage:{total, last, modelContextWindow}}`
after every model call; the `last` values of a turn sum to its usage (input = `inputTokens` −
`cachedInputTokens`, cache_read = `cachedInputTokens`, cache_write = `cacheWriteInputTokens`,
always 0 so far). `account/rateLimits/updated {rateLimits:{limitId:"codex", primary:{usedPercent, windowDurationMins, resetsAt}, secondary, credits, planType}}`
also after every model call (the driver passes on changes only). The account here is on the free
plan: one 43200-minute window, so kind `monthly`; 300 → `session`, 10080 → `weekly`, other limit
ids prefix the kind (`<limitId>:session`). `CodexDriver::read_rate_limits` sends
`account/rateLimits/read` (no params, no model turn) → `{rateLimits, rateLimitsByLimitId, rateLimitResetCredits:{credits:[{id,..}]}, accountId, ..}`
(ids scrubbed in the fixture).

**Questions.** The `request_user_input` tool exists outside plan mode only with
`--enable default_mode_request_user_input` (feature "under development": it emits a `warning`
notification unless `-c suppress_unstable_features_warning=true`). No `experimentalApi` needed.
Server request `{"method":"item/tool/requestUserInput","id":0,"params":{threadId, turnId, itemId:"call_...", questions:[{id, header, question, isOther, isSecret, options:[{label, description}]|null}], isBlocking, autoResolutionMs}}`,
preceded by `thread/status/changed` `activeFlags:["waitingOnUserInput"]`; no item events for that
tool call. Answer: `{"id":0,"result":{"answers":{"<question id>":{"answers":["Red"]}}}}`; then
`serverRequest/resolved {threadId, requestId:0}`. `request_id` = `itemId` (or `itemId#n` for
several questions in one request), not the JSON id: those restart at 0 per process. `isOther` →
`free_text`; `isSecret` is not surfaced. Plan mode (`collaborationMode` on `turn/start`) needs
`experimentalApi` and is not used.

**Interrupt.** `turn/interrupt {threadId, turnId}` → `{}`, `turn/completed` status `interrupted`
(error null). A command still running finishes in the background: its `item/completed` arrives
after `turn/completed` (the driver marks open items Failed at turn end, the late completion then
upserts Completed + result). With a question pending, `serverRequest/resolved` arrives after
`turn/completed`. Interrupt before the turn id is known waits for the `turn/start` response.

**Steer / queue.** `turn/steer {threadId, input, expectedTurnId}` → `{turnId}`; the text shows up as
a userMessage item inside the running turn. There is no queue in the stable API (`thread/queue/*`
is experimental): a second `turn/start` during a turn returns the *same* turn id and is injected
like a steer, so the driver buffers `Queue` prompts and sends them after `turn/completed`.

**Resume.** `thread/resume {threadId, cwd, approvalPolicy, sandbox, model?}` returns the full
history (`thread.turns[].items`, `itemsView:"full"`, same item ids as live, so backfilling them as
`ItemCompleted` is idempotent), sends no `thread/started` but `thread/goal/cleared` and a
`deprecationNotice` ("Full-history hydration is deprecated for paginated threads; use
`excludeTurns: true`, then page with `thread/turns/list` and `thread/items/list`": the upgrade path
once full hydration goes; the notice is logged, not shown). A failed resume falls back to
`thread/start` (new `Started`). `thread/loaded/list {}` → `{data:[thread ids], nextCursor}`: a driver
re-attached after a daemon restart that was not given the thread id by `attach` and has not seen it
in the replay uses it (oldest UUIDv7).

**TUI takeover.** `codex resume <FLAGS> --no-daemon [-m model] <threadId>`. Without `--no-daemon`
the TUI talks to a shared background app-server daemon that outlives the PTY.
(From `codex resume --help`: "Run without the shared background server"; the TUI itself was not
run in a PTY here.)

## Claude Code (headless `stream-json`)

Verified on `claude` 2.1.302 with `claude-haiku-5-5`; fixtures `fixtures/claude/`, driver `src/claude.rs`,
capture `scripts/capture-claude.py` (same argv as `ClaudeDriver::spawn`, checked by a test).

```
claude -p --input-format=stream-json --output-format=stream-json --verbose --include-partial-messages
       --replay-user-messages --permission-mode=bypassPermissions --permission-prompt-tool=stdio
       [--model=M] (--resume=ID | --session-id=UUID) [--settings=PATH]        env CLAUDE_CODE_SDK_READS_SESSION_STATE=1
```

### Flags: plan vs reality

| Plan | Reality |
|---|---|
| `--session-id\|--resume <id>` | Both exist and work. `--session-id` needs a UUID and only for a session that never ran; `--resume` of an id that never ran fails (`result` `error_during_execution`, `errors:["No conversation found with session ID: ..."]`, exit 1). So: first spawn `--session-id`, every later spawn `--resume`. `--resume` keeps the session id and the context; nothing is replayed on stdout. |
| (nothing) | `--permission-prompt-tool=stdio` is **required** for questions. With the plan's exact flags `AskUserQuestion` is not even in the model's tool list (it answers "the tool isn't available"), so no `can_use_tool` ever arrives. |
| (nothing) | `--replay-user-messages` is what makes the CLI echo (a) every user message with `isReplay:true` and our uuid (the `UserMessage` item comes from this echo, not from the daemon) and (b) our `control_response` (how a restarted daemon learns a question was already answered). Without it there are no echoes. |
| (nothing) | Env `CLAUDE_CODE_SDK_READS_SESSION_STATE=1` gives `system/session_state_changed {running,idle,requires_action}`. Without it only `background_tasks_changed` appears. |
| `--settings <file>` | Exists (driver passes `Launch.hooks_path`); only meant for the TUI. |

### Wire shapes (all in fixtures)

* stdin user message: `{"type":"user","message":{"role":"user","content":[{"type":"text","text":T}]},"uuid":U}`. The driver stamps a uuid on every message; the CLI answers with `command_lifecycle {command_uuid:U,state:queued|started|completed|cancelled}` and echoes it.
* stdin control: `{"type":"control_request","request_id":R,"request":{"subtype":"initialize"|"interrupt"|"set_model",...}}`; reply `{"type":"control_response","response":{"subtype":"success"|"error","request_id":R,"response"|"error":...}}` (error also has `error_code`). Unknown subtype: `error: "Unsupported control request subtype"`. Duplicate or unknown ids in a `control_response` we send are silently ignored.
* A turn: lifecycle `queued`, `started`, `system/init`, `system/status {status:"requesting",user_message_uuids}`, user echo, `stream_event`s (`message_start`, `content_block_start|delta|stop`, `message_delta`, `message_stop`), one complete `assistant` line **per content block** (same `message.id`, tool input is the final CLI-processed input), `user` lines with `tool_result`, `rate_limit_event`, `result`, lifecycle `completed`, `session_state_changed idle`. Hook noise (`system/hook_started|hook_response`) comes from the user's SessionStart hooks; ignored.
* `system/init` (session id, model, tools) is emitted at the start of **every turn**, not at process start. The `initialize` response has no session id, so the driver plans the uuid itself (`--session-id`) or takes it from `--resume`.
* `initialize` is answered after the SessionStart hooks (1 to 3 s) with a large body (commands, agents, models, account, pid, `session_state`) plus, next to `response`, `pending_permission_requests` (full `control_request` objects) and `pending_user_dialog_requests`. It can be sent again at any time (fixture `reinit`: `session_state: requires_action` and the open `AskUserQuestion` listed). That is a spool-independent way to find open questions; the driver does not need it because the spool replay recreates them (not implemented: ponytail).
* A user message sent right after `initialize` without waiting for its answer works; the CLI serializes stdin. The driver still holds prompts until the answer so ordering is its own.
* Thinking text is empty (`thinking:""` plus a `signature`; sizes only in `system/thinking_tokens`), so there are no `Thinking` items in practice. The code handles non-empty thinking.

### `can_use_tool` under `bypassPermissions` + `--permission-prompt-tool stdio`

* Only `AskUserQuestion` always arrives (`requires_user_interaction:true`, `input.questions[]` with `question`, `header`, `options[{label,description}]`, `multiSelect`; 1 to 4 questions in one request).
* `ExitPlanMode` arrives only while plan mode is on. Without plan mode it is a plain tool error ("not in plan mode") and no request. `EnterPlanMode` works under bypass (`system/status permissionMode:"plan"`); the later `ExitPlanMode` request carries `{plan, planFilePath}` (the streamed input is `{}`; the `assistant` line has the real one). Approval returns the mode to `bypassPermissions` (when launched with `--permission-mode plan` it returns to `default`, after which ordinary tools prompt too, so the driver maps every other tool to an Allow/Deny question and never auto-answers).
* Allow `AskUserQuestion`: `{"behavior":"allow","updatedInput":{<original input>,"answers":{"<question text>":"<label>"}}}`; multi-select labels joined with `", "`; free text is sent as the answer string. Decline: `{"behavior":"deny","message":"The user declined to answer."}` gives a `tool_result` with `is_error:true`. With several questions the driver sends one response after the last answer (`request_id` of each part is `<id>#<i>`).
* The CLI sends `control_cancel_request {request_id}` when the question dies (interrupt), then a synthetic rejection `tool_result`.

### interrupt / set_model / errors

* `interrupt` -> `control_response {still_queued:[]}`, then `user` "[Request interrupted by user]" (or "... for tool use"), `result` `subtype:error_during_execution, is_error:true, terminal_reason: aborted_streaming|aborted_tools`, lifecycle `cancelled`. `usage` in that result is all zero (driver reports `None`). The session stays usable. Interrupt while idle: harmless success. Extra request fields exist (`reason`, `cancel_queued`, `send_now`, `message_uuid`); unused.
* `set_model` -> success without a body; a `user` echo `<local-command-stdout>Set model to \`X\`</local-command-stdout>` follows; the next turn's `system/init.model` shows it. Bad model: `control_response` error `Model 'x' not found`, `error_code:"catalog_unknown"` (driver: Event item).
* Bad `--model`: no stream events; synthetic `assistant` (`model:"<synthetic>"`, `error:"model_not_found"`, `is_api_error_message:true`), `result` `subtype:"success"` but `is_error:true, terminal_reason:"api_error"`; exits 1 on close. Driver: `TurnEnded{Error(text)}`.
* `result.stop_reason`: `end_turn` -> EndTurn; `terminal_reason aborted*` -> Interrupted; `is_error` -> Error(result text | errors | subtype).

### Queue vs steer

A second user message mid-turn is acknowledged at once (`queued`). If the running turn has a tool boundary the CLI folds it into that turn (second echo, one `result` listing both `user_message_uuids`); a text-only turn finishes first and the message runs as its own turn with no idle in between. So the driver holds `PromptHow::Queue` until the CLI has nothing of ours in flight (the `result`, and also the window between our write and its `started` frame) and sends one at a time (true "after this turn"); `Steer` writes at once (folded at the next tool boundary, or its own turn); `InterruptThenSend` writes `interrupt` and releases the prompt after the interrupted `result`.

### Resume and `kill -9`

Killed mid-tool (`kill9_1`), then `--resume <id>` (`kill9_2`): the conversation is intact, the model knows the tool did not finish. When the dead process had a *backgrounded* shell task the new process first emits `system/task_notification {status:"stopped", summary:"Background shell command didn't finish before the previous session ended"}` and an empty `result` (`num_turns:0`, `origin.kind:"task-notification"`); the driver ignores that `result` (unit test in `claude.rs`; not in a fixture because a foreground `python3 -c sleep` does not trigger it).

### Rate limits and `/usage`

* In stream: `rate_limit_event {rate_limit_info:{status:allowed|allowed_warning|rejected, resetsAt:<unix s>, rateLimitType:five_hour|seven_day|seven_day_opus|seven_day_sonnet|seven_day_overage_included|overage, unifiedWindows:{five_hour:{utilization:0.31,resetsAt}}}}` before and after each turn. `utilization` is a **fraction** (0.48 matched `/usage` "52% used" minutes later). Driver: `RateLimit{kind:"session"|"weekly"|<key>, percent: utilization*100, resets_at}`, only when the value changed; `rejected` forces at least 100.
* `claude -p /usage --output-format stream-json --verbose --no-session-persistence` still works without a model call (`fixtures/claude/usage*`): `assistant.usage_report.rate_limits.limits[] = {kind:"session"|"weekly_scoped", group, percent:52, resets_at:"ISO8601", scope, severity, is_active}` plus human text; `result.local_command:"usage"`. The old `worker.sh` jq on that shape is still valid.

### Mapping decisions

Item ids: `<message id>:<block index>` text/thinking, tool_use id for `ToolCall`, `<id>:result` for its `ToolResult`, uuid of the `user` line for `UserMessage`/`Event`, `<tool_use_id>[#i]` for `Question`, `<tool_use_id>:ask` for plan approval/generic permission, `o<offset>` when a line has no id; all deterministic so a replay re-emits the same ids (the daemon upserts by id). `AskUserQuestion`/`ExitPlanMode` calls are hidden (`Question`/`Plan` items instead). Subagent traffic (`parent_tool_use_id != null`) runs through the same handlers with its own stream state per parent and its items carry `parent` = that `Agent` tool_use_id; it never opens/closes a turn or changes status, its AskUserQuestion/ExitPlanMode are plain tool calls, and an `o<offset>` id becomes `<parent>/o<offset>`. Ignored line types: `keep_alive`, `tool_progress`, `transcript_mirror`, `tool_use_summary`, `prompt_suggestion`, `auth_status`, `update_environment_variables`; unknown types become `Raw`.


## Pi

`pi --mode rpc` (pi 1.3.0), driver `src/pi.rs`, fixtures `fixtures/pi/` (`scripts/capture-pi.py`).
Pi had **no provider credentials** on this host (`pi --list-models`: "No models available"; the
brief assumed a login), so the captures run the real pi binary (RPC layer, agent loop, tools,
sessions, extensions) against a scripted OpenAI-compatible endpoint served by the capture script
(provider `mock`, models `mock-a`/`mock-b`, isolated `PI_CODING_AGENT_DIR` with a `models.json`,
`PI_OFFLINE=1`). Only the LLM is fake; it also makes the fixtures deterministic. `--real` uses pi's
own default model; `--serve` runs just the mock for `tests/live_pi.rs`.

### Wire shapes (all in fixtures)
- One JSON object per line both ways (split on `\n` only). Commands take an optional `id`, echoed
  in `{"type":"response","command":...,"success":bool,"data"|"error"}`. Pi prints nothing until
  it gets a command and has no handshake: the driver sends `get_state` on start (`sessionId`,
  `sessionFile`, full `model` object; `provider:"unknown"` when none is configured).
- A run: `response(prompt, disposition:"started")`, `agent_start`, `turn_start`, a `system`
  message (first run of a session only), `message_start/end` user, `message_start` assistant,
  `message_update{assistantMessageEvent: text_start|text_delta|text_end|thinking_*|toolcall_start
  {id,toolName}|toolcall_delta|toolcall_end{toolCall}}`, `message_end` (authoritative content,
  `stopReason` stop|toolUse|length|aborted|error, `usage`, `timestamp`, `durationMs`),
  `tool_execution_start{toolCallId,args}`, `tool_execution_update` (partial output),
  `tool_execution_end{result.content,isError}`, `message_*` role `toolResult`, `turn_end`, ...,
  `agent_end`, **`agent_settled{aborted}`**. `turn_*` are single LLM calls; `agent_end` is not the
  end either (retries, compaction recovery and queued messages continue the run). The driver ends
  the FEMME turn at `agent_settled` only.
- Usage per assistant message: `input` (uncached), `output`, `cacheRead`, `cacheWrite`,
  `reasoning`, `totalTokens`, `cost{...}`; the driver sums the run. Aborted requests report zeros.
- Unknown command: `success:false, error:"Unknown command: x"`; bad JSON: `command:"parse"`.

### abort / steer / follow_up / Queue
- `abort` stops the stream (`message_end stopReason:"aborted"`, `errorMessage:"Request was
  aborted"`, partial text kept), then `agent_settled{aborted:true}`, and only **then** the `abort`
  response (it waits for idle). Undelivered queued messages survive an abort and ride along with
  the next run, so the driver sends `clear_queue` first (its response lists the dropped texts;
  reported as an `Event`). Fixtures `interrupt`, `interrupt_send`.
- `steer`/`follow_up` **always queue, even when idle** (a steer sent just after the run ended waits
  for the next prompt). `prompt` with `streamingBehavior:"steer"|"followUp"` queues while streaming
  (response `disposition:"queued"`) and starts a run when idle, so the driver steers with that.
  A steer is delivered after the current LLM call and its tools, as a user message inside the same
  run (`steer`). A plain `prompt` while streaming is refused ("Agent is already processing").
- Native `follow_up` folds into the running run: one `agent_settled` for two prompts (`followup`).
  So `Queue` is buffered by the driver and sent as a `prompt` after `agent_settled` (`queue`): one
  turn per prompt; the buffer is driver memory only (`held_prompts()` lists it, `PromptDropped`
  announces what is lost; see Audit follow-up).
- A prompt arriving while pi emits `agent_settled` is deferred by pi and answered after it.

### set_model
`{"type":"set_model","provider":"mock","modelId":"mock-b"}` (two fields, not `provider/model`);
confirmed only by the response, whose `data` is the full model object (no stream event; later
assistant messages carry the new `provider`/`model`). The driver maps the response to
`ModelChanged("mock/mock-b")`; a bare id keeps the current provider. `--model` accepts
`provider/id` or a pattern.

### Sessions, resume, kill -9
- `sessionId` is a UUIDv7; the file is `~/.pi/agent/sessions/--<cwd mangled>--/<ts>_<id>.jsonl`,
  written at the first user message. `--session-id <id>` opens exactly that session (verified: a
  second process answered "banana", `resume`), or creates it when missing. `--session <path|partial
  id>` also works, but partial ids are prefix matches and UUIDv7 ids made close together share
  prefixes, so never pass a prefix. `--continue` picks the most recent session of the cwd (not
  necessarily ours); `--resume` is an interactive picker only. `Started.agent_sid` = `sessionId`,
  `Launch::resume` takes it (a path goes to `--session`), takeover runs `pi --session-id <id>`.
- `kill -9` mid-turn: no further output; the resumed session (`kill9_resume`) holds the user
  message but not the partial assistant reply (`messageCount` 2), and the next prompt works.
- `pi -p` waits for stdin when it is not a TTY: give it `/dev/null`.

### Extension UI (questions)
No permission prompts. Extensions ask through `ctx.ui`: `extension_ui_request{id (random UUID),
method: select{title,options}|confirm{title,message}|input{title,placeholder}|editor{title,prefill},
timeout?}`; pi blocks the extension until `{"type":"extension_ui_response","id",
"value"|"confirmed"|"cancelled":true}`. Pi never echoes the answer and says nothing when a
dialog times out or is aborted. Fire-and-forget: `notify{message,notifyType}` (an `Event` item),
`setStatus`, `setWidget`, `setTitle`, `set_editor_text` (ignored). Captured: `select` through a
tiny `-e` extension tool (`question`); `confirm`/`input`/`editor` are synthetic tests built from
pi's `docs/rpc-extension-ui.md`.

### Mapping decisions
- Ids: tool calls use pi's `toolCallId`; text/thinking `o<offset of message_start>:<contentIndex>`;
  user messages `o<offset of message_start>`; events `o<offset>`; questions the request id.
- `QuestionResolved` after `answer()` comes with the next stdout line. Like Codex, no
  `ItemCompleted` for the question (the daemon records the answer it sent). A dialog pi closed by
  itself counts as resolved when the tools running at ask time end, at the next assistant message,
  or at `agent_settled`.
- Tool calls of an aborted/errored assistant message never run: `ItemCompleted(ToolCall, Failed)`.
- Pi's own `turn_*`, `agent_end`, `queue_update`, `tool_execution_update` (no streamed tool output)
  produce no events.


## Hooks (PTY mode)

`src/hooks.rs`: config generators + `parse_hook(agent, event, payload) -> Option<HookEvent>`
(`Started{agent_sid, transcript}`, `Working`, `Idle`, `Blocked{text}`, `Stopped`; `None` = no
status change; never panics). Fixtures `fixtures/hooks/<agent>-<event>[-<variant>].json` are the
payloads exactly as femmed receives them; `fixtures/hooks/raw/` holds every call of each capture
run in order, the TUIs' OSC status reports and pi's full event order. Capture:
`scripts/capture-hooks.py {claude|codex|pi}`. Verified with claude 2.1.302, codex 0.163.0, pi 1.3.0.

21 live runs (Claude 8, Codex 7, pi 6 against the mock). Re-running the merged script for pi
reproduced the fixtures (same keys, same 11-call order, same OSC 7501 sequence). The Codex
re-run failed: "Hooks need review" appeared after the prompt was typed, Enter opened the panel
and `t` trusted the user's own hook in `~/.codex/config.toml` (restored from a snapshot). The
panel listed our hooks as installed and trusted with nothing to review, which confirms the
`hooks.state` hashes live. The script now waits for that dialog and dismisses it with Esc. Around
each run it backs up `~/.codex/config.toml` and `hooks.json` to `<file>.femme-capture-bak` and puts a
file back only when the run added nothing but Codex TUI writes and removed nothing (else it leaves
both and says so; a leftover backup stops the next capture). Not re-run.

### What femmed passes, what the agent runs

| agent | femmed passes | agent runs (argv) | payload |
|---|---|---|---|
| Claude | `claude --settings <file-or-json>` = `claude_settings_json(bin)` | `<bin> hook claude <HookEventName>` (exec form, no shell) | JSON on stdin, EOF |
| Codex | `codex --no-daemon ... <codex_hook_args(bin)>` | `$SHELL -c "<sh-quoted bin> hook codex <HookEventName>"`; bash execs it, so argv is `[bin, hook, codex, <Event>]` | JSON on stdin, EOF |
| Codex notify (fallback, not recommended) | `codex_notify_args(bin)` = `-c notify=[bin,"hook","codex","notify"]` | `<bin> hook codex notify <json>` (no shell) | JSON as LAST argv, stdin empty |
| Pi | `pi -e <abs file>`, file = `pi_extension_source(bin)` | `<bin> hook pi <event>` (node `spawn`, no shell, detached) | one JSON line on stdin, EOF |

`femmed hook <agent> <event> [<json>]` contract: payload = 4th argument if present, else stdin to
EOF; empty/broken = `{}`; print NOTHING on stdout (Claude/Codex inject SessionStart and
UserPromptSubmit stdout into the model context; PermissionRequest JSON stdout answers the dialog;
exit 2 blocks the action); always exit 0; return in well under a second (Claude and Codex wait
for hooks; pi SIGKILLs a call after 3 s and serializes them). Session identity: `FEMME_SID` in the
agent's environment reaches every hook process in all three agents (verified); nothing else is
needed. Hook processes have no tty (Codex: new session; pi: detached; Codex notify inherits the
ctty). Claude also sets `CLAUDE_PROJECT_DIR`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`; pi
`PI_CODING_AGENT=true`; Codex adds nothing (no thread id in env, only in the JSON).

### Mapping (`parse_hook`)

| | Started | Working | Blocked | Idle | Stopped |
|---|---|---|---|---|---|
| Claude | SessionStart (main agent) | UserPromptSubmit, PostToolUse, PostToolUseFailure | PermissionRequest (every dialog: tools, AskUserQuestion, ExitPlanMode, also under bypassPermissions); Notification elicitation_dialog / elicitation_url_dialog / permission_prompt | Stop, StopFailure, Notification idle_prompt, PostToolUseFailure `is_interrupt` | SessionEnd except reasons clear/resume |
| Codex | SessionStart | UserPromptSubmit, PostToolUse | PreToolUse of `request_user_input*`, PermissionRequest | Stop, Interrupt, notify `agent-turn-complete` (not title turns) | SessionEnd |
| Pi | session_start | agent_start, turn_start, ui_prompt_end (`idle:false`) | ui_prompt_start (title, else kind) | agent_settled, ui_prompt_end (`idle:true`) | session_shutdown reason quit/null |

Blocked texts: `A or B? (A, B)` (questions + options), `Approve the plan: <first line>`,
`Allow <Tool>: <command|file_path|url|path|pattern|description>`, clipped to 300 bytes.
`Started.agent_sid` is what each agent resumes by (`claude --resume`, `codex resume`,
`pi --session <id|file>`); `transcript` = Claude/Codex `transcript_path`, pi `session_file`.

### Claude Code 2.1.302

- Settings hooks are MERGED with user, project and plugin hooks (herdr, plugins all still ran).
  Generated: `SessionStart UserPromptSubmit PermissionRequest PostToolUse PostToolUseFailure Stop
  StopFailure SessionEnd` + `Notification` with matcher `idle_prompt|elicitation_dialog|elicitation_url_dialog`,
  `timeout: 5`, exec form (`command` + `args`): no quoting issue for any path.
- 33 events exist (PreToolUse, PostToolBatch, UserPromptExpansion, Subagent*, *Compact,
  *ModelSwitch, PermissionDenied, Setup, TeammateIdle, Task*, Elicitation*, ConfigChange, Worktree*,
  InstructionsLoaded, CwdChanged, FileChanged, DirectoryAdded, MessageDisplay, ...); PreToolUse is
  not needed (PermissionRequest follows it 40 ms later for every dialog).
- Orders: prompt -> UserPromptSubmit, PreToolUse, PostToolUse[Failure], PostToolBatch, Stop.
  Dialog -> PreToolUse, PermissionRequest (dialog on screen), Notification permission_prompt 6.0 s
  later only if still open ("Claude needs your permission"; left out of the config: it could race
  the PostToolUse of an answer given at 6 s), PostToolUse after the answer (AskUserQuestion:
  `tool_response.answers`). Stop -> Notification idle_prompt exactly 60 s later
  (`messageIdleNotifThresholdMs` in `~/.claude.json`, no flag/env). `/clear` -> SessionEnd(clear),
  SessionStart(clear) with a NEW session_id; `/exit` -> SessionEnd(prompt_input_exit); `-p` exit
  -> SessionEnd(other). Bad model -> StopFailure(`error: model_not_found`).
- **Esc interrupt fires no hook** (no Stop, no PostToolUseFailure, not even one with
  `is_interrupt`): Working until idle_prompt (60 s). `parse_hook` still maps a `PostToolUseFailure`
  whose payload has `is_interrupt: true` to Idle, but no capture has ever had it: the one real
  `PostToolUseFailure` (a failing Bash call, `fixtures/hooks/claude-PostToolUseFailure-Bash.json`)
  says `is_interrupt: false` and the turn goes on, so it is Working. The `true` arm is covered by a
  doctored payload only (`claude_post_tool_use_failure_is_working_unless_the_payload_says_interrupt`). Real-time fallback: the TUI title (OSC 0) is `✳ <title>` idle, a spinner glyph
  (`◐`/`◑`...) working.
- **Workspace trust dialog** (untrusted cwd, default "No, exit"): no hook at all (not even
  SessionStart) until accepted. femmed pre-trusts (`projects[<cwd>].hasTrustDialogAccepted` in
  `~/.claude.json`) or leaves it to the user.
- Hooks are synchronous; SessionEnd gets max(1.5 s, largest SessionEnd timeout ≤ 60 s).
- `--include-hook-events` (stream-json) and the `http` hook type are alternatives; a
  PermissionRequest hook can answer the dialog (`hookSpecificOutput`): a way to answer PTY-mode
  questions from the desktop later.

### Codex 0.163.0

- `codex_hook_args`: per event `-c hooks.<Event>=[{hooks=[{type="command",command="<line>",timeout=N}]}]`
  (session-flags layer, nothing written to `~/.codex`) for `SessionStart UserPromptSubmit PreToolUse
  PermissionRequest PostToolUse Stop Interrupt SessionEnd` (timeout 5; Interrupt/SessionEnd 3, the
  maximum Codex allows), then ONE `-c hooks.state={"/<session-flags>/config.toml:<snake>:0:0"={trusted_hash="sha256:.."},..}`:
  our hooks are trusted for this run only, without `--dangerously-bypass-hook-trust` (exists, but
  would also run the user's untrusted hooks). Hash = sha256 of the key-sorted compact JSON
  `{"event_name":"<snake>","hooks":[{"async":false,"command":"<line>","timeout":N,"type":"command"}]}`,
  checked against `hooks/list` and live runs. `-c` splits keys at every `.` (quotes included), and a
  second `hooks.state` replaces the first: inline tables only. **It merges with the user's own
  `[hooks.state]`** (verified on 0.163.0 through `codex app-server`'s `hooks/list`, offline): a user
  hook switched off (`enabled = false`) and trusted in `~/.codex/config.toml` stays off and trusted
  next to our eight trusted hooks, because Codex merges config layers table by table and our keys
  all start with `/<session-flags>/config.toml:`, which no key of the user's file can equal. Test:
  `cargo test -p femme-agents --test hooks -- --ignored codex_hook_state` (needs the binary).
- Launch with `--no-daemon` (hooks are children of the TUI and inherit FEMME_SID; under the shared
  daemon they would get its env: inferred). Folder trust without a dialog, this run only:
  `-c projects={"<cwd>"={trust_level="trusted"}}` (femmed's call; not generated).
- Payloads: common `session_id`, `transcript_path` (`~/.codex/sessions/YYYY/MM/DD/rollout-...jsonl`),
  `cwd`, `hook_event_name`, `model`, `permission_mode` (+ `turn_id` inside a turn). SessionStart
  `source` (startup, clear; docs: resume, compact, fork); UserPromptSubmit `prompt`; Pre/PostToolUse
  `tool_name` (shell = `Bash`), `tool_input`, `tool_use_id`, PostToolUse `tool_response`;
  PermissionRequest `tool_input{command, description}`; Stop `last_assistant_message`,
  `stop_hook_active`; SessionEnd `reason` (always `other`). notify: `type: agent-turn-complete`,
  `thread-id`, `turn-id`, `client` (`codex-tui`), `input-messages`, `last-assistant-message`.
- Orders: **SessionStart only fires with the first prompt** (nothing at launch). Prompt ->
  SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop. Approval -> PreToolUse,
  PermissionRequest (overlay), PostToolUse after approval, Stop. Esc -> Interrupt (no Stop; the
  command goes on as a background terminal). `/clear` emits nothing; the next prompt brings
  SessionStart(clear) with a NEW session_id. `/quit` -> SessionEnd for EVERY session of the
  process (also the one /clear left). SIGTERM/SIGHUP -> no hook at all.
- **Async question:** in the TUI the model calls `request_user_input_async`: PreToolUse (Blocked
  with the question), PostToolUse 55 ms later (`{"accepted":true}`, ignored), Stop 1.9 s after the
  PreToolUse (`raw/codex-main.calls.jsonl`: 18.923 s, 18.978 s, 20.823 s) with
  `last_assistant_message: null` -> Idle while the question is still on screen; the answer arrives
  as the next UserPromptSubmit (`prompt: "red"`). `parse_hook` keeps reporting Idle for that Stop
  (`codex-Stop-question.json`; stateless, and a finished turn's `Stop` looks the same apart from its
  text, `codex-Stop.json`): the payload cannot tell. The hints are the Stop's `turn_id` equal to the
  `turn_id` of the `PreToolUse request_user_input*` and the null text; a daemon that wants the board
  to keep saying "needs you" holds Blocked when a Stop follows such a PreToolUse of the same turn
  within a few seconds (1.9 s here) and lets the next UserPromptSubmit clear it
  (`codex_stop_cannot_tell_that_a_question_is_still_open`).
- notify also fires for Codex's internal title-generation turn, mid first turn (even while an
  approval is open), with a foreign `thread-id`: `parse_hook` ignores it, but don't register notify.
- A user's own untrusted hook (here herdr's `~/.codex/hooks.json`) raises "Hooks need review" at
  every TUI start (Esc skips, writes nothing); the user's decision. Every TUI start writes
  `[tui] screen_reader_detection_done = true` once. The bwrap sandbox fails on this host, so only
  escalated commands reach PermissionRequest; FEMME's `--dangerously-bypass-approvals-and-sandbox`
  never asks. OSC 9 (`-c tui.notifications=true -c tui.notification_method="osc9"
  -c tui.notification_condition="always"`) gives `Approval requested: ...`, `Question: ...` and
  the final message as a PTY-side fallback.
- Untested: approval denied, answering through the question panel, Subagent*/Pre/PostCompact,
  resume/fork. (Our `hooks.state` merging with a user's persisted `[hooks.state]` is verified: see
  `codex_hook_args` above.)

### Pi 1.3.0

- No hook flags: the extension is the mechanism. `pi -e <abs path>` loads it for that run (TS via
  jiti; user extensions still load; don't pass `-ne`). Keep the file in place while pi runs
  (`/reload` re-reads it); write it atomically per launch. Gated on `FEMME_SID` (no-op without).
- Forwarded events: session_start{reason startup|reload|new|resume|fork}, agent_start,
  turn_start{turn_index}, agent_settled{aborted, stop_reason, error}, ui_prompt_start/end{kind,
  title}, session_shutdown{reason quit|reload|new|resume|fork}; every line also has `event`, `seq`
  (1..N per pi process, survives /reload), `ts`, `pid` (= hook's ppid), `mode`, `session_id`,
  `session_file`, `idle`. agent_end is NOT final (retry/compaction may follow): agent_settled is.
- Orders: print -> session_start, agent_start, turn_start, agent_settled, session_shutdown(quit).
  TUI dialog -> agent_start, turn_start, ui_prompt_start, ui_prompt_end, turn_start{1},
  agent_settled. `--session <file>` (takeover) -> session_start(startup) with the same id/file.
  /reload, /new, /resume, /fork -> shutdown{reason} then start{reason}, a new session_id except
  for /reload (from pi's source, not run live).
- ui_prompt_* only for extension `ctx.ui` dialogs, TUI/RPC only. Pi has no permission prompts.
  The core trust ("Trust project folder?", before session_start), session-cwd and /login dialogs
  fire nothing; OSC 7501 reports them (below).
- SIGTERM to the TUI: session_shutdown(quit) first, exit 0 (not 143); agent_settled may still
  come after it (the extension drops events after shutdown). SIGKILL/crash: nothing: femmed takes
  Stopped from the PTY exit. Order by (pid, seq); FEMME_SID leaks to pi's children (a nested pi
  does not report; femmed may check `pid`).
- pi had no credentials here: captures use a scripted OpenAI-compatible endpoint
  (`PI_CODING_AGENT_DIR` with a models.json); pi itself is the real binary.

### OSC 7501 (Claude and pi)

Both TUIs speak a program-status protocol: Claude probes `ESC]7501;?ESC\` and reports only if the
terminal answers `ESC]7501;?...`; pi reports when `PI_PROGRAM_STATUS=1` (or on the same probe).
Reports: `ESC]7501;state=idle|working|blocked|done|error|clear:app=<claude-code|pi>[:kind=question|permission|auth][:msg=<base64>]ESC\`
(pi TUI: idle, working, done, working, blocked(question, "Pick a color"), working, done, clear).
It covers what hooks miss (pi's trust/login dialogs) and could feed femme-term directly.

### Brief vs reality

- No Codex lifecycle hook flag/file; hooks go through `-c` overrides plus `hooks.state` trust.
  `-c notify` works but is not recommended. `codex_hook_args` is new; `codex_notify_args` stays.
- Pi has no `--hook(s)`, `--extensions`, `--settings`, `--cwd`, `--tui` and no env var that adds
  extensions; `-e` it is. `pi -p 'say ok'` needed a model: none configured on this host (mock used).
- Claude `--permission-mode default` is spelled `manual` in 2.1.302 (payloads still say `default`).
- `HookEvent::Started` gained `transcript`; consts `HOOK_TIMEOUT_SECS`, `CLAUDE_HOOKS`, `CODEX_HOOKS`.

### Open points for femmed

- CLI shape (implemented by the configs): `femmed hook claude <HookEventName>` (stdin),
  `femmed hook codex <HookEventName>` (stdin), `femmed hook codex notify <json>` (argv),
  `femmed hook pi <event>` (stdin). Pass `<event>` and the payload to `parse_hook` unchanged.
- Stopped also from the PTY child's exit (Codex signals, pi SIGKILL, Claude crash fire nothing).
- Claude Esc-interrupt gap (title/OSC 7501 fallback?), Codex async-question Blocked across Stop
  (state), pre-trust policy for Claude (`~/.claude.json`) and Codex (`projects=` arg).

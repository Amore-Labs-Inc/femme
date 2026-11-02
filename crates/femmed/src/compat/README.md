# compat: the phone board and the Instinct/WhatsApp API on axum

Route for route what `board/src/main.rs` served, so the phone page, the OpenAPI at
`/api/openapi.json` and `examples/api-client.sh` keep working unchanged. Entry point:
`femmed::compat::router(core: Arc<dyn Core>) -> axum::Router`; `femmed::serve(core, cfg)` binds
`Config::http_bind` and runs it with `into_make_service_with_connect_info::<SocketAddr>()` (the
peer address is what makes `/internal/hook` loopback-only and feeds the sign-in throttle).
Everything the board did through herdr is a `Core` call now; git, gh, claude, cswap and aws stay
subprocesses (run in `spawn_blocking`), found through `Config::tool_path` when it is set (tests).

One fallback handler (`mod.rs::dispatch` -> `route`) mirrors the board's `route()`, in this order:

1. `POST`/`PATCH` without `Content-Type: application/json` -> 415 (the CSRF rule: no CORS preflight is ever answered).
2. Public routes and the loopback door.
3. A bearer that is an API key may use only what `util::api_route` lists, else 403 with the board's exact text.
4. Anything else needs the owner: a live `femme_session` cookie, or the `<state>/worker.token` bearer (kept one release).
   Not signed in -> 401 `{"error":"sign in first"}` under `/api/`, a redirect to `/login` elsewhere.
5. The owner's routes; no match -> 404.

Auth classes: **public** (nothing), **loopback** (peer is 127.0.0.1/::1 and no `X-Forwarded-For`,
`Forwarded` or `X-Real-IP`: Caddy always adds one), **key** (an API key or the owner),
**owner** (cookie or worker token only). API keys and the worker token are compared in constant time.
Bodies: JSON at most `MAX_BODY` (1 MiB, else 413); uploads at most `MAX_UPLOAD` (20 MB).
Errors are `{"error": "..."}`; a `CoreError` maps NotFound 404, Busy/Conflict 409, Bad 400, else 502.

| Method | Path | Auth | Served by |
|---|---|---|---|
| GET | `/login`, `/magic/{token}` | public | `mod.rs::route` (LOGIN page) |
| GET | `/logo.svg` | public | `mod.rs::route` (LOGO) |
| POST | `/api/login` | public | `mod.rs::request_link` |
| POST | `/api/magic` | public | `mod.rs::use_link` |
| POST | `/internal/hook` | loopback | `mod.rs::hook` -> `Core::hook` |
| GET | `/api/ping` | key | `mod.rs::route` |
| GET | `/api/openapi.json` | key | `mod.rs::route` (OPENAPI, `{BOARD_URL}` filled in) |
| GET, POST, DELETE | `/api/agents/{name}` | key | `agents::get`, `agents::post`, `agents::delete` |
| POST | `/api/agents/{name}/push`, `/discard` | key | `agents::verdict` |
| GET | `/api/terminals` | key | `terminals::list` |
| GET | `/api/terminals/{pane}` (text screen + status) | key | `terminals::screen(.., false)` |
| POST | `/api/terminals/{pane}/prompt` | key | `terminals::prompt` |
| POST | `/api/terminals/{pane}/input` | key | `terminals::input` |
| GET | `/` | owner | `mod.rs::route` (PAGE) |
| POST | `/api/logout` | owner | `mod.rs::logout` |
| GET, POST | `/api/issues` | owner | `issues::list`, `issues::create` |
| PATCH, DELETE | `/api/issues/{id}` | owner | `issues::patch`, `issues::delete` |
| GET | `/api/meta` | owner | `mod.rs::meta` (+ `deploy.json`) |
| DELETE | `/api/deploy` | owner | `mod.rs::route` (removes `deploy.json`) |
| POST | `/api/terminals` | owner | `terminals::create` |
| DELETE | `/api/terminals/{ws}` | owner | `terminals::close` |
| GET | `/api/terminals/{pane}/screen` (ANSI) | owner | `terminals::screen(.., true)` |
| POST | `/api/terminals/upload` | owner | `terminals::upload` |
| GET, POST | `/api/keys` | owner | `keys::list`, `keys::create` |
| DELETE | `/api/keys/{name}` | owner | `keys::delete` |
| POST, DELETE | `/api/keys/{name}/github` | owner | `keys::github_start`, `keys::github_cancel` |
| GET | `/api/accounts` | owner | `accounts::list` |
| POST | `/api/accounts/switch` | owner | `accounts::switch` |
| POST | `/api/accounts/claude/login`, `/claude/code` | owner | `accounts::login`, `accounts::code` |
| DELETE | `/api/accounts/{kind}/login` | owner | `accounts::cancel` |

Pane ids are the board's: `w<handle>:p<no>`, with an `instinct~` prefix for sessions opened
through `/api/agents` (`CreateSession.name` set, machine tag `instinct`). Timestamps in the JSON are
unix seconds as before (the `Core` speaks milliseconds).

Agent sessions of `/api/agents` and of issues are headless (a driver in `femmed`, no terminal; the
board's own terminals stay TUIs). The same routes serve both: `screen` of a headless session is its
last 50 transcript items as text (a pending question with numbered options; `ansi` changes nothing),
`prompt` queues the text, `input` answers a pending question (`1`.., `y`, `n`, Esc) and is `Busy`
otherwise, and a second answer or a prompt while a question waits is a 409.

## Files

- `mod.rs`: router, `App` state, `Req`, auth order, magic-link sign-in, hook door, meta, response helpers.
- `agents.rs`: `/api/agents`: clone, brief, report, the git outcome, push/discard.
- `terminals.rs`: `/api/terminals`: list, open, close, screen, prompt, input, uploads (7-day prune).
- `issues.rs`, `keys.rs`, `accounts.rs`: their routes; `login.rs` is the `claude`/`gh` device sign-in child.
- `util.rs`: validators, `api_route`, `apply` (issue PATCH fields), small helpers. `tests.rs`: all tests.

## Tests

`cargo test -p femmed --lib compat`: the board's unit tests (moved), then every route through
`tower::ServiceExt::oneshot` against `core_fake::FakeCore`, with fake `git`/`gh`/`claude`/`cswap`/`aws`
scripts on `Config::tool_path`, and `examples/api-client.sh` run for real against `femmed::serve_on`
(skipped when `curl` or `jq` is missing).

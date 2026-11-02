//! PTY-mode hook glue: generated configs and `parse_hook` on the payloads the real CLIs sent
//! (`fixtures/hooks/<agent>-<event>[-<variant>].json`, captured by `scripts/capture-hooks.py`).

use std::path::{Path, PathBuf};

use femme_agents::hooks::{
    claude_settings_json, codex_hook_args, codex_notify_args, parse_hook, pi_extension_source,
    HookEvent, CLAUDE_HOOKS, CODEX_HOOKS, HOOK_TIMEOUT_SECS,
};
use femme_proto::state::Agent;
use proptest::prelude::*;
use serde_json::{json, Value};

/// A femmed path with a space, a single quote and a double quote: every generator must keep it intact.
const NASTY: &str = r#"/opt/my tools/it's "femme"/femmed"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("hooks")
}

fn agent(name: &str) -> Agent {
    match name {
        "claude" => Agent::Claude,
        "codex" => Agent::Codex,
        "pi" => Agent::Pi,
        _ => Agent::Shell,
    }
}

// ---------------------------------------------------------------- generated configs

#[test]
fn claude_settings_hook_every_event_in_exec_form() {
    let v: Value = serde_json::from_str(&claude_settings_json(NASTY)).expect("settings are JSON");
    let hooks = v["hooks"].as_object().expect("hooks object");
    assert_eq!(hooks.len(), CLAUDE_HOOKS.len());
    for (event, matcher) in CLAUDE_HOOKS {
        let entry = &hooks[*event][0];
        assert_eq!(entry["matcher"].as_str(), *matcher, "{event}");
        let h = &entry["hooks"][0];
        assert_eq!(h["type"], "command");
        assert_eq!(
            h["command"], NASTY,
            "path survives untouched (exec form, no shell)"
        );
        assert_eq!(h["args"], json!(["hook", "claude", event]));
        assert_eq!(h["timeout"], json!(HOOK_TIMEOUT_SECS));
    }
    for must in [
        "SessionStart",
        "UserPromptSubmit",
        "PermissionRequest",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(must), "{must}");
    }
}

/// The words a POSIX shell makes of `line` (Codex runs hook commands with `$SHELL -c`).
fn sh_words(line: &str) -> Vec<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("set -- {line}; printf '%s\\0' \"$@\""))
        .output()
        .expect("sh");
    assert!(out.status.success(), "{line}");
    String::from_utf8(out.stdout)
        .unwrap()
        .split_terminator('\0')
        .map(str::to_owned)
        .collect()
}

/// The first TOML basic string after `key=` in `s` (ours are valid JSON strings too).
fn toml_string_after(s: &str, key: &str) -> String {
    let rest = &s[s.find(key).unwrap_or_else(|| panic!("{key} in {s}")) + key.len()..];
    serde_json::Deserializer::from_str(rest)
        .into_iter::<String>()
        .next()
        .expect("a string")
        .expect("TOML basic string")
}

#[test]
fn codex_hook_args_register_every_event_trusted() {
    let args = codex_hook_args(NASTY);
    assert_eq!(args.len(), 2 * (CODEX_HOOKS.len() + 1));
    assert!(args.iter().step_by(2).all(|a| a == "-c"));
    let values: Vec<&String> = args.iter().skip(1).step_by(2).collect();
    let state = values.last().unwrap();
    assert!(state.starts_with("hooks.state={") && state.ends_with('}'));
    for (event, label) in CODEX_HOOKS {
        let prefix = format!("hooks.{event}=[{{hooks=[{{type=\"command\",command=");
        let v = values
            .iter()
            .find(|v| v.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{event}"));
        let command = toml_string_after(v, "command=");
        assert_eq!(
            sh_words(&command),
            [NASTY, "hook", "codex", *event],
            "{command}"
        );
        let timeout = if matches!(*event, "SessionEnd" | "Interrupt") {
            3
        } else {
            HOOK_TIMEOUT_SECS
        };
        assert!(v.ends_with(&format!(",timeout={timeout}}}]}}]")), "{v}");
        let key = format!("\"/<session-flags>/config.toml:{label}:0:0\"={{trusted_hash=");
        let hash = toml_string_after(state, &key);
        assert!(hash.len() == 71 && hash.starts_with("sha256:"), "{hash}");
    }
}

#[test]
fn codex_notify_args_quote_the_path_as_toml() {
    let args = codex_notify_args(NASTY);
    assert_eq!(args[0], "-c");
    let value = args[1].strip_prefix("notify=").expect("notify override");
    // The TOML basic strings we emit are also valid JSON strings.
    let arr: Vec<String> = serde_json::from_str(value).expect("TOML array of basic strings");
    assert_eq!(arr, [NASTY, "hook", "codex", "notify"]);
}

#[test]
fn pi_extension_embeds_the_path_and_the_gate() {
    let src = pi_extension_source(NASTY);
    assert!(
        src.contains("if (!process.env.FEMME_SID) return;"),
        "gated on FEMME_SID"
    );
    let line = src
        .lines()
        .find(|l| l.starts_with("const FEMMED_BIN = "))
        .expect("FEMMED_BIN constant");
    let lit = line
        .trim_start_matches("const FEMMED_BIN = ")
        .trim_end_matches(';');
    assert_eq!(
        serde_json::from_str::<String>(lit).expect("JS string literal"),
        NASTY
    );
    assert!(!src.contains("__FEMMED_BIN__"));
    assert!(src.contains(r#"spawn(FEMMED_BIN, ["hook", "pi", event]"#));
}

/// `FEMME_HOOKS_DUMP=<dir> [FEMME_HOOKS_FEMMED=<path>] cargo test -p femme-agents --test hooks --
/// --ignored --exact dump_hook_configs` writes the generated configs for `scripts/capture-hooks.py`.
#[test]
#[ignore = "writes files; used by scripts/capture-hooks.py"]
fn dump_hook_configs() {
    let dir = PathBuf::from(std::env::var("FEMME_HOOKS_DUMP").expect("FEMME_HOOKS_DUMP"));
    let femmed = std::env::var("FEMME_HOOKS_FEMMED").unwrap_or_else(|_| "femmed".into());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("claude-settings.json"),
        claude_settings_json(&femmed),
    )
    .unwrap();
    // Hooks and notify both, so a capture sees both kinds of calls.
    let codex: Vec<String> = codex_hook_args(&femmed)
        .into_iter()
        .chain(codex_notify_args(&femmed))
        .collect();
    std::fs::write(
        dir.join("codex-args.json"),
        serde_json::to_string(&codex).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.join("femme-hooks.ts"), pi_extension_source(&femmed)).unwrap();
}

// ---------------------------------------------------------------- captured payloads

fn started(sid: &str, transcript: Option<&str>) -> Option<HookEvent> {
    Some(HookEvent::Started {
        agent_sid: Some(sid.into()),
        transcript: transcript.map(Into::into),
    })
}

fn blocked(text: &str) -> Option<HookEvent> {
    Some(HookEvent::Blocked { text: text.into() })
}

/// Every fixture and what it must parse to. `None` = the event does not change the status.
fn table() -> Vec<(&'static str, Option<HookEvent>)> {
    use HookEvent::*;
    let tr = |id: &str| format!("/home/user/.claude/projects/-work-repo/{id}.jsonl");
    let s1 = "a5c64e6b-54cc-4668-a873-f927b2905a93";
    let s2 = "d7728a12-25e6-49a3-901e-88b4013553de";
    let pf = |id: &str, at: &str| {
        format!("/home/user/.pi/agent/sessions/--work-repo--/2026-11-02T{at}_{id}.jsonl")
    };
    let cr = |id: &str, at: &str| {
        format!("/home/user/.codex/sessions/2026/11/02/rollout-2026-11-02T{at}-{id}.jsonl")
    };
    let c1 = "01a19bba-7dc7-7370-b191-fd47dd00557e";
    let c2 = "01a19bba-9fdd-7221-9052-3d6da597d480";
    let p1 = "01a19bb9-5d75-7405-aaec-627eb5594c52";
    let p2 = "01a19bb9-6b9f-7247-8d7c-69142a082644";
    vec![
        // Claude 2.1.302
        (
            "claude-SessionStart-startup.json",
            started(s1, Some(&tr(s1))),
        ),
        (
            "claude-SessionStart-resume.json",
            started(s1, Some(&tr(s1))),
        ),
        ("claude-SessionStart-clear.json", started(s2, Some(&tr(s2)))),
        ("claude-UserPromptSubmit.json", Some(Working)),
        ("claude-PreToolUse-AskUserQuestion.json", None),
        (
            "claude-PermissionRequest-AskUserQuestion.json",
            blocked("A or B? (A, B)"),
        ),
        (
            "claude-PermissionRequest-ExitPlanMode.json",
            blocked("Approve the plan: Reply with the single word ok."),
        ),
        (
            "claude-PermissionRequest-Write.json",
            blocked("Allow Write: /work/repo/perm-test.txt"),
        ),
        (
            "claude-Notification-permission_prompt.json",
            blocked("Claude needs your permission"),
        ),
        ("claude-Notification-idle_prompt.json", Some(Idle)),
        ("claude-PostToolUse-AskUserQuestion.json", Some(Working)),
        ("claude-PostToolUse-ExitPlanMode.json", Some(Working)),
        ("claude-PostToolUse-Write.json", Some(Working)),
        ("claude-PostToolUse-Bash.json", Some(Working)),
        ("claude-PostToolUseFailure-Bash.json", Some(Working)),
        ("claude-Stop.json", Some(Idle)),
        ("claude-StopFailure-model_not_found.json", Some(Idle)),
        ("claude-SessionEnd-other.json", Some(Stopped)),
        ("claude-SessionEnd-prompt_input_exit.json", Some(Stopped)),
        ("claude-SessionEnd-clear.json", None),
        ("claude-InstructionsLoaded.json", None),
        ("claude-PostToolBatch.json", None),
        ("claude-MessageDisplay.json", None),
        // Codex 0.163.0 TUI (`main`: --dangerously-bypass-approvals-and-sandbox; PermissionRequest
        // from `-a on-request -s read-only`)
        (
            "codex-SessionStart-startup.json",
            started(c1, Some(&cr(c1, "10-07-32"))),
        ),
        (
            "codex-SessionStart-clear.json",
            started(c2, Some(&cr(c2, "10-07-41"))),
        ),
        ("codex-UserPromptSubmit.json", Some(Working)),
        ("codex-PreToolUse-Bash.json", None),
        (
            "codex-PreToolUse-request_user_input_async.json",
            blocked("Pick a color (red, blue)"),
        ),
        (
            "codex-PermissionRequest-Bash.json",
            blocked("Allow Bash: touch femme-approval-test.txt"),
        ),
        ("codex-PostToolUse-Bash.json", Some(Working)),
        ("codex-PostToolUse-request_user_input_async.json", None),
        ("codex-Stop.json", Some(Idle)),
        ("codex-Stop-question.json", Some(Idle)),
        ("codex-Interrupt.json", Some(Idle)),
        ("codex-SessionEnd-other.json", Some(Stopped)),
        ("codex-notify.json", Some(Idle)),
        ("codex-notify-question.json", Some(Idle)),
        ("codex-notify-title.json", None),
        // pi 1.3.0 (extension from pi_extension_source; print, TUI, takeover with --session)
        (
            "pi-session_start-print.json",
            started(p1, Some(&pf(p1, "10-06-18-998Z"))),
        ),
        (
            "pi-session_start-tui.json",
            started(p2, Some(&pf(p2, "10-06-22-623Z"))),
        ),
        (
            "pi-session_start-takeover.json",
            started(p2, Some(&pf(p2, "10-06-22-623Z"))),
        ),
        ("pi-agent_start-print.json", Some(Working)),
        ("pi-agent_start-tui.json", Some(Working)),
        ("pi-agent_start-takeover.json", Some(Working)),
        ("pi-turn_start-print.json", Some(Working)),
        ("pi-turn_start-tui.json", Some(Working)),
        ("pi-turn_start-takeover.json", Some(Working)),
        ("pi-ui_prompt_start-tui.json", blocked("Pick a color")),
        ("pi-ui_prompt_end-tui.json", Some(Working)),
        ("pi-agent_settled-print.json", Some(Idle)),
        ("pi-agent_settled-tui.json", Some(Idle)),
        ("pi-session_shutdown-print.json", Some(Stopped)),
        ("pi-session_shutdown-tui.json", Some(Stopped)),
        ("pi-session_shutdown-takeover.json", Some(Stopped)),
    ]
}

/// `<agent>-<event>[-<variant>].json` -> (agent, event).
fn split_name(name: &str) -> (&str, &str) {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    let mut it = stem.splitn(3, '-');
    (it.next().unwrap_or_default(), it.next().unwrap_or_default())
}

#[test]
fn captured_payloads_parse() {
    let mut bad = Vec::new();
    for (name, want) in table() {
        let payload = std::fs::read_to_string(fixtures().join(name))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let (a, event) = split_name(name);
        let got = parse_hook(agent(a), event, &payload);
        if got != want {
            bad.push(format!("{name}: got {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn every_fixture_is_in_the_table() {
    let listed: Vec<&str> = table().iter().map(|(n, _)| *n).collect();
    let mut missing = Vec::new();
    for e in std::fs::read_dir(fixtures()).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".json") && !listed.contains(&name.as_str()) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "fixtures without an expected result: {missing:?}"
    );
}

#[test]
fn garbage_is_none() {
    for agent in [Agent::Claude, Agent::Codex, Agent::Pi, Agent::Shell] {
        for payload in [
            "",
            "null",
            "[]",
            "{",
            "\"x\"",
            "42",
            "{\"hook_event_name\":7}",
            "\u{0}",
        ] {
            for event in ["", "Bogus", "SessionStartX", "notify2"] {
                assert_eq!(
                    parse_hook(agent, event, payload),
                    None,
                    "{agent:?} {event:?} {payload:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------- documented gaps and merges

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The fixture with `key` set to `v`, or removed when `v` is `None`.
fn fixture_with(name: &str, key: &str, v: Option<Value>) -> String {
    let mut p: Value = serde_json::from_str(&fixture_text(name)).unwrap();
    match v {
        Some(v) => p[key] = v,
        None => {
            p.as_object_mut().unwrap().remove(key);
        }
    }
    p.to_string()
}

/// `PostToolUseFailure` is `Working` (a failed tool, the turn goes on), `Idle` only when the
/// payload says `is_interrupt: true`. The one real capture has `false`; no capture has ever had
/// `true`, because an Esc interrupt fires no hook at all (`hooks::CLAUDE_HOOKS` docs), so that arm
/// is exercised by a doctored payload only.
#[test]
fn claude_post_tool_use_failure_is_working_unless_the_payload_says_interrupt() {
    let f = "claude-PostToolUseFailure-Bash.json";
    assert_eq!(fixture_text(f).matches("\"is_interrupt\":false").count(), 1);
    let parse = |p: String| parse_hook(Agent::Claude, "PostToolUseFailure", &p);
    assert_eq!(parse(fixture_text(f)), Some(HookEvent::Working));
    assert_eq!(
        parse(fixture_with(f, "is_interrupt", None)),
        Some(HookEvent::Working)
    );
    assert_eq!(
        parse(fixture_with(f, "is_interrupt", Some(json!("true")))),
        Some(HookEvent::Working),
        "only a JSON true counts"
    );
    assert_eq!(
        parse(fixture_with(f, "is_interrupt", Some(json!(true)))),
        Some(HookEvent::Idle)
    );
}

/// A Codex `Stop` can come while a question is still on screen, and the payload cannot say so:
/// both captured `Stop`s parse to `Idle`. What differs is only the hint `hooks::CODEX_HOOKS`
/// documents: the question's `Stop` carries the `turn_id` of the `PreToolUse request_user_input_async`
/// that announced it and `last_assistant_message: null`; a finished turn has its text.
#[test]
fn codex_stop_cannot_tell_that_a_question_is_still_open() {
    let parse = |f: &str| parse_hook(Agent::Codex, "Stop", &fixture_text(f));
    assert_eq!(parse("codex-Stop.json"), Some(HookEvent::Idle));
    assert_eq!(parse("codex-Stop-question.json"), Some(HookEvent::Idle));
    let json = |f: &str| -> Value { serde_json::from_str(&fixture_text(f)).unwrap() };
    let (done, asking) = (json("codex-Stop.json"), json("codex-Stop-question.json"));
    let pre = json("codex-PreToolUse-request_user_input_async.json");
    assert!(done["last_assistant_message"].is_string());
    assert!(asking["last_assistant_message"].is_null());
    assert_eq!(asking["turn_id"], pre["turn_id"]);
    assert_eq!(asking["session_id"], pre["session_id"]);
    assert_ne!(done["turn_id"], pre["turn_id"]);
    // the announcement itself is the `Blocked` that a daemon can hold through the `Stop`
    assert_eq!(
        parse_hook(
            Agent::Codex,
            "PreToolUse",
            &fixture_text("codex-PreToolUse-request_user_input_async.json")
        ),
        blocked("Pick a color (red, blue)")
    );
}

/// `codex_hook_args` is ONE `-c hooks.state={...}` (a second `hooks.state` override would replace
/// the first) and every key in it sits under the `/<session-flags>/config.toml:` layer, which no
/// key of the user's `[hooks.state]` can equal; that is why the override merges with, and does not
/// replace, the user's trust decisions (`codex_hook_state_merges_with_the_users`).
#[test]
fn codex_hook_state_is_one_override_keyed_apart_from_the_users_files() {
    let args = codex_hook_args(NASTY);
    let state: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with("hooks.state"))
        .collect();
    assert_eq!(state.len(), 1, "{args:?}");
    let state = state[0].strip_prefix("hooks.state=").unwrap();
    assert_eq!(
        state.matches("trusted_hash=").count(),
        CODEX_HOOKS.len(),
        "{state}"
    );
    assert_eq!(
        state.matches("\"/<session-flags>/config.toml:").count(),
        CODEX_HOOKS.len(),
        "every key is in the session-flags layer: {state}"
    );
    assert!(
        !state.contains(".codex/") && !state.contains("hooks.json"),
        "{state}"
    );
}

/// `hooks/list` of a fresh `codex app-server` (no model, no network) with `CODEX_HOME=home`,
/// started in `cwd` with the extra `args`; the `hooks` of its one entry.
fn codex_hooks_list(home: &Path, cwd: &Path, args: &[String]) -> Vec<Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }
    let mut child = Kill(
        Command::new("codex")
            .arg("app-server")
            .args(args)
            .env("CODEX_HOME", home)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the codex binary on PATH"),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let mut ask = |msg: Value| {
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        let Some(id) = msg.get("id").cloned() else {
            return Value::Null;
        };
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let v = rx
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no answer to {msg}: {e}"));
            if v["id"] == id {
                return v;
            }
        }
    };
    let client = json!({"name": "femme", "title": "FEMME", "version": "0.1.0"});
    ask(json!({"id": 1, "method": "initialize", "params": {"clientInfo": client}}));
    ask(json!({"method": "initialized"}));
    let r = ask(json!({"id": 2, "method": "hooks/list", "params": {"cwds": [cwd]}}));
    let entry = &r["result"]["data"][0];
    assert_eq!(entry["errors"], json!([]), "{r}");
    entry["hooks"].as_array().expect("hooks array").clone()
}

/// Offline check of what `hooks::codex_hook_args` claims: our `-c hooks.state=...` does not
/// replace the user's `[hooks.state]`. A user hook switched off and trusted in `CODEX_HOME`'s
/// `config.toml` is still off and trusted next to our hooks, which are all trusted. Needs the
/// `codex` binary; `cargo test -p femme-agents --test hooks -- --ignored codex_hook_state`.
#[test]
#[ignore = "needs the codex binary (runs `codex app-server`, offline, no model)"]
fn codex_hook_state_merges_with_the_users() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codex-hook-state");
    let (home, cwd) = (root.join("home"), root.join("cwd"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let user_hook = "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\n\
                     command = \"echo user-stop\"\ntimeout = 5\n";
    let config = home.join("config.toml");
    let user_of = |hooks: &[Value]| -> Value {
        hooks
            .iter()
            .find(|h| h["command"] == "echo user-stop")
            .unwrap_or_else(|| panic!("the user's hook is listed: {hooks:?}"))
            .clone()
    };

    std::fs::write(&config, user_hook).unwrap();
    let user = user_of(&codex_hooks_list(&home, &cwd, &[]));
    assert_ne!(user["trustStatus"], "trusted", "{user}");
    let (key, hash) = (user["key"].as_str().unwrap(), user["currentHash"].clone());

    // the user trusted that hook and switched it off
    let toml_str = |v: &Value| v.to_string();
    std::fs::write(
        &config,
        format!(
            "{user_hook}\n[hooks.state.{}]\nenabled = false\ntrusted_hash = {}\n",
            toml_str(&json!(key)),
            toml_str(&hash)
        ),
    )
    .unwrap();
    let user = user_of(&codex_hooks_list(&home, &cwd, &[]));
    assert_eq!(
        (&user["enabled"], &user["trustStatus"]),
        (&json!(false), &json!("trusted"))
    );

    // ... and femme's override on top of it
    let hooks = codex_hooks_list(&home, &cwd, &codex_hook_args(NASTY));
    let user = user_of(&hooks);
    assert_eq!(
        (&user["enabled"], &user["trustStatus"]),
        (&json!(false), &json!("trusted")),
        "femme's hooks.state replaced the user's: {user}"
    );
    let ours: Vec<&Value> = hooks
        .iter()
        .filter(|h| {
            h["key"]
                .as_str()
                .is_some_and(|k| k.starts_with("/<session-flags>/config.toml:"))
        })
        .collect();
    assert_eq!(ours.len(), CODEX_HOOKS.len(), "{hooks:?}");
    for h in ours {
        assert_eq!(h["trustStatus"], "trusted", "{h}");
        assert_eq!(h["enabled"], true, "{h}");
    }
}

// ---------------------------------------------------------------- properties

const EVENTS: &[&str] = &[
    "",
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
    "PreToolUse",
    "notify",
    "session_start",
    "agent_start",
    "agent_end",
    "session_shutdown",
    "turn_start",
    "turn_end",
    "agent_settled",
    "ui_prompt_start",
    "ui_prompt_end",
    "Interrupt",
];

const KEYS: &[&str] = &[
    "hook_event_name",
    "session_id",
    "transcript_path",
    "agent_id",
    "notification_type",
    "message",
    "reason",
    "source",
    "tool_name",
    "tool_input",
    "questions",
    "question",
    "options",
    "label",
    "plan",
    "command",
    "is_interrupt",
    "type",
    "event",
    "session_file",
    "thread-id",
    "turn-id",
    "last-assistant-message",
    "last_assistant_message",
    "input-messages",
    "title",
    "kind",
    "idle",
];

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        prop::sample::select(EVENTS).prop_map(|s| json!(s)),
        "\\PC{0,40}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((prop::sample::select(KEYS), inner), 0..8).prop_map(|kv| {
                Value::Object(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
            }),
        ]
    })
}

fn arb_agent() -> impl Strategy<Value = Agent> {
    prop::sample::select(vec![Agent::Claude, Agent::Codex, Agent::Pi, Agent::Shell])
}

proptest! {
    #[test]
    fn random_text_never_panics(a in arb_agent(), event in "\\PC{0,24}", payload in "\\PC{0,300}") {
        let _ = parse_hook(a, &event, &payload);
    }

    #[test]
    fn random_json_never_panics(a in arb_agent(), event in prop::sample::select(EVENTS), v in arb_json()) {
        let _ = parse_hook(a, event, &v.to_string());
    }

    #[test]
    fn unknown_events_are_none(a in arb_agent(), event in "[A-Za-z_]{0,12}", v in arb_json()) {
        let event = format!("{event}Zq9");
        prop_assert_eq!(parse_hook(a, &event, &v.to_string()), None);
    }
}

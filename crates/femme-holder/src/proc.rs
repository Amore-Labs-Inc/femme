//! Child process helpers: the environment a child gets, exit codes, signals.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use femme_proto::Sid;
use rustix::process::{kill_process, kill_process_group, Pid, Signal};

/// Variables a child inherits from the holder's own environment. Everything else (notably
/// `NOTIFY_SOCKET`, `WATCHDOG_USEC`, `INVOCATION_ID`, `JOURNAL_STREAM`) stays out.
const INHERIT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The holder's own environment, minus anything that is not valid UTF-8.
pub fn holder_env() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
}

/// The environment for a pane (`pane = Some(no)`) or the agent (`None`).
///
/// Order of precedence, lowest first: the inherited allowlist, `TERM=xterm-256color`,
/// `COLORTERM=truecolor` and (when no locale is set) `LANG=C.UTF-8`, then the caller's `extra`,
/// then `FEMME_SID` and `FEMME_PANE`, which the caller cannot override.
pub fn child_env(
    inherit_from: impl IntoIterator<Item = (String, String)>,
    extra: &[(String, String)],
    sid: Sid,
    pane: Option<u16>,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = inherit_from
        .into_iter()
        .filter(|(k, _)| INHERIT.contains(&k.as_str()) || k.starts_with("LC_"))
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    if !["LANG", "LC_ALL", "LC_CTYPE"]
        .iter()
        .any(|k| env.contains_key(*k))
    {
        env.insert("LANG".into(), "C.UTF-8".into());
    }
    for (k, v) in extra {
        // `Command` refuses these and the spawn would fail with a bare EINVAL.
        if !k.is_empty() && !k.contains(['=', '\0']) && !v.contains('\0') {
            env.insert(k.clone(), v.clone());
        }
    }
    env.insert("FEMME_SID".into(), sid.to_string());
    match pane {
        Some(no) => env.insert("FEMME_PANE".into(), no.to_string()),
        None => env.remove("FEMME_PANE"),
    };
    env.into_iter().collect()
}

/// Exit code, or `-signal` when the process was killed by a signal.
pub fn exit_code(st: ExitStatus) -> i32 {
    st.code().or_else(|| st.signal().map(|s| -s)).unwrap_or(-1)
}

/// Signal `pid` (and its whole process group when `group`). Errors are reported as text.
pub fn deliver(pid: u32, sig: i32, group: bool) -> Result<(), String> {
    let sig = Signal::from_named_raw(sig).ok_or_else(|| format!("unsupported signal {sig}"))?;
    send(pid, sig, group).map_err(|e| e.to_string())
}

/// `kill(2)` on a pid or its process group; `ESRCH`-style failures are the caller's to ignore.
pub fn send(pid: u32, sig: Signal, group: bool) -> rustix::io::Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(rustix::io::Errno::SRCH)?;
    if group {
        // The child is a session/group leader, so its pid is the group id. Fall back to the pid
        // alone if the group is already gone.
        kill_process_group(pid, sig).or_else(|_| kill_process(pid, sig))
    } else {
        kill_process(pid, sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }

    fn sid() -> Sid {
        "01J00000000000000000000000".parse().unwrap()
    }

    #[test]
    fn inherits_only_the_allowlist() {
        let holder = kv(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("NOTIFY_SOCKET", "/run/systemd/notify"),
            ("WATCHDOG_USEC", "1000"),
            ("LC_TIME", "C"),
            ("SECRET_TOKEN", "x"),
        ]);
        let env = child_env(holder, &[], sid(), Some(2));
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
        assert_eq!(get(&env, "HOME"), Some("/home/u"));
        assert_eq!(get(&env, "LC_TIME"), Some("C"));
        assert_eq!(get(&env, "NOTIFY_SOCKET"), None);
        assert_eq!(get(&env, "WATCHDOG_USEC"), None);
        assert_eq!(get(&env, "SECRET_TOKEN"), None);
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "COLORTERM"), Some("truecolor"));
        assert_eq!(get(&env, "FEMME_SID"), Some("01J00000000000000000000000"));
        assert_eq!(get(&env, "FEMME_PANE"), Some("2"));
    }

    #[test]
    fn caller_env_wins_except_for_femme_ids() {
        let extra = kv(&[
            ("TERM", "screen"),
            ("FOO", "bar"),
            ("FEMME_SID", "evil"),
            ("FEMME_PANE", "9"),
            ("BAD=KEY", "x"),
            ("", "y"),
        ]);
        let env = child_env(kv(&[("PATH", "/bin")]), &extra, sid(), Some(1));
        assert_eq!(get(&env, "TERM"), Some("screen"));
        assert_eq!(get(&env, "FOO"), Some("bar"));
        assert_eq!(get(&env, "FEMME_SID"), Some("01J00000000000000000000000"));
        assert_eq!(get(&env, "FEMME_PANE"), Some("1"));
        assert_eq!(get(&env, "BAD=KEY"), None);
        assert_eq!(get(&env, ""), None);
    }

    #[test]
    fn agent_has_no_pane_and_locale_defaults_to_utf8() {
        let env = child_env(Vec::new(), &kv(&[("FEMME_PANE", "3")]), sid(), None);
        assert_eq!(get(&env, "FEMME_PANE"), None);
        assert_eq!(get(&env, "LANG"), Some("C.UTF-8"));
        let env = child_env(kv(&[("LC_ALL", "en_US.UTF-8")]), &[], sid(), None);
        assert_eq!(get(&env, "LANG"), None);
    }

    #[test]
    fn unknown_signals_are_refused() {
        assert!(deliver(1, 9999, false).is_err());
        assert!(deliver(0, 15, false).is_err());
    }
}

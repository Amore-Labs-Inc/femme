//! Where the daemon is and which device this is: the flags, the `FEMME_*` variables and
//! `~/.config/femme/desktop.env`, in that order, plus the device identity `femmed enroll` records.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use femme_transport::{fp_from_hex, fp_to_hex, DeviceIdentity};

/// What reaches the daemon: QUIC (its address and the SHA-256 of its certificate) and/or the ssh
/// alias that is the fallback link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    /// The daemon's UDP address and certificate fingerprint.
    pub quic: Option<(SocketAddr, [u8; 32])>,
    /// The ssh alias of the box.
    pub ssh: Option<String>,
    /// Where this device's certificate and key live.
    pub identity: PathBuf,
}

/// What the command line said about the daemon (`None`: not given).
#[derive(Clone, Debug, Default)]
pub struct Flags {
    /// `--daemon ADDR:PORT`.
    pub daemon: Option<String>,
    /// `--fingerprint HEX64`.
    pub fingerprint: Option<String>,
    /// `--ssh HOST`.
    pub ssh: Option<String>,
}

/// `~/.config/femme`, if there is a home.
fn config_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".config/femme"))
}

/// `KEY=VALUE` lines of the env file: blank lines and `#` comments skipped, one pair of quotes
/// around a value dropped. ponytail: no `export`, no inline comments, no expansion; it is not a shell.
fn parse_env_file(text: &str) -> HashMap<&str, &str> {
    text.lines()
        .filter_map(|l| l.trim().split_once('='))
        .filter(|(k, _)| !k.trim_start().starts_with('#'))
        .map(|(k, v)| {
            let v = v.trim();
            let unquoted = ['"', '\''].iter().find_map(|q| {
                v.strip_prefix(*q)
                    .and_then(|v| v.strip_suffix(*q))
                    .filter(|_| v.len() >= 2)
            });
            (k.trim(), unquoted.unwrap_or(v))
        })
        .collect()
}

/// The first of flag, environment and file that has a value for `key`.
fn pick(
    flag: Option<String>,
    key: &str,
    env: &impl Fn(&str) -> Option<String>,
    file: &HashMap<&str, &str>,
) -> Option<String> {
    let given = |v: Option<String>| v.filter(|s| !s.is_empty());
    given(flag)
        .or_else(|| given(env(key)))
        .or_else(|| given(file.get(key).map(|v| (*v).to_owned())))
}

/// The address behind `--daemon`: a literal `ADDR:PORT` or a name that resolves to one.
// ponytail: resolved once at start (blocking, std only); a daemon whose address changes needs a restart.
fn address(s: &str) -> anyhow::Result<SocketAddr> {
    s.to_socket_addrs()
        .with_context(|| format!("`{s}` is not ADDR:PORT"))?
        .next()
        .with_context(|| format!("`{s}` resolves to no address"))
}

/// Merges the three sources (flags, then `env`, then the env file's `file` text) into what to
/// connect to; `None` when neither a daemon nor an ssh host is configured (the app then runs on its
/// synthetic panes).
pub fn resolve(
    flags: Flags,
    env: impl Fn(&str) -> Option<String>,
    file: &str,
    identity: PathBuf,
) -> anyhow::Result<Option<Connect>> {
    let file = parse_env_file(file);
    let daemon = pick(flags.daemon, "FEMME_DAEMON", &env, &file);
    let fingerprint = pick(flags.fingerprint, "FEMME_FINGERPRINT", &env, &file);
    let ssh = pick(flags.ssh, "FEMME_SSH", &env, &file);
    let quic = match (daemon, fingerprint) {
        (Some(addr), Some(fp)) => Some((
            address(&addr)?,
            fp_from_hex(&fp).context("the fingerprint is not 64 hex digits")?,
        )),
        (Some(_), None) => bail!("--daemon needs --fingerprint (`femmed enroll` prints it)"),
        // ponytail: a fingerprint without an address is ignored, so one env file can serve --ssh runs too.
        (None, _) => None,
    };
    Ok((quic.is_some() || ssh.is_some()).then_some(Connect {
        quic,
        ssh,
        identity,
    }))
}

/// [`resolve`] over the real environment and `~/.config/femme/desktop.env`; `identity` is
/// `--identity` (default `~/.config/femme/device`).
pub fn load(flags: Flags, identity: Option<PathBuf>) -> anyhow::Result<Option<Connect>> {
    let file = match config_dir().map(|d| d.join("desktop.env")) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        },
        None => String::new(),
    };
    let identity = identity.or_else(|| config_dir().map(|d| d.join("device")));
    // Without a home and without --identity there is nowhere to keep the device: only a run that
    // connects needs one, so the error waits for a connection to be asked for.
    let connect = resolve(
        flags,
        |k| std::env::var(k).ok(),
        &file,
        identity.clone().unwrap_or_default(),
    )?;
    if connect.is_some() && identity.is_none() {
        bail!("HOME is not set: pass --identity DIR");
    }
    Ok(connect)
}

/// This machine's name as the daemon will list the device: `/etc/hostname`, `$HOSTNAME`,
/// `hostname(1)`, else a fixed word; made safe to paste into a shell line.
pub fn hostname() -> String {
    let named = |s: String| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .and_then(named)
        .or_else(|| std::env::var("HOSTNAME").ok().and_then(named))
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            String::from_utf8(out.stdout).ok().and_then(named)
        })
        .unwrap_or_else(|| "femme-desktop".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// This device's identity from `dir`, made on first use (the directory 0700, the key 0600).
pub fn identity(dir: &Path) -> anyhow::Result<DeviceIdentity> {
    let mut dirs = std::fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
    dirs.create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    DeviceIdentity::load_or_generate(dir, &hostname())
        .with_context(|| format!("device identity in {}", dir.display()))
}

/// The line that enrolls this device on the daemon's box (`host` is the ssh alias, if known).
pub fn enroll_line(host: Option<&str>, spki: &[u8; 32]) -> String {
    format!(
        "ssh {} femmed enroll --name {} --spki {}",
        host.unwrap_or("HOST"),
        hostname(),
        fp_to_hex(spki)
    )
}

/// What `--show-spki` prints: the SPKI hash of this device's key and the line that enrolls it.
pub fn show_spki(dir: &Path, host: Option<&str>) -> anyhow::Result<String> {
    let spki = identity(dir)?
        .spki_sha256()
        .context("the device certificate has no public key")?;
    Ok(format!(
        "spki {}\n{}",
        fp_to_hex(&spki),
        enroll_line(host, &spki)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "aa00000000000000000000000000000000000000000000000000000000000001";
    const FP_B: &str = "bb00000000000000000000000000000000000000000000000000000000000002";

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    fn resolve_with(
        flags: Flags,
        env: &[(&str, &str)],
        file: &str,
    ) -> anyhow::Result<Option<Connect>> {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        resolve(flags, |k| env.get(k).cloned(), file, PathBuf::from("/id"))
    }

    #[test]
    fn env_file_parses_pairs_comments_and_quotes() {
        let f = "# comment\n\nFEMME_SSH = vps\nFEMME_DAEMON=\"1.2.3.4:7788\"\n  FEMME_FINGERPRINT='ab'  \n#X=1\nnoequals\nQ=\"\n";
        let m = parse_env_file(f);
        assert_eq!(m.get("FEMME_SSH"), Some(&"vps"));
        assert_eq!(m.get("FEMME_DAEMON"), Some(&"1.2.3.4:7788"));
        assert_eq!(m.get("FEMME_FINGERPRINT"), Some(&"ab"));
        assert_eq!(
            m.get("Q"),
            Some(&"\""),
            "a lone quote is a value, not a pair"
        );
        assert!(!m.contains_key("#X") && !m.contains_key("noequals"));
    }

    #[test]
    fn flags_beat_env_beat_file() {
        let file = format!("FEMME_DAEMON=10.0.0.3:1\nFEMME_FINGERPRINT={FP_B}\nFEMME_SSH=file\n");
        let env = [("FEMME_DAEMON", "10.0.0.2:2"), ("FEMME_SSH", "env")];
        let flags = Flags {
            daemon: some("10.0.0.1:3"),
            fingerprint: some(FP_A),
            ssh: None,
        };
        let got = resolve_with(flags, &env, &file).unwrap().unwrap();
        let (addr, fp) = got.quic.unwrap();
        assert_eq!(
            addr,
            "10.0.0.1:3".parse().unwrap(),
            "the flag wins over env and file"
        );
        assert_eq!(fp, fp_from_hex(FP_A).unwrap());
        assert_eq!(got.ssh.as_deref(), Some("env"), "env wins over the file");

        let got = resolve_with(Flags::default(), &env, &file)
            .unwrap()
            .unwrap();
        assert_eq!(got.quic.unwrap().0, "10.0.0.2:2".parse().unwrap());
        assert_eq!(
            got.quic.unwrap().1,
            fp_from_hex(FP_B).unwrap(),
            "the file fills what is left"
        );

        let got = resolve_with(Flags::default(), &[], &file).unwrap().unwrap();
        assert_eq!(got.quic.unwrap().0, "10.0.0.3:1".parse().unwrap());
        assert_eq!(got.ssh.as_deref(), Some("file"));
    }

    #[test]
    fn nothing_configured_stays_synthetic() {
        assert_eq!(resolve_with(Flags::default(), &[], "").unwrap(), None);
        // A fingerprint alone does not connect to anything.
        let flags = Flags {
            fingerprint: some(FP_A),
            ..Flags::default()
        };
        assert_eq!(resolve_with(flags, &[], "").unwrap(), None);
        // Empty values count as not given.
        let flags = Flags {
            ssh: some(""),
            ..Flags::default()
        };
        assert_eq!(
            resolve(flags, no_env, "FEMME_SSH=\n", PathBuf::new()).unwrap(),
            None
        );
    }

    #[test]
    fn ssh_alone_is_enough_and_a_daemon_needs_its_fingerprint() {
        let flags = Flags {
            ssh: some("vps"),
            ..Flags::default()
        };
        let got = resolve_with(flags, &[], "").unwrap().unwrap();
        assert_eq!((got.quic, got.ssh.as_deref()), (None, Some("vps")));

        let flags = Flags {
            daemon: some("127.0.0.1:7788"),
            ..Flags::default()
        };
        let err = resolve_with(flags, &[], "").unwrap_err().to_string();
        assert!(err.contains("--fingerprint"), "{err}");
        let flags = Flags {
            daemon: some("127.0.0.1:7788"),
            fingerprint: some("zz"),
            ..Flags::default()
        };
        assert!(resolve_with(flags, &[], "").is_err());
        let flags = Flags {
            daemon: some("not an address"),
            fingerprint: some(FP_A),
            ..Flags::default()
        };
        assert!(resolve_with(flags, &[], "").is_err());
    }

    #[test]
    fn fingerprints_may_come_with_colons() {
        let colons: String = FP_A
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        let flags = Flags {
            daemon: some("127.0.0.1:7788"),
            fingerprint: Some(colons),
            ..Flags::default()
        };
        let got = resolve_with(flags, &[], "").unwrap().unwrap();
        assert_eq!(got.quic.unwrap().1, fp_from_hex(FP_A).unwrap());
    }

    #[test]
    fn identity_is_made_once_in_a_private_directory() {
        let dir = std::env::temp_dir().join(format!("femme-desktop-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir = dir.join("a/device");
        let first = identity(&dir).unwrap().spki_sha256().unwrap();
        let again = identity(&dir).unwrap().spki_sha256().unwrap();
        assert_eq!(first, again, "the second run loads what the first made");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("device.key")), 0o600);
        }
        let text = show_spki(&dir, Some("vps")).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some(format!("spki {}", fp_to_hex(&first)).as_str())
        );
        assert_eq!(
            lines.next(),
            Some(
                format!(
                    "ssh vps femmed enroll --name {} --spki {}",
                    hostname(),
                    fp_to_hex(&first)
                )
                .as_str()
            )
        );
        assert!(show_spki(&dir, None)
            .unwrap()
            .contains("ssh HOST femmed enroll"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn hostname_is_one_safe_word() {
        let h = hostname();
        assert!(!h.is_empty() && !h.contains(char::is_whitespace), "{h}");
    }
}

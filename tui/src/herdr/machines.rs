//! Which herdr servers to show: this machine's socket, then every socket a linked machine lends
//! in ~/.config/herdr/remote (a laptop's femme-link, the Instinct session's symlink), as the board
//! lists them (board/src/main.rs machines()).
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub sock: PathBuf,
    pub local: bool,
}

/// The local socket, in herdr's own order: --socket, HERDR_SOCKET_PATH, --session, HERDR_SESSION,
/// the default.
pub fn local_socket(home: &Path, socket: Option<&Path>, session: Option<&str>) -> PathBuf {
    if let Some(s) = socket {
        return s.to_path_buf();
    }
    if let Some(s) = std::env::var_os("HERDR_SOCKET_PATH").filter(|s| !s.is_empty()) {
        return PathBuf::from(s);
    }
    let named = |n: &str| home.join(".config/herdr/sessions").join(n).join("herdr.sock");
    if let Some(n) = session {
        return named(n);
    }
    if let Ok(n) = std::env::var("HERDR_SESSION") {
        if !n.is_empty() {
            return named(&n);
        }
    }
    home.join(".config/herdr/herdr.sock")
}

/// A machine name as the board accepts it in `laptop~w1:p1` ids.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() < 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn discover(home: &Path, local: &Path, local_name: &str) -> Vec<Machine> {
    let mut list = vec![Machine { name: local_name.to_string(), sock: local.to_path_buf(), local: true }];
    let mut remote: Vec<Machine> = std::fs::read_dir(home.join(".config/herdr/remote"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| {
            let file = f.file_name();
            let name = file.to_string_lossy().strip_suffix(".sock")?.to_string();
            (valid_name(&name) && name != local_name).then(|| Machine { name, sock: f.path(), local: false })
        })
        .collect();
    remote.sort_by(|a, b| a.name.cmp(&b.name));
    list.extend(remote);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_like_the_board() {
        assert!(valid_name("laptop"));
        assert!(valid_name("dora-mbp_2"));
        assert!(!valid_name(""));
        assert!(!valid_name("a.b"));
        assert!(!valid_name("../x"));
    }

    #[test]
    fn socket_order() {
        let home = Path::new("/h");
        assert_eq!(local_socket(home, Some(Path::new("/s")), Some("x")), PathBuf::from("/s"));
        assert_eq!(local_socket(home, None, Some("instinct")), PathBuf::from("/h/.config/herdr/sessions/instinct/herdr.sock"));
    }
}

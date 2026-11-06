//! The theme choice kept between runs: the word `dark` or `light` in
//! `$XDG_STATE_HOME/femme/theme` (or `~/.local/state/femme/theme`), beside the board's file. It is
//! read once, before the first window opens, and written on every flip. No file, an unreadable one
//! or any word but `dark` means light: a theme that cannot be kept is not worth a dialog.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The name of the file the choice is kept in, in the same directory as the board's.
const FILE_NAME: &str = "theme";

/// Where the choice is kept, from the two environment variables that decide it (the same rule as
/// the board's file: an absolute `XDG_STATE_HOME`, else `HOME`/.local/state). None when neither is
/// usable.
pub(super) fn path_from(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    crate::board::store::path_from(xdg_state_home, home).map(|p| p.with_file_name(FILE_NAME))
}

/// Where this process keeps the choice (see [`path_from`], with the real environment).
fn path() -> Option<PathBuf> {
    path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// What a file's text says: dark for `dark` (any case, blanks around it), light for anything else.
pub(super) fn parse(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("dark")
}

/// The word that is written for a choice.
pub(super) fn word(dark: bool) -> &'static str {
    if dark {
        "dark"
    } else {
        "light"
    }
}

/// The choice in the file at `path`: light when the file is missing or cannot be read.
pub(super) fn read_from(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| parse(&text))
}

/// Writes the choice to `path`: to a temporary file beside it, then renamed over it, so a crash
/// leaves the old word and never half of a new one.
pub(super) fn write_to(path: &Path, dark: bool) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, word(dark))?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The saved choice of this process: `true` for dark.
pub(super) fn load() -> bool {
    path().is_some_and(|p| read_from(&p))
}

/// Keeps the choice of this process. Best effort: with no home directory, or a disk that refuses,
/// the theme still flips and is light again at the next start.
pub(super) fn save(dark: bool) {
    if let Some(path) = path() {
        let _ = write_to(&path, dark);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "femme-theme-state-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("femme").join(FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_file_is_the_boards_neighbour() {
        let p = |x: Option<&str>, h: Option<&str>| path_from(x.map(Into::into), h.map(Into::into));
        assert_eq!(
            p(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/femme/theme"))
        );
        assert_eq!(
            p(Some("/xdg/state"), Some("/home/u")),
            Some(PathBuf::from("/xdg/state/femme/theme"))
        );
        // A relative XDG_STATE_HOME is invalid and ignored; with no usable home nothing is kept.
        assert_eq!(
            p(Some("relative"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/femme/theme"))
        );
        assert_eq!(p(None, None), None);
    }

    #[test]
    fn only_the_word_dark_is_dark() {
        for text in ["dark", "dark\n", "  Dark \r\n", "DARK"] {
            assert!(parse(text), "{text:?}");
        }
        for text in [
            "",
            "light",
            "light\n",
            "darker",
            "dar",
            "1",
            "true",
            "dark mode",
        ] {
            assert!(!parse(text), "{text:?}");
        }
        assert!(parse(word(true)) && !parse(word(false)));
    }

    #[test]
    fn a_missing_file_is_light() {
        let dir = TempDir::new();
        assert!(!read_from(&dir.file()));
    }

    #[test]
    fn the_choice_survives_a_round_trip_and_creates_the_directory() {
        let dir = TempDir::new();
        let file = dir.file();
        write_to(&file, true).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "dark");
        assert!(read_from(&file));
        write_to(&file, false).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "light");
        assert!(!read_from(&file));
        // No temporary file is left beside it.
        let left: Vec<_> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [OsString::from(FILE_NAME)]);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_light() {
        let dir = TempDir::new();
        // A directory where the file should be: reading it fails.
        fs::create_dir_all(dir.file()).unwrap();
        assert!(!read_from(&dir.file()));
        // And writing over it fails without panicking.
        assert!(write_to(&dir.file(), true).is_err());
    }
}

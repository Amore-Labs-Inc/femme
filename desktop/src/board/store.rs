//! Keeping the board between runs: the scene as JSON in `$XDG_STATE_HOME/femme/board.json` (or
//! `~/.local/state/femme/board.json`), written whole and atomically (a temporary file renamed over
//! the old one, so a crash mid-write leaves the old board). A file that cannot be read is moved
//! aside as `board.json.bad` and never overwritten unread, so a bad save cannot eat a drawing.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::scene::Scene;

/// The name of the file the board is kept in.
pub const FILE_NAME: &str = "board.json";

/// The directory under the state home that holds the file.
const DIR_NAME: &str = "femme";

/// Where the board is kept, from the two environment variables that decide it: an absolute
/// `XDG_STATE_HOME`, else `HOME`/.local/state. None when neither is usable.
pub fn path_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let usable = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let state =
        usable(xdg_state_home).or_else(|| usable(home).map(|h| h.join(".local").join("state")))?;
    Some(state.join(DIR_NAME).join(FILE_NAME))
}

/// Where this process keeps the board (see [`path_from`], with the real environment).
pub fn default_path() -> Option<PathBuf> {
    path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// What [`Store::load`] found.
#[derive(Debug)]
pub struct Loaded {
    /// The saved board, or an empty one when there was none or it could not be read.
    pub scene: Scene,
    /// Why a saved board was not used, for the user: set only when a file was there and was set
    /// aside.
    pub warning: Option<String>,
}

/// The file the board lives in. With no path (no home directory) nothing is saved.
#[derive(Clone, Debug)]
pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    /// A store at `path`.
    pub fn at(path: PathBuf) -> Store {
        Store { path: Some(path) }
    }

    /// The store of this process: [`default_path`].
    pub fn standard() -> Store {
        Store {
            path: default_path(),
        }
    }

    /// The file, when there is one.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Reads the saved board. No file is an empty board; a file that is not a board (or is from a
    /// newer version) is renamed to `board.json.bad` and gives an empty board with a warning.
    pub fn load(&self) -> Loaded {
        let fresh = |warning| Loaded {
            scene: Scene::new(),
            warning,
        };
        let Some(path) = &self.path else {
            return fresh(None);
        };
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return fresh(None),
            Err(e) => return fresh(Some(self.set_aside(path, &e.to_string()))),
        };
        match Scene::from_json(&text) {
            Ok(scene) => Loaded {
                scene,
                warning: None,
            },
            Err(why) => fresh(Some(self.set_aside(path, &why))),
        }
    }

    /// Moves an unreadable file out of the way and says so.
    fn set_aside(&self, path: &Path, why: &str) -> String {
        let bad = path.with_extension("json.bad");
        match fs::rename(path, &bad) {
            Ok(()) => format!(
                "the saved board could not be read ({why}); kept as {}",
                bad.display()
            ),
            Err(e) => format!(
                "the saved board could not be read ({why}) and could not be moved aside ({e})"
            ),
        }
    }

    /// Writes `json` (a scene as [`Scene::to_json`] makes it): to a temporary file next to the board,
    /// then renamed over it.
    pub fn save_json(&self, json: &str) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// Writes the scene.
    pub fn save(&self, scene: &Scene) -> io::Result<()> {
        self.save_json(&scene.to_json())
    }
}

#[cfg(test)]
mod tests;

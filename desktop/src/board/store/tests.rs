//! Tests of the board file: where it lives, the round trip, the set-aside of a bad file.

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::board::geom::Point;
use crate::board::scene::{Kind, StrokeColor, Style};

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "femme-board-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join(DIR_NAME).join(FILE_NAME)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scene() -> Scene {
    let mut s = Scene::new();
    s.add(
        Kind::Rect {
            a: Point::new(1.0, 2.0),
            b: Point::new(30.0, 40.0),
        },
        Style::default(),
    );
    s.add(
        Kind::Text {
            at: Point::new(5.0, 6.0),
            text: "label\nsecond line".into(),
        },
        Style {
            color: StrokeColor::Pink,
            ..Style::default()
        },
    );
    s.sketchy = true;
    s
}

#[test]
fn the_path_follows_xdg_state_home_then_home() {
    let p = |x: Option<&str>, h: Option<&str>| path_from(x.map(Into::into), h.map(Into::into));
    assert_eq!(
        p(Some("/xdg/state"), Some("/home/u")),
        Some(PathBuf::from("/xdg/state/femme/board.json"))
    );
    assert_eq!(
        p(None, Some("/home/u")),
        Some(PathBuf::from("/home/u/.local/state/femme/board.json"))
    );
    // The spec says a relative XDG_STATE_HOME is invalid: ignored.
    assert_eq!(
        p(Some("relative/dir"), Some("/home/u")),
        Some(PathBuf::from("/home/u/.local/state/femme/board.json"))
    );
    assert_eq!(
        p(Some(""), Some("/home/u")),
        Some(PathBuf::from("/home/u/.local/state/femme/board.json"))
    );
    assert_eq!(p(None, None), None);
    assert_eq!(p(None, Some("not/absolute")), None);
}

#[test]
fn no_file_is_an_empty_board_without_a_warning() {
    let dir = TempDir::new();
    let loaded = Store::at(dir.file()).load();
    assert!(loaded.scene.is_empty());
    assert!(loaded.warning.is_none());
    // A store with no path loads and saves nothing, without failing.
    let none = Store { path: None };
    assert!(none.load().scene.is_empty());
    none.save(&scene()).unwrap();
}

#[test]
fn a_saved_board_comes_back_whole() {
    let dir = TempDir::new();
    let store = Store::at(dir.file());
    let mut s = scene();
    let name = s.next_export_name();
    store.save(&s).unwrap();
    assert!(dir.file().exists(), "the directory was made");
    let back = store.load();
    assert!(back.warning.is_none());
    assert_eq!(back.scene.shapes(), s.shapes());
    assert!(back.scene.sketchy);
    let mut back = back.scene;
    assert_ne!(
        back.next_export_name(),
        name,
        "the count of sent pictures is kept"
    );
    // A second save replaces the first, and no temporary file stays behind.
    s.clear();
    store.save(&s).unwrap();
    assert!(store.load().scene.is_empty());
    let leftovers: Vec<_> = fs::read_dir(dir.file().parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec![FILE_NAME.to_string()]);
}

#[test]
fn a_file_that_is_not_a_board_is_kept_aside_and_the_board_starts_empty() {
    let dir = TempDir::new();
    let file = dir.file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "{ this is not json").unwrap();
    let store = Store::at(file.clone());
    let loaded = store.load();
    assert!(loaded.scene.is_empty());
    let warning = loaded.warning.expect("the user is told");
    assert!(warning.contains("board.json.bad"), "{warning}");
    assert!(!file.exists(), "the bad file is out of the way");
    let bad = file.with_extension("json.bad");
    assert_eq!(fs::read_to_string(&bad).unwrap(), "{ this is not json");
    // The next save does not touch the kept file.
    store.save(&scene()).unwrap();
    assert_eq!(fs::read_to_string(&bad).unwrap(), "{ this is not json");
    assert_eq!(store.load().scene.len(), 2);
}

#[test]
fn a_board_from_a_newer_version_is_kept_aside_too() {
    let dir = TempDir::new();
    let file = dir.file();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, r#"{"version":999,"shapes":[]}"#).unwrap();
    let loaded = Store::at(file.clone()).load();
    assert!(loaded.warning.unwrap().contains("newer"));
    assert!(file.with_extension("json.bad").exists());
}

#[test]
fn saving_into_an_unwritable_place_is_an_error_not_a_panic() {
    let dir = TempDir::new();
    // The "directory" of the file is a regular file.
    let blocker = dir.0.join("femme");
    fs::write(&blocker, "x").unwrap();
    assert!(Store::at(dir.file()).save(&scene()).is_err());
}

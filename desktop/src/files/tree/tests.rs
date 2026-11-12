//! Tests of the tree model (fake listings, no gpui).

use super::*;

fn dir(name: &str) -> FsEntry {
    FsEntry {
        name: name.into(),
        dir: true,
        len: 0,
    }
}

fn file(name: &str, len: u64) -> FsEntry {
    FsEntry {
        name: name.into(),
        dir: false,
        len,
    }
}

/// `path`, `depth` and kind of every line, for comparing a whole tree at a glance.
fn shape(t: &Tree) -> Vec<String> {
    t.rows()
        .iter()
        .map(|r| {
            let tag = match r.kind {
                Kind::Up => "..".to_owned(),
                Kind::Dir { open: true } => "v".to_owned(),
                Kind::Dir { open: false } => ">".to_owned(),
                Kind::File { len } => format!("f{len}"),
                Kind::Loading => "loading".to_owned(),
                Kind::Empty => "empty".to_owned(),
                Kind::Failed => "failed".to_owned(),
                Kind::Transfer { id } => format!("up{id}"),
            };
            format!("{}{} {}", "  ".repeat(r.depth), tag, r.path)
        })
        .collect()
}

/// A tree whose root and `src` are listed: `src/` and `.git/` above `Cargo.toml`, `README.md`
/// and `.env`; `src` holds `ui/`, `main.rs`.
fn sample() -> Tree {
    let mut t = Tree::new();
    assert_eq!(t.wanted(), [""]);
    t.loaded(
        "",
        Ok(vec![
            file("README.md", 2048),
            dir("src"),
            file(".env", 12),
            dir(".git"),
            file("Cargo.toml", 700),
        ]),
    );
    assert!(t.set_open("src", true));
    assert_eq!(t.wanted(), ["src"]);
    t.loaded("src", Ok(vec![file("main.rs", 99), dir("ui")]));
    t
}

#[test]
fn names_are_plain_components_and_paths_never_climb() {
    for bad in ["", ".", "..", "a/b", "/", "/etc", "a\0b", "../x"] {
        assert_eq!(join("src", bad), None, "{bad:?}");
        assert_eq!(join("", bad), None, "{bad:?}");
    }
    assert_eq!(join("", "src").as_deref(), Some("src"));
    assert_eq!(join("src/ui", "mod.rs").as_deref(), Some("src/ui/mod.rs"));
    // Odd but legal names pass: dots inside, spaces, unicode, leading dot, a backslash.
    for ok in [".env", "a..b", "...", "my file", "é", "a\\b"] {
        assert!(join("d", ok).is_some(), "{ok:?}");
    }
    assert!(clean("") && clean("a") && clean("a/b/c"));
    for bad in ["/a", "a/", "a//b", "a/../b", "./a", "a/."] {
        assert!(!clean(bad), "{bad:?}");
    }
}

#[test]
fn the_header_path_puts_the_root_under_the_session_directory() {
    assert_eq!(under("/home/u/work", ""), "/home/u/work");
    assert_eq!(under("/home/u/work/", "src/ui"), "/home/u/work/src/ui");
    assert_eq!(under("/", ""), "/");
    assert_eq!(under("/", "a"), "/a");
    assert_eq!(under("", ""), "~");
    assert_eq!(under("", "a"), "~/a");
}

#[test]
fn path_parts() {
    assert_eq!(parent("a/b/c"), "a/b");
    assert_eq!(parent("a"), "");
    assert_eq!(parent(""), "");
    assert_eq!(base("a/b/c.rs"), "c.rs");
    assert_eq!(base("c.rs"), "c.rs");
    assert_eq!((depth(""), depth("a"), depth("a/b/c")), (0, 1, 3));
}

#[test]
fn a_new_tree_wants_its_root_once() {
    let mut t = Tree::new();
    assert_eq!(shape(&t), ["loading "]);
    assert!(!t.busy());
    assert_eq!(t.wanted(), [""]);
    assert!(t.busy());
    // The request is out: not asked for twice.
    assert!(t.wanted().is_empty());
    t.loaded("", Ok(vec![]));
    assert!(!t.busy());
    assert_eq!(shape(&t), ["empty "]);
    assert!(t.wanted().is_empty());
}

#[test]
fn listings_are_sorted_directories_first_hidden_ones_listed() {
    let t = sample();
    assert_eq!(
        shape(&t),
        [
            "> .git",
            "v src",
            "  > src/ui",
            "  f99 src/main.rs",
            "f12 .env",
            "f700 Cargo.toml",
            "f2048 README.md",
        ]
    );
    // Without regard to case: Cargo.toml before README.md, .env before both.
    assert!(t.rows().iter().any(Row::hidden));
    assert_eq!(t.rows()[0].name(), ".git");
}

#[test]
fn names_that_could_escape_are_dropped_from_a_listing() {
    let mut t = Tree::new();
    t.wanted();
    t.loaded(
        "",
        Ok(vec![
            file("ok", 1),
            file("..", 2),
            dir("a/b"),
            file("", 3),
            dir("."),
            file("c\0d", 4),
            file("ok", 1),
        ]),
    );
    // The bad names are gone and the duplicate is shown once.
    assert_eq!(shape(&t), ["f1 ok"]);
}

#[test]
fn opening_is_lazy_and_closing_keeps_the_listing() {
    let mut t = sample();
    assert!(t.set_open("src/ui", true));
    assert_eq!(shape(&t)[2..4], ["  v src/ui", "    loading src/ui"]);
    assert_eq!(t.wanted(), ["src/ui"]);
    t.loaded("src/ui", Ok(vec![file("mod.rs", 5)]));
    assert_eq!(shape(&t)[3], "    f5 src/ui/mod.rs");
    // Close and open again: listed already, nothing to ask.
    assert!(t.set_open("src/ui", false));
    assert!(
        !t.set_open("src/ui", false),
        "closing twice changes nothing"
    );
    assert!(t.set_open("src/ui", true));
    assert!(t.wanted().is_empty());
    assert_eq!(shape(&t)[3], "    f5 src/ui/mod.rs");
}

#[test]
fn an_open_directory_below_a_closed_one_is_asked_for_only_when_shown() {
    let mut t = sample();
    t.set_open("src/ui", true);
    t.set_open("src", false);
    // `src/ui` stays open but nothing shows it: not worth a request.
    assert_eq!(t.wanted(), Vec::<String>::new());
    t.set_open("src", true);
    assert_eq!(t.wanted(), ["src/ui"]);
}

#[test]
fn a_failed_listing_says_so_and_a_retry_asks_again() {
    let mut t = Tree::new();
    t.wanted();
    t.loaded("", Err("Unsupported: not yet".into()));
    assert_eq!(shape(&t), ["failed "]);
    assert!(!t.busy());
    // Not asked for again by itself.
    assert!(t.wanted().is_empty());
    t.retry("");
    assert_eq!(t.wanted(), [""]);
    t.loaded("", Ok(vec![file("a", 1)]));
    assert_eq!(shape(&t), ["f1 a"]);
}

#[test]
fn a_failed_refresh_keeps_the_old_entries_and_adds_the_retry_line() {
    let mut t = sample();
    t.refresh();
    // Everything on show is asked for again, and stays on screen meanwhile.
    let before = shape(&t);
    assert_eq!(before.len(), 7);
    assert_eq!(t.wanted(), ["", "src"]);
    assert_eq!(shape(&t), before);
    t.loaded("src", Err("timeout".into()));
    let after = shape(&t);
    assert_eq!(after.len(), 8);
    assert_eq!(after[4], "  failed src");
    assert!(
        t.wanted().is_empty(),
        "the failure is shown, not retried by itself"
    );
}

#[test]
fn refresh_drops_what_is_off_screen_and_gives_up_requests_in_flight() {
    let mut t = sample();
    t.set_open("src/ui", true);
    t.wanted();
    t.loaded("src/ui", Ok(vec![file("mod.rs", 5)]));
    // `src` closes: its subtree is off screen.
    t.set_open("src", false);
    t.set_open("src", true);
    t.refresh();
    assert_eq!(t.wanted(), ["", "src", "src/ui"]);
    // A refresh while requests are out forgets them: all asked for again.
    t.refresh();
    assert_eq!(t.wanted(), ["", "src", "src/ui"]);
    // Close `src`, refresh: `src/ui` is not on show any more and is dropped.
    t.set_open("src", false);
    t.refresh();
    assert_eq!(t.wanted(), [""]);
    t.loaded("", Ok(vec![dir("src")]));
    t.set_open("src", true);
    assert_eq!(
        t.wanted(),
        ["src"],
        "src was dropped by the refresh: asked for afresh"
    );
}

#[test]
fn old_listings_on_show_are_asked_for_again_and_stay_on_screen() {
    let mut t = sample();
    // Just answered: nothing is old yet.
    assert!(!t.expire(Duration::from_secs(3600)));
    assert!(t.wanted().is_empty());
    let before = shape(&t);
    // Everything counts as old with no time to live: the root and `src`, once each.
    assert!(t.expire(Duration::ZERO));
    assert_eq!(shape(&t), before, "kept on screen");
    assert_eq!(t.wanted(), ["", "src"]);
    assert!(
        !t.expire(Duration::ZERO),
        "asked for: loading, not marked again"
    );
    t.loaded("", Ok(vec![file("only.txt", 1)]));
    assert_eq!(t.rows()[0].path, "only.txt");
}

#[test]
fn a_failed_directory_is_not_asked_for_again_by_the_clock() {
    let mut t = Tree::new();
    t.wanted();
    t.loaded("", Err("Unsupported".into()));
    assert!(!t.expire(Duration::ZERO));
    assert!(t.wanted().is_empty());
    // Nor is one whose refresh failed: its entries stay, its retry line says why.
    let mut t = sample();
    assert!(t.expire(Duration::ZERO));
    t.wanted();
    t.loaded("src", Err("timeout".into()));
    assert!(!t.expire(Duration::ZERO));
    assert!(
        t.wanted().is_empty(),
        "the root is still out, src shows its retry line"
    );
}

#[test]
fn an_answer_for_a_directory_nobody_asked_about_is_ignored() {
    let mut t = Tree::new();
    t.loaded("ghost", Ok(vec![file("x", 1)]));
    assert_eq!(shape(&t), ["loading "]);
}

#[test]
fn entering_a_directory_re_roots_and_up_stops_at_the_session_directory() {
    let mut t = sample();
    assert!(
        !t.enter(""),
        "the session's directory is where the view starts"
    );
    assert!(!t.enter("a/../b"));
    assert!(t.enter("src"));
    assert_eq!(t.root(), "src");
    assert_eq!(shape(&t), [".. ", "> src/ui", "f99 src/main.rs"]);
    assert_eq!(
        t.rows()[0].path,
        "",
        "the up line leads to the session's directory"
    );
    assert!(t.enter("src/ui"));
    assert_eq!(shape(&t)[..2], [".. src", "loading src/ui"]);
    assert_eq!(t.wanted(), ["src/ui"]);
    // Up twice: back at the top, the cursor on the directory just left each time.
    assert!(t.up());
    assert_eq!((t.root(), t.selected()), ("src", Some("src/ui")));
    assert!(t.up());
    assert_eq!((t.root(), t.selected()), ("", Some("src")));
    assert!(!t.up(), "never above the session's directory");
    assert_eq!(
        t.rows()[0].kind,
        Kind::Dir { open: false },
        "no up line at the top"
    );
}

#[test]
fn the_cursor_lands_on_the_first_entry_of_a_directory_once_it_is_listed() {
    let mut t = sample();
    // Not listed yet: nothing to land on, the cursor waits.
    t.set_open("src/ui", true);
    assert!(t.enter("src/ui"));
    assert_eq!(shape(&t), [".. src", "loading src/ui"]);
    assert_eq!(t.selected(), None);
    assert_eq!(t.wanted(), ["src/ui"]);
    t.loaded("src/ui", Ok(vec![file("b.rs", 2), file("a.rs", 1)]));
    assert_eq!(t.selected(), Some("src/ui/a.rs"), "not on the up line");
    // Once is enough: a later answer does not pull the cursor back.
    assert!(t.select(Some("src/ui/b.rs")));
    t.loaded("src/ui", Ok(vec![file("a.rs", 1), file("b.rs", 3)]));
    assert_eq!(t.selected(), Some("src/ui/b.rs"));
}

#[test]
fn a_cursor_put_elsewhere_while_the_listing_is_out_stays() {
    let mut t = sample();
    t.set_open("src/ui", true);
    t.enter("src/ui");
    t.wanted();
    assert_eq!(t.step(1), Some(0), "down from nowhere lands on the up line");
    t.loaded("src/ui", Ok(vec![file("a.rs", 1)]));
    assert_eq!(t.selected(), Some(".."));
}

#[test]
fn an_empty_or_failed_directory_gives_the_cursor_nothing_and_forgets_it() {
    let mut t = sample();
    t.set_open("src/ui", true);
    t.enter("src/ui");
    t.wanted();
    t.loaded("src/ui", Ok(vec![]));
    assert_eq!(t.selected(), None);
    // The wish is gone: files that show up later do not take the cursor.
    t.refresh();
    t.wanted();
    t.loaded("src/ui", Ok(vec![file("late.rs", 1)]));
    assert_eq!(t.selected(), None);
    let mut t = sample();
    t.set_open("src/ui", true);
    t.enter("src/ui");
    t.wanted();
    t.loaded("src/ui", Err("Bad: permission denied".into()));
    assert_eq!(t.selected(), None);
}

#[test]
fn an_open_directory_keeps_its_state_while_the_view_moves() {
    let mut t = sample();
    t.set_open("src/ui", true);
    t.wanted();
    t.loaded("src/ui", Ok(vec![file("mod.rs", 5)]));
    t.enter("src");
    assert_eq!(
        shape(&t),
        [".. ", "v src/ui", "  f5 src/ui/mod.rs", "f99 src/main.rs"]
    );
    t.up();
    assert_eq!(
        shape(&t)[..4],
        ["> .git", "v src", "  v src/ui", "    f5 src/ui/mod.rs"]
    );
}

#[test]
fn the_cursor_steps_over_placeholders_and_stops_at_the_ends() {
    let mut t = sample();
    t.set_open("src/ui", true); // a loading line under src/ui
    let at = |t: &mut Tree, d: isize| {
        t.step(d);
        t.selected().map(str::to_owned)
    };
    assert_eq!(
        at(&mut t, 1).as_deref(),
        Some(".git"),
        "down from nothing: the first line"
    );
    assert_eq!(at(&mut t, 1).as_deref(), Some("src"));
    assert_eq!(at(&mut t, 1).as_deref(), Some("src/ui"));
    assert_eq!(
        at(&mut t, 1).as_deref(),
        Some("src/main.rs"),
        "the loading line is skipped"
    );
    assert_eq!(at(&mut t, -2).as_deref(), Some("src"));
    assert_eq!(at(&mut t, -1).as_deref(), Some(".git"));
    assert_eq!(at(&mut t, -1).as_deref(), Some(".git"), "stops at the top");
    assert_eq!(
        at(&mut t, 100).as_deref(),
        Some("README.md"),
        "and at the bottom"
    );
    assert_eq!(t.edge(false), Some(0));
    assert_eq!(t.selected(), Some(".git"));
    assert_eq!(t.edge(true), Some(t.rows().len() - 1));
    assert_eq!(t.selected(), Some("README.md"));
    t.select(None);
    t.step(-1);
    assert_eq!(
        t.selected(),
        Some("README.md"),
        "up from nothing: the last line"
    );
    assert_eq!(Tree::new().step(1), None, "nothing to stop on");
}

#[test]
fn right_opens_then_enters_and_left_closes_then_climbs() {
    let mut t = sample();
    assert!(t.select(Some(".git")));
    assert!(t.key_right(), "closed: opens");
    assert!(t.is_open(".git"));
    t.loaded(".git", Ok(vec![]));
    // An open directory with nothing to stop on below it: right does nothing more.
    assert!(!t.key_right());
    assert!(t.select(Some("src")));
    assert!(
        t.key_right() && t.selected() == Some("src/ui"),
        "open: onto the first child"
    );
    assert!(t.key_right() && t.is_open("src/ui"), "closed: opens");
    assert_eq!(t.selected(), Some("src/ui"), "and the cursor stays");
    // Left on a file climbs to its directory; on an open directory it closes it.
    assert!(t.select(Some("src/main.rs")));
    assert!(t.key_left() && t.selected() == Some("src"));
    assert!(t.key_left() && !t.is_open("src"));
    assert!(
        !t.key_left(),
        "a top-level closed directory has nowhere to climb"
    );
    assert!(t.select(Some("README.md")));
    assert!(!t.key_left(), "a top-level file has nowhere to climb");
    assert!(!t.key_right());
}

#[test]
fn closing_a_directory_takes_the_cursor_to_it() {
    let mut t = sample();
    assert!(t.select(Some("src/main.rs")));
    assert!(t.set_open("src", false));
    assert_eq!(t.selected(), Some("src"));
    // Deeper: the nearest directory that is still on show.
    t.set_open("src", true);
    t.set_open("src/ui", true);
    t.wanted();
    t.loaded("src/ui", Ok(vec![file("mod.rs", 5)]));
    assert!(t.select(Some("src/ui/mod.rs")));
    t.set_open("src", false);
    assert_eq!(t.selected(), Some("src"));
    // Entering a directory puts the cursor on its first entry (it is listed already).
    t.enter("src");
    assert_eq!(t.selected(), Some("src/ui"));
    t.select(Some(".."));
    assert_eq!(t.selected_row().map(|r| r.kind.clone()), Some(Kind::Up));
    assert!(
        !t.select(Some("nope")),
        "a line that is not on show cannot be selected"
    );
}

#[test]
fn toggling_flips_the_directory() {
    let mut t = sample();
    assert!(t.toggle(".git") && t.is_open(".git"));
    assert!(t.toggle(".git") && !t.is_open(".git"));
    assert!(!t.toggle(""), "the root has no line to close");
    assert!(!t.toggle("../x"));
}

fn up(path: &str, id: u64, yields: bool) -> Pending {
    Pending {
        path: path.into(),
        id,
        yields,
    }
}

#[test]
fn an_upload_shows_at_its_sorted_place_among_the_files() {
    let mut t = sample();
    t.set_pending(vec![
        up("b.txt", 1, false),
        up("zz", 2, false),
        up("src/a.rs", 3, false),
        up("closed/x", 4, false),
    ]);
    assert_eq!(
        shape(&t),
        [
            "> .git",
            "v src",
            "  > src/ui",
            "  up3 src/a.rs",
            "  f99 src/main.rs",
            "f12 .env",
            "up1 b.txt",
            "f700 Cargo.toml",
            "f2048 README.md",
            "up2 zz",
        ],
        "{:#?}",
        shape(&t)
    );
    // The cursor skips them.
    t.select(Some(".env"));
    t.step(1);
    assert_eq!(t.selected(), Some("Cargo.toml"));
}

#[test]
fn a_finished_upload_gives_way_to_the_listed_file_and_a_running_one_does_not() {
    let mut t = sample();
    t.set_pending(vec![up("Cargo.toml", 1, true), up("README.md", 2, false)]);
    let ups: Vec<_> = shape(&t)
        .into_iter()
        .filter(|l| l.starts_with("up"))
        .collect();
    assert_eq!(ups, ["up2 README.md"]);
}

#[test]
fn an_upload_into_an_empty_directory_takes_the_place_of_the_empty_line() {
    let mut t = Tree::new();
    t.wanted();
    t.loaded("", Ok(vec![]));
    t.set_pending(vec![up("n.txt", 7, false)]);
    assert_eq!(shape(&t), ["up7 n.txt"]);
}

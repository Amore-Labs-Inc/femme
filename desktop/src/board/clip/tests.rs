//! Tests of the clipboard helpers: which ones are tried, and that the bytes reach the program.

use std::fs;
use std::path::PathBuf;

use super::*;

/// A fresh directory under the system temp dir.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("femme-board-clip-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn programs(list: &[Helper]) -> Vec<&'static str> {
    list.iter().map(|h| h.program).collect()
}

#[test]
fn wayland_asks_wl_copy_first_and_x11_xclip() {
    assert_eq!(programs(&helpers(true, true)), ["wl-copy", "xclip"]);
    assert_eq!(programs(&helpers(true, false)), ["wl-copy"]);
    assert_eq!(programs(&helpers(false, true)), ["xclip"]);
    assert!(helpers(false, false).is_empty());
}

#[test]
fn both_helpers_are_told_the_data_is_a_png() {
    for h in helpers(true, true) {
        assert!(h.args.contains(&"image/png"), "{}", h.program);
    }
}

#[test]
fn the_bytes_reach_the_program_unchanged_even_when_the_pipe_fills_up() {
    let dir = temp_dir("bytes");
    let out = dir.join("out.png");
    // Every byte value, and far more than a pipe holds.
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    pipe_to("sh", &["-c", "cat > \"$0\"", out.to_str().unwrap()], &bytes).unwrap();
    assert_eq!(fs::read(&out).unwrap(), bytes);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_program_is_not_found() {
    let e = pipe_to("femme-no-such-helper", &[], b"x").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_program_that_fails_is_an_error_that_names_it() {
    let e = pipe_to("sh", &["-c", "exit 3"], b"x").unwrap_err();
    assert!(e.to_string().contains("sh failed"), "{e}");
}

#[test]
fn a_program_that_quits_without_reading_is_no_hang_and_no_panic() {
    let bytes = vec![7u8; 1 << 20];
    assert!(pipe_to("sh", &["-c", "exit 0"], &bytes).is_ok());
}

#[test]
fn a_missing_helper_is_skipped_for_the_next_one() {
    let list = [
        Helper {
            program: "femme-no-such-helper",
            args: &[],
        },
        Helper {
            program: "sh",
            args: &["-c", "cat > /dev/null"],
        },
    ];
    assert_eq!(copy_png(&list, b"png"), Ok(()));
}

#[test]
fn a_helper_that_fails_is_reported_when_no_other_one_works() {
    let list = [
        Helper {
            program: "sh",
            args: &["-c", "exit 1"],
        },
        Helper {
            program: "femme-no-such-helper",
            args: &[],
        },
    ];
    let e = copy_png(&list, b"png").unwrap_err();
    assert!(e.contains("sh failed"), "{e}");
}

#[test]
fn with_no_helper_at_all_it_says_what_to_install() {
    let e = copy_png(&[], b"png").unwrap_err();
    assert!(e.contains("wl-copy") && e.contains("xclip"), "{e}");
}

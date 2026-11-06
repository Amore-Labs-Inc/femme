//! Tests of the clipboard helpers: which ones are tried, that the bytes reach the program, and
//! which bytes a picture puts on the clipboard.

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

/// A 3x2 picture in `format`.
fn picture(format: image::ImageFormat) -> Vec<u8> {
    let rgb = image::ImageBuffer::from_pixel(3, 2, image::Rgb([200u8, 40, 40]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(rgb)
        .write_to(&mut Cursor::new(&mut bytes), format)
        .unwrap();
    bytes
}

/// The size of a PNG, which must decode as one.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).unwrap();
    (decoded.width(), decoded.height())
}

#[test]
fn a_png_goes_on_the_clipboard_as_it_is() {
    let png = picture(image::ImageFormat::Png);
    let out = png_of(&png).unwrap();
    assert!(matches!(out, Cow::Borrowed(_)), "a PNG is not copied");
    assert_eq!(out.as_ref(), png.as_slice());
}

#[test]
fn every_other_raster_becomes_a_png_of_the_same_size() {
    for format in [
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::Bmp,
        image::ImageFormat::Tiff,
        image::ImageFormat::WebP,
    ] {
        let src = picture(format);
        let out = png_of(&src).unwrap_or_else(|e| panic!("{format:?}: {e}"));
        assert_eq!(png_size(&out), (3, 2), "{format:?}");
    }
}

/// `jpeg` with an EXIF block after its start marker that says: turn it 90 degrees clockwise.
fn turned(jpeg: &[u8]) -> Vec<u8> {
    let exif: &[u8] = b"Exif\0\0MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
    let mut out = jpeg[..2].to_vec();
    out.extend([0xFF, 0xE1]);
    out.extend(((exif.len() + 2) as u16).to_be_bytes());
    out.extend(exif);
    out.extend(&jpeg[2..]);
    out
}

#[test]
fn a_photo_is_copied_the_right_way_up_as_the_viewer_shows_it() {
    let src = turned(&picture(image::ImageFormat::Jpeg));
    let out = png_of(&src).unwrap();
    assert_eq!(png_size(&out), (2, 3));
}

#[test]
fn bytes_that_are_no_picture_say_so() {
    let e = png_of(b"not a picture at all").unwrap_err();
    assert!(e.contains("could not"), "{e}");
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"3\" height=\"2\"/>";
    assert!(png_of(svg).is_err());
}

#[test]
fn a_vector_picture_is_the_one_kind_that_cannot_be_copied() {
    let of = |format| Image::from_bytes(format, Vec::new());
    assert!(!copyable(&of(ImageFormat::Svg)));
    for format in [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::Webp,
        ImageFormat::Bmp,
        ImageFormat::Tiff,
        ImageFormat::Ico,
    ] {
        assert!(copyable(&of(format)), "{format:?}");
    }
}

//! The files attached to a user message. The daemon appends one `Attached: <path>` line per
//! uploaded file to the prompt's text (`docs/desktop-ui.md`, `Prompt` with attachments); the
//! transcript shows them as chips and thumbnails, not as text.

use gpui::ImageFormat;

/// The prefix of an attachment line.
const PREFIX: &str = "Attached: ";

/// One attached file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attached {
    /// The path the daemon wrote the file to.
    pub path: String,
    /// The file's name: the last component of the path.
    pub name: String,
    /// The file is an image the view can show as a thumbnail.
    pub image: bool,
}

/// Splits a user message into what the user wrote and the files attached to it. The attachment
/// lines are taken out wherever they stand; the text loses the blank lines they leave behind.
pub fn split(text: &str) -> (String, Vec<Attached>) {
    let mut kept = Vec::new();
    let mut files = Vec::new();
    for line in text.lines() {
        match line.strip_prefix(PREFIX).map(str::trim) {
            Some(path) if !path.is_empty() => files.push(attached(path)),
            _ => kept.push(line),
        }
    }
    let text = kept.join("\n");
    (text.trim_end().to_owned(), files)
}

fn attached(path: &str) -> Attached {
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    Attached {
        image: image_format_of_name(&name).is_some(),
        name,
        path: path.to_owned(),
    }
}

/// The image format a file name's extension says.
pub fn image_format_of_name(name: &str) -> Option<ImageFormat> {
    let (_, ext) = name.rsplit_once('.')?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "svg" => ImageFormat::Svg,
        _ => return None,
    })
}

/// The image format of `bytes`, from their first bytes; the file name's extension when they are
/// none of the known ones. `None` when it is not an image gpui draws.
pub fn image_format(name: &str, bytes: &[u8]) -> Option<ImageFormat> {
    let svg = || {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
        let head = head.trim_start_matches('\u{feff}').trim_start();
        head.starts_with("<svg") || head.starts_with("<?xml")
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if svg() {
        Some(ImageFormat::Svg)
    } else {
        image_format_of_name(name).filter(|f| *f != ImageFormat::Svg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_lines_become_files_and_leave_the_text() {
        let (text, files) = split(
            "look at these\n\nAttached: /home/u/.local/state/femme/uploads/s1/pic.PNG\nAttached: /tmp/notes.txt",
        );
        assert_eq!(text, "look at these");
        assert_eq!(
            files,
            [
                Attached {
                    path: "/home/u/.local/state/femme/uploads/s1/pic.PNG".into(),
                    name: "pic.PNG".into(),
                    image: true
                },
                Attached {
                    path: "/tmp/notes.txt".into(),
                    name: "notes.txt".into(),
                    image: false
                },
            ]
        );
    }

    #[test]
    fn a_message_of_only_attachments_has_no_text() {
        let (text, files) = split("\n\nAttached: a.jpg");
        assert_eq!(text, "");
        assert_eq!(files.len(), 1);
        let (text, files) = split("Attached: a.jpg");
        assert_eq!((text.as_str(), files.len()), ("", 1));
    }

    #[test]
    fn text_without_attachments_is_left_alone() {
        let (text, files) = split("line 1\n\n  indented\nAttached:  \nattached: lower");
        assert_eq!(text, "line 1\n\n  indented\nAttached:  \nattached: lower");
        assert!(files.is_empty());
        assert_eq!(split("").0, "");
    }

    #[test]
    fn attachment_lines_are_found_in_the_middle_and_trimmed() {
        let (text, files) = split("a\nAttached:   /x/y z.gif  \r\nb\n");
        assert_eq!(text, "a\nb");
        assert_eq!(files[0].path, "/x/y z.gif");
        assert_eq!(files[0].name, "y z.gif");
        assert!(files[0].image);
    }

    #[test]
    fn the_format_comes_from_the_bytes_then_the_name() {
        let png = b"\x89PNG\r\n\x1a\n....";
        assert_eq!(image_format("x.bin", png), Some(ImageFormat::Png));
        assert_eq!(
            image_format("x.png", &[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(image_format("x", b"GIF89a.."), Some(ImageFormat::Gif));
        assert_eq!(
            image_format("x", b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(image_format("x", b"BM\0\0"), Some(ImageFormat::Bmp));
        assert_eq!(
            image_format("x", b"  <?xml version=\"1.0\"?><svg/>"),
            Some(ImageFormat::Svg)
        );
        // Unknown bytes fall back to the extension (a format we sniff nothing for: none here).
        assert_eq!(image_format("photo.webp", b"junk"), Some(ImageFormat::Webp));
        assert_eq!(image_format("notes.txt", b"hello"), None);
        // An extension of svg does not make binary junk an svg.
        assert_eq!(image_format("fake.svg", b"\x00\x01"), None);
        assert_eq!(image_format("noext", b""), None);
    }
}

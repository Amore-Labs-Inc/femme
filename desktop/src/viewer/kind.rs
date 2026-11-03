//! What a file is: its magic bytes first, its extension second, and only then what its content looks
//! like. Pure functions over a name and the bytes, so every rule here is unit-tested.

/// A raster image format the viewer decodes through the `image` crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raster {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF, animated or not.
    Gif,
    /// WebP, animated or not.
    Webp,
    /// Windows bitmap.
    Bmp,
    /// TIFF.
    Tiff,
    /// Windows icon.
    Ico,
}

impl Raster {
    /// The `image` crate's identifier for this format.
    pub(crate) fn format(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Gif => image::ImageFormat::Gif,
            Self::Webp => image::ImageFormat::WebP,
            Self::Bmp => image::ImageFormat::Bmp,
            Self::Tiff => image::ImageFormat::Tiff,
            Self::Ico => image::ImageFormat::Ico,
        }
    }

    /// Short name for captions.
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Ico => "ICO",
        }
    }
}

/// What a file holds, as far as the viewer can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A raster image.
    Raster(Raster),
    /// An SVG document.
    Svg,
    /// A PDF document.
    Pdf,
    /// UTF-8 text (source code, markdown, logs, ...).
    Text,
    /// Anything else. The label says what the magic bytes suggest (`"ELF executable"`), or
    /// `"unknown"`.
    Binary(&'static str),
}

impl Kind {
    /// A few words for the notice and the captions: `"PNG image"`, `"PDF document"`, ...
    pub fn label(&self) -> String {
        match self {
            Self::Raster(r) => format!("{} image", r.name()),
            Self::Svg => "SVG image".into(),
            Self::Pdf => "PDF document".into(),
            Self::Text => "text".into(),
            Self::Binary(what) => (*what).into(),
        }
    }
}

/// How much of the start of a file decides whether it is text.
pub(crate) const TEXT_SNIFF: usize = 8 * 1024;

/// How much of a text file is read to find out whether its root element is `<svg` (editors put long
/// comments and metadata before it).
const SVG_SCAN: usize = 512 * 1024;

/// The part of a file name after its last dot, lower-cased; empty when there is none.
pub(crate) fn extension(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Classifies a file. Magic bytes win over the extension (a `.jpg` that holds a PNG is a PNG); the
/// extension only breaks ties where the content cannot: the weak ICO signature and gzip'd SVG.
pub fn detect(name: &str, data: &[u8]) -> Kind {
    if let Some(raster) = raster_magic(data) {
        return Kind::Raster(raster);
    }
    let ext = extension(name);
    if has_ico_magic(data) && (ext == "ico" || ext == "cur") {
        return Kind::Raster(Raster::Ico);
    }
    if has_pdf_header(data) {
        return Kind::Pdf;
    }
    if looks_like_text(data) {
        return if looks_like_svg(data) {
            Kind::Svg
        } else {
            Kind::Text
        };
    }
    if ext == "svgz" && data.starts_with(&[0x1f, 0x8b]) {
        // usvg inflates gzip by itself.
        return Kind::Svg;
    }
    Kind::Binary(binary_label(data))
}

/// The raster format named by the first bytes, if any.
fn raster_magic(data: &[u8]) -> Option<Raster> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Raster::Png)
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Raster::Jpeg)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some(Raster::Gif)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Raster::Webp)
    } else if is_bmp(data) {
        Some(Raster::Bmp)
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        Some(Raster::Tiff)
    } else {
        None
    }
}

/// `BM` alone is two printable letters; the DIB header size that follows the 14-byte file header
/// is what tells a bitmap from a text file that starts with "BM".
fn is_bmp(data: &[u8]) -> bool {
    if data.len() < 26 || &data[..2] != b"BM" {
        return false;
    }
    let dib = u32::from_le_bytes([data[14], data[15], data[16], data[17]]);
    matches!(dib, 12 | 40 | 52 | 56 | 64 | 108 | 124)
}

/// Icon directory header: reserved 0, type 1 (icon) or 2 (cursor), at least one image.
fn has_ico_magic(data: &[u8]) -> bool {
    data.len() >= 6
        && data[0] == 0
        && data[1] == 0
        && (data[2] == 1 || data[2] == 2)
        && data[3] == 0
        && (data[4] != 0 || data[5] != 0)
}

/// PDF readers accept up to 1 KiB of junk before `%PDF-`.
fn has_pdf_header(data: &[u8]) -> bool {
    let head = &data[..data.len().min(1024)];
    head.windows(5).any(|w| w == b"%PDF-")
}

/// Text: no NUL byte and valid UTF-8 in the first 8 KiB. A multi-byte character cut by the end of
/// that window does not count against it; an invalid byte does.
pub(crate) fn looks_like_text(data: &[u8]) -> bool {
    let head = &data[..data.len().min(TEXT_SNIFF)];
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // `error_len() == None`: the input ended inside a sequence, not an invalid byte.
        Err(e) => e.error_len().is_none(),
    }
}

/// An SVG is an XML document whose root element is `<svg`. Skips the byte order mark, the XML
/// declaration, a doctype and comments, then asks for the root: an HTML page with an inline `<svg`
/// somewhere in it is not an SVG file.
fn looks_like_svg(data: &[u8]) -> bool {
    let data = &data[..data.len().min(SVG_SCAN)];
    let mut rest = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    loop {
        rest = rest.trim_ascii_start();
        let skip = if rest.starts_with(b"<?") {
            find(rest, b"?>").map(|i| i + 2)
        } else if rest.starts_with(b"<!--") {
            find(rest, b"-->").map(|i| i + 3)
        } else if rest.starts_with(b"<!") {
            end_of_declaration(rest)
        } else {
            return rest.starts_with(b"<svg");
        };
        match skip {
            Some(n) => rest = &rest[n..],
            None => return false,
        }
    }
}

/// Position of the first occurrence of `needle`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The length of a `<!DOCTYPE ...>` declaration, whose internal subset (`[ ... ]`) may itself contain
/// `>`.
fn end_of_declaration(rest: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, &b) in rest.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'>' if depth == 0 => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// A name for a binary format, from its signature.
fn binary_label(data: &[u8]) -> &'static str {
    let starts = |sig: &[u8]| data.starts_with(sig);
    if starts(b"\x7fELF") {
        "ELF executable"
    } else if starts(&[0xcf, 0xfa, 0xed, 0xfe])
        || starts(&[0xce, 0xfa, 0xed, 0xfe])
        || starts(&[0xfe, 0xed, 0xfa, 0xcf])
        || starts(&[0xfe, 0xed, 0xfa, 0xce])
    {
        "Mach-O binary"
    } else if starts(&[0xca, 0xfe, 0xba, 0xbe]) {
        "Java class or Mach-O universal binary"
    } else if starts(b"MZ") {
        "Windows executable"
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        "zip archive"
    } else if starts(&[0x1f, 0x8b]) {
        "gzip archive"
    } else if starts(b"BZh") {
        "bzip2 archive"
    } else if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        "xz archive"
    } else if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "zstd archive"
    } else if starts(b"7z\xbc\xaf\x27\x1c") {
        "7-zip archive"
    } else if starts(b"Rar!\x1a\x07") {
        "RAR archive"
    } else if data.len() > 262 && &data[257..262] == b"ustar" {
        "tar archive"
    } else if starts(b"!<arch>\n") {
        "ar archive"
    } else if starts(b"SQLite format 3\0") {
        "SQLite database"
    } else if starts(b"\0asm") {
        "WebAssembly module"
    } else if starts(b"wOFF") {
        "WOFF font"
    } else if starts(b"wOF2") {
        "WOFF2 font"
    } else if starts(&[0, 1, 0, 0]) || starts(b"true") {
        "TrueType font"
    } else if starts(b"OTTO") {
        "OpenType font"
    } else if starts(b"ID3") || (data.len() > 1 && data[0] == 0xff && data[1] & 0xe0 == 0xe0) {
        "MP3 audio"
    } else if starts(b"OggS") {
        "Ogg media"
    } else if starts(b"fLaC") {
        "FLAC audio"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        "WAV audio"
    } else if data.len() >= 12 && &data[4..8] == b"ftyp" {
        "MP4 media"
    } else if starts(&[0x1a, 0x45, 0xdf, 0xa3]) {
        "Matroska media"
    } else if starts(&[0xd0, 0xcf, 0x11, 0xe0]) {
        "OLE document"
    } else if starts(b"8BPS") {
        "Photoshop image"
    } else if starts(b"\x93NUMPY") {
        "NumPy array"
    } else if starts(b"PAR1") {
        "Parquet file"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn magic_bytes_beat_the_extension() {
        assert_eq!(detect("photo.jpg", PNG), Kind::Raster(Raster::Png));
        assert_eq!(detect("noext", PNG), Kind::Raster(Raster::Png));
        assert_eq!(
            detect("a.txt", &[0xff, 0xd8, 0xff, 0xe0, 0, 16]),
            Kind::Raster(Raster::Jpeg)
        );
        assert_eq!(
            detect("x", b"GIF89a\x01\0\x01\0"),
            Kind::Raster(Raster::Gif)
        );
        assert_eq!(
            detect("x", b"GIF87a\x01\0\x01\0"),
            Kind::Raster(Raster::Gif)
        );
        assert_eq!(
            detect("x.png", b"RIFF\x24\0\0\0WEBPVP8 "),
            Kind::Raster(Raster::Webp)
        );
        assert_eq!(detect("x", b"II*\0\x08\0\0\0"), Kind::Raster(Raster::Tiff));
        assert_eq!(detect("x", b"MM\0*\0\0\0\x08"), Kind::Raster(Raster::Tiff));
    }

    #[test]
    fn the_extension_does_not_make_text_an_image() {
        assert_eq!(
            detect("error.png", b"500 Internal Server Error\n"),
            Kind::Text
        );
        assert_eq!(detect("empty.png", b""), Kind::Text);
        assert_eq!(detect("fake.pdf", b"just words"), Kind::Text);
    }

    #[test]
    fn bmp_needs_a_dib_header_not_just_bm() {
        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[14] = 40;
        assert_eq!(detect("x.bmp", &bmp), Kind::Raster(Raster::Bmp));
        assert_eq!(
            detect(
                "notes.txt",
                b"BM is a two letter word and this is text, long enough."
            ),
            Kind::Text
        );
    }

    #[test]
    fn ico_signature_is_only_believed_with_the_extension() {
        let ico = [0, 0, 1, 0, 1, 0, 16, 16, 0, 0];
        assert_eq!(detect("favicon.ico", &ico), Kind::Raster(Raster::Ico));
        assert_eq!(detect("FAVICON.ICO", &ico), Kind::Raster(Raster::Ico));
        assert!(matches!(detect("favicon.bin", &ico), Kind::Binary(_)));
    }

    #[test]
    fn pdf_header_may_follow_a_little_junk() {
        assert_eq!(detect("a.pdf", b"%PDF-1.7\n%\xe2\xe3"), Kind::Pdf);
        let mut junk = vec![b' '; 200];
        junk.extend_from_slice(b"%PDF-1.4\n\x80\x81");
        assert_eq!(detect("x", &junk), Kind::Pdf);
        let mut far = vec![b'a'; 2000];
        far.extend_from_slice(b"%PDF-1.4");
        assert_eq!(detect("x.pdf", &far), Kind::Text);
    }

    #[test]
    fn svg_is_xml_whose_root_is_svg() {
        let svg = b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        assert_eq!(detect("logo.svg", svg), Kind::Svg);
        assert_eq!(detect("logo", svg), Kind::Svg);
        assert_eq!(detect("logo.svg", b"<html></html>"), Kind::Text);
        assert_eq!(detect("bare.svg", b"\xef\xbb\xbf  <svg/>"), Kind::Svg);
        // Comments, a doctype with an internal subset and metadata before the root are skipped.
        let mut late = b"<!--".to_vec();
        late.extend(std::iter::repeat_n(b'x', 10_000));
        late.extend_from_slice(b"--><!DOCTYPE svg [ <!ENTITY ns \"x\"> ]><svg/>");
        assert_eq!(detect("a.svg", &late), Kind::Svg);
        assert_eq!(detect("a.xml", &late), Kind::Svg);
        // An HTML page with an inline <svg> is a page, not an image.
        let html = b"<!DOCTYPE html>\n<html><body><svg width=\"1\"></svg></body></html>";
        assert_eq!(detect("page.html", html), Kind::Text);
        assert_eq!(detect("page.svg", html), Kind::Text);
        // An unterminated prolog is not an SVG.
        assert_eq!(detect("a.svg", b"<!-- never closed <svg/>"), Kind::Text);
    }

    #[test]
    fn svgz_is_gzip_with_the_extension() {
        assert_eq!(detect("a.svgz", &[0x1f, 0x8b, 8, 0, 0, 0]), Kind::Svg);
        assert_eq!(
            detect("a.gz", &[0x1f, 0x8b, 8, 0, 0, 0]),
            Kind::Binary("gzip archive")
        );
    }

    #[test]
    fn utf8_text_is_text() {
        assert!(looks_like_text(b""));
        assert!(looks_like_text(
            "fn main() { println!(\"héllo wörld ✓\"); }".as_bytes()
        ));
        assert!(looks_like_text("\u{feff}with a byte order mark".as_bytes()));
        assert_eq!(detect("x.rs", "日本語のテキスト".as_bytes()), Kind::Text);
    }

    #[test]
    fn a_nul_byte_makes_it_binary() {
        assert!(!looks_like_text(b"abc\0def"));
        assert!(matches!(
            detect("x.txt", b"abc\0def"),
            Kind::Binary("unknown")
        ));
        // Only the first 8 KiB are looked at.
        let mut late = vec![b'a'; TEXT_SNIFF + 10];
        late[TEXT_SNIFF + 5] = 0;
        assert!(looks_like_text(&late));
    }

    #[test]
    fn invalid_utf8_makes_it_binary_but_a_cut_character_does_not() {
        assert!(!looks_like_text(b"caf\xe9 au lait"));
        assert!(!looks_like_text(&[0xff, 0xfe, b'h', 0, b'i', 0]));
        // A 3-byte character split by the 8 KiB window.
        let mut cut = vec![b'a'; TEXT_SNIFF - 1];
        cut.extend_from_slice("€".as_bytes());
        assert!(looks_like_text(&cut));
        // The same split at the true end of a short file is still tolerated (lossy decode later).
        assert!(looks_like_text(&"€".as_bytes()[..2]));
        // But an invalid byte in the middle of the window is not.
        let mut bad = vec![b'a'; 100];
        bad[50] = 0xc3;
        bad[51] = 0x28;
        assert!(!looks_like_text(&bad));
    }

    #[test]
    fn binaries_get_a_label() {
        assert_eq!(
            detect("a.out", b"\x7fELF\x02\x01\x01\0\0\0"),
            Kind::Binary("ELF executable")
        );
        assert_eq!(
            detect("a.zip", b"PK\x03\x04\x14\0\0\0\0\0"),
            Kind::Binary("zip archive")
        );
        assert_eq!(
            detect("a.ttf", &[0, 1, 0, 0, 0, 0x0f]),
            Kind::Binary("TrueType font")
        );
        assert_eq!(
            detect("blob", &[0x80, 0x81, 0x82, 0xfe]),
            Kind::Binary("unknown")
        );
        assert_eq!(
            Kind::Binary("zip archive").label(),
            "zip archive".to_string()
        );
    }

    #[test]
    fn extension_is_the_last_dot_lowercased() {
        assert_eq!(extension("a.PNG"), "png");
        assert_eq!(extension("dir.d/file"), "");
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension("C:\\x\\y.Rs"), "rs");
    }
}

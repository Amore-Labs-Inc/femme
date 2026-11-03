//! Looks at bytes without decoding them: which image format a blob is (magic bytes), how big the
//! picture is (header fields) and the small formatting helpers the chips share. Pure, so it is
//! unit-tested; the decoding itself stays gpui's.

use gpui::ImageFormat;

fn le16(b: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([b[at], b[at + 1]]) as u32
}

fn le24(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], 0])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be16(b: &[u8], at: usize) -> u32 {
    u16::from_be_bytes([b[at], b[at + 1]]) as u32
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The image format of `bytes` judged by its magic bytes, `None` for anything gpui cannot show
/// (text, PDF, archives, ...). The weak magics (BMP, ICO, SVG) are sanity-checked so a text file that
/// happens to start with `BM` is not taken for a picture.
pub fn sniff(b: &[u8]) -> Option<ImageFormat> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if is_bmp(b) {
        Some(ImageFormat::Bmp)
    } else if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else if is_ico(b) {
        Some(ImageFormat::Ico)
    } else if is_svg(b) {
        Some(ImageFormat::Svg)
    } else {
        None
    }
}

/// `BM`, four reserved zero bytes, then a DIB header of a size the format defines.
fn is_bmp(b: &[u8]) -> bool {
    b.len() >= 26
        && b.starts_with(b"BM")
        && b[6..10] == [0; 4]
        && matches!(le32(b, 14), 12 | 40 | 52 | 56 | 64 | 108 | 124)
}

/// An icon directory: type 1, at least one entry, reserved byte of the first entry zero.
fn is_ico(b: &[u8]) -> bool {
    b.len() >= 22 && b[..4] == [0, 0, 1, 0] && (1..=255).contains(&le16(b, 4)) && b[9] == 0
}

/// Text whose first element, after the XML declaration, doctype and comments, is `<svg`. An HTML
/// page with an inline icon is not an SVG.
fn is_svg(b: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&b[..b.len().min(4096)]);
    let mut s = head.trim_start_matches('\u{feff}');
    loop {
        s = s.trim_start();
        let rest = if let Some(r) = s.strip_prefix("<?") {
            r.split_once("?>")
        } else if let Some(r) = s.strip_prefix("<!--") {
            r.split_once("-->")
        } else if let Some(r) = s.strip_prefix("<!") {
            r.split_once('>')
        } else {
            return s.strip_prefix("<svg").is_some_and(|r| {
                r.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
            });
        };
        match rest {
            Some((_, after)) => s = after,
            None => return false,
        }
    }
}

/// Pixel size of an image read from its header, without decoding. `None` for formats whose size is
/// not worth parsing here (TIFF, ICO, SVG), for cut-short bytes and for a zero dimension.
pub fn dimensions(b: &[u8], fmt: ImageFormat) -> Option<(u32, u32)> {
    let (w, h) = match fmt {
        ImageFormat::Png if b.len() >= 24 && &b[12..16] == b"IHDR" => (be32(b, 16), be32(b, 20)),
        ImageFormat::Gif if b.len() >= 10 => (le16(b, 6), le16(b, 8)),
        ImageFormat::Bmp if b.len() >= 26 => bmp_size(b),
        ImageFormat::Webp => webp_size(b)?,
        ImageFormat::Jpeg => jpeg_size(b)?,
        _ => return None,
    };
    (w > 0 && h > 0).then_some((w, h))
}

fn bmp_size(b: &[u8]) -> (u32, u32) {
    if le32(b, 14) == 12 {
        (le16(b, 18), le16(b, 20))
    } else {
        // The height is negative for a top-down bitmap.
        (
            (le32(b, 18) as i32).unsigned_abs(),
            (le32(b, 22) as i32).unsigned_abs(),
        )
    }
}

fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 30 {
        return None;
    }
    match &b[12..16] {
        // Lossy: frame tag, start code, then two 14-bit sizes.
        b"VP8 " if b[23..26] == [0x9D, 0x01, 0x2A] => {
            Some((le16(b, 26) & 0x3FFF, le16(b, 28) & 0x3FFF))
        }
        // Lossless: signature byte, then 14 bits each of width - 1 and height - 1.
        b"VP8L" if b[20] == 0x2F => {
            let bits = le32(b, 21);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        // Extended: canvas size minus one, 24 bits each.
        b"VP8X" => Some((le24(b, 24) + 1, le24(b, 27) + 1)),
        _ => None,
    }
}

/// Walks the marker segments up to the first start-of-frame, which holds the size.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        match b[i + 1] {
            0xFF => i += 1, // fill byte
            0xD8 | 0x01 | 0xD0..=0xD7 => i += 2,
            0xD9 | 0xDA => return None,
            m @ 0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => {
                return (i + 9 <= b.len()).then(|| (be16(b, i + 7), be16(b, i + 5)));
            }
            _ => {
                let len = be16(b, i + 2) as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// `1.5 KB`, `3.2 MB`: sizes on the chips.
pub fn human_size(n: usize) -> String {
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if f < 1024.0 * 1024.0 {
        format!("{:.1} KB", f / 1024.0)
    } else if f < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", f / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", f / (1024.0 * 1024.0 * 1024.0))
    }
}

/// The name a pasted image gets: `paste-3.png`.
pub fn paste_name(n: u32, fmt: ImageFormat) -> String {
    format!("paste-{n}.{}", fmt.extension())
}

/// `w` x `h` scaled down (never up) to fit a `max` px square, aspect kept.
pub fn fit_within(w: u32, h: u32, max: f32) -> (f32, f32) {
    let k = (max / w.max(h).max(1) as f32).min(1.0);
    ((w as f32 * k).max(1.0), (h as f32 * k).max(1.0))
}

/// The tile text of a non-image chip: the extension in capitals (`TXT`), `FILE` when there is none.
pub fn ext_label(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            ext.chars().take(4).collect::<String>().to_uppercase()
        }
        _ => "FILE".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0]);
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        // SOI, an APP0 segment to skip, then SOF0.
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        v.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        v.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        v
    }

    fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend(w.to_le_bytes());
        v.extend(h.to_le_bytes());
        v
    }

    fn bmp(w: i32, h: i32) -> Vec<u8> {
        let mut v = b"BM".to_vec();
        v.extend([0; 8]); // file size, reserved
        v.extend(54u32.to_le_bytes());
        v.extend(40u32.to_le_bytes());
        v.extend(w.to_le_bytes());
        v.extend(h.to_le_bytes());
        v.extend([1, 0, 24, 0]);
        v
    }

    fn webp(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF\0\0\0\0WEBP".to_vec();
        v.extend(fourcc);
        v.extend((payload.len() as u32).to_le_bytes());
        v.extend(payload);
        v.resize(v.len().max(30), 0);
        v
    }

    #[test]
    fn magic_bytes_name_the_format() {
        assert_eq!(sniff(&png(3, 2)), Some(ImageFormat::Png));
        assert_eq!(sniff(&jpeg(3, 2)), Some(ImageFormat::Jpeg));
        assert_eq!(sniff(&gif(3, 2)), Some(ImageFormat::Gif));
        assert_eq!(sniff(b"GIF87a\x01\0\x01\0"), Some(ImageFormat::Gif));
        assert_eq!(sniff(&bmp(3, 2)), Some(ImageFormat::Bmp));
        assert_eq!(sniff(&webp(b"VP8 ", &[0; 10])), Some(ImageFormat::Webp));
        assert_eq!(sniff(b"II*\0\x08\0\0\0"), Some(ImageFormat::Tiff));
        assert_eq!(sniff(b"MM\0*\0\0\0\x08"), Some(ImageFormat::Tiff));
        let mut ico = vec![0, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0];
        ico.resize(22, 0);
        assert_eq!(sniff(&ico), Some(ImageFormat::Ico));
    }

    #[test]
    fn svg_needs_an_svg_root() {
        assert_eq!(sniff(b"<svg xmlns='x'/>"), Some(ImageFormat::Svg));
        assert_eq!(sniff(b"\xEF\xBB\xBF  <svg>"), Some(ImageFormat::Svg));
        let xml = b"<?xml version='1.0'?>\n<!-- made by hand -->\n<!DOCTYPE svg>\n<svg width='1'>";
        assert_eq!(sniff(xml), Some(ImageFormat::Svg));
        // An HTML page that merely contains an icon is text.
        assert_eq!(sniff(b"<!DOCTYPE html><body><svg></svg>"), None);
        assert_eq!(sniff(b"<svgs></svgs>"), None);
        assert_eq!(sniff(b"<?xml version='1.0'?>"), None);
    }

    #[test]
    fn everything_else_is_not_an_image() {
        for not in [
            &b""[..],
            b"hello world",
            b"%PDF-1.7\n",
            b"PK\x03\x04",
            b"BM is a text file about nothing in particular",
            b"\x89PNG", // cut short, but a PNG magic needs all eight bytes
            &[
                0, 0, 1, 0, 0, 0, 16, 16, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ], // zero entries
        ] {
            assert_eq!(sniff(not), None, "{:?}", String::from_utf8_lossy(not));
        }
    }

    #[test]
    fn header_gives_the_size() {
        assert_eq!(
            dimensions(&png(640, 480), ImageFormat::Png),
            Some((640, 480))
        );
        assert_eq!(
            dimensions(&jpeg(800, 600), ImageFormat::Jpeg),
            Some((800, 600))
        );
        assert_eq!(dimensions(&gif(32, 16), ImageFormat::Gif), Some((32, 16)));
        assert_eq!(dimensions(&bmp(7, 5), ImageFormat::Bmp), Some((7, 5)));
        // Top-down bitmaps store a negative height.
        assert_eq!(dimensions(&bmp(7, -5), ImageFormat::Bmp), Some((7, 5)));

        let mut lossy = vec![0; 10];
        lossy[3..6].copy_from_slice(&[0x9D, 0x01, 0x2A]);
        lossy[6..8].copy_from_slice(&320u16.to_le_bytes());
        lossy[8..10].copy_from_slice(&(240u16 | 0x4000).to_le_bytes()); // scale bits are masked
        assert_eq!(
            dimensions(&webp(b"VP8 ", &lossy), ImageFormat::Webp),
            Some((320, 240))
        );

        let mut lossless = vec![0x2F];
        lossless.extend((99u32 | (49 << 14)).to_le_bytes()); // 100 x 50
        assert_eq!(
            dimensions(&webp(b"VP8L", &lossless), ImageFormat::Webp),
            Some((100, 50))
        );

        let mut ext = vec![0; 10];
        ext[4..7].copy_from_slice(&[0x0F, 0x01, 0x00]); // 272
        ext[7..10].copy_from_slice(&[0x63, 0x00, 0x00]); // 100
        assert_eq!(
            dimensions(&webp(b"VP8X", &ext), ImageFormat::Webp),
            Some((272, 100))
        );
    }

    #[test]
    fn header_size_refuses_what_it_cannot_read() {
        assert_eq!(dimensions(&png(0, 5), ImageFormat::Png), None);
        assert_eq!(dimensions(&png(5, 5)[..20], ImageFormat::Png), None);
        assert_eq!(dimensions(&jpeg(5, 5)[..12], ImageFormat::Jpeg), None);
        assert_eq!(dimensions(b"\xFF\xD8\xFF\xD9", ImageFormat::Jpeg), None);
        assert_eq!(dimensions(b"<svg/>", ImageFormat::Svg), None);
        assert_eq!(dimensions(b"II*\0", ImageFormat::Tiff), None);
        assert_eq!(dimensions(&webp(b"VP8 ", &[0; 4]), ImageFormat::Webp), None);
    }

    #[test]
    fn sizes_read_like_a_person_says_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(8 << 20), "8.0 MB");
        assert_eq!(human_size(3 << 30), "3.0 GB");
    }

    #[test]
    fn names_and_fitting() {
        assert_eq!(paste_name(3, ImageFormat::Png), "paste-3.png");
        assert_eq!(paste_name(1, ImageFormat::Jpeg), "paste-1.jpg");
        assert_eq!(fit_within(4000, 2000, 480.0), (480.0, 240.0));
        assert_eq!(fit_within(100, 50, 480.0), (100.0, 50.0));
        assert_eq!(fit_within(1, 4800, 480.0), (1.0, 480.0));
        assert_eq!(ext_label("notes.txt"), "TXT");
        assert_eq!(ext_label("a.tar.gz"), "GZ");
        assert_eq!(ext_label("README.markdown"), "MARK");
        assert_eq!(ext_label("Makefile"), "FILE");
        assert_eq!(ext_label(".bashrc"), "FILE");
        assert_eq!(ext_label("trailing."), "FILE");
    }
}

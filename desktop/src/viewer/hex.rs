//! The hex dump shown for files the viewer cannot render, and the size formatting its captions use.

use std::fmt::Write as _;

/// Bytes per line of the dump.
const WIDTH: usize = 16;

/// A `hexdump -C` style dump of the first `max` bytes of `data`, one string per line:
/// `00000000  7f 45 4c 46 02 01 01 00  00 00 00 00 00 00 00 00  |.ELF............|`.
pub(crate) fn hex_dump(data: &[u8], max: usize) -> Vec<String> {
    let shown = &data[..data.len().min(max)];
    shown
        .chunks(WIDTH)
        .enumerate()
        .map(|(row, chunk)| {
            let mut line = String::with_capacity(8 + 2 + WIDTH * 3 + 4 + WIDTH + 2);
            let _ = write!(line, "{:08x} ", row * WIDTH);
            for i in 0..WIDTH {
                if i.is_multiple_of(8) {
                    line.push(' ');
                }
                match chunk.get(i) {
                    Some(b) => {
                        let _ = write!(line, "{b:02x} ");
                    }
                    None => line.push_str("   "),
                }
            }
            line.push_str(" |");
            line.extend(chunk.iter().map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            }));
            line.push('|');
            line
        })
        .collect()
}

/// `1023 B`, `1.0 KB`, `412 KB`, `8.0 MB`: the size as a person reads it (powers of 1024).
pub(crate) fn human_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < 10.0 * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < 10.0 * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b < KB * KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_has_offset_hex_and_ascii() {
        let lines = hex_dump(b"\x7fELF\x02\x01\x01\x00ABCDEFGH-and-more", 256);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "00000000  7f 45 4c 46 02 01 01 00  41 42 43 44 45 46 47 48  |.ELF....ABCDEFGH|"
        );
        // The short last line is padded so the ascii column lines up.
        let pad = " ".repeat(23);
        assert_eq!(
            lines[1],
            format!("00000010  2d 61 6e 64 2d 6d 6f 72  65{pad}|-and-more|")
        );
        assert_eq!(lines[0].find('|'), lines[1].find('|'));
    }

    #[test]
    fn dump_is_capped_and_handles_empty() {
        assert!(hex_dump(&[], 256).is_empty());
        let data = vec![0xabu8; 1000];
        let lines = hex_dump(&data, 256);
        assert_eq!(lines.len(), 16);
        assert!(lines[15].starts_with("000000f0 "));
        assert_eq!(hex_dump(&data, 20).len(), 2);
    }

    #[test]
    fn sizes_read_like_a_person() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(412 * 1024), "412 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(8 * 1024 * 1024), "8.0 MB");
        assert_eq!(human_size(100 * 1024 * 1024), "100 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}

//! Byte counts as people read them: `512 B`, `1.2 KB`, `34 KB`, `8.4 MB`.

const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// `len` in bytes with 1024-based units, three significant digits at most: `0 B`, `999 B`,
/// `1.0 KB`, `12 KB`, `123 KB`, `1.5 MB`. A value that would round up to `1024` moves to the next
/// unit (`1048575` is `1.0 MB`, not `1024 KB`).
pub fn fmt_size(len: u64) -> String {
    if len < 1024 {
        return format!("{len} B");
    }
    let mut v = len as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    // One decimal below ten, none from ten up; rounding to the shown digits may reach 1024.
    if v >= 1023.5 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v < 9.95 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_exact() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1), "1 B");
        assert_eq!(fmt_size(1023), "1023 B");
    }

    #[test]
    fn larger_sizes_keep_three_digits() {
        assert_eq!(fmt_size(1024), "1.0 KB");
        assert_eq!(fmt_size(1229), "1.2 KB");
        assert_eq!(fmt_size(10 * 1024), "10 KB");
        assert_eq!(
            fmt_size(10 * 1024 - 1),
            "10 KB",
            "9.999 KB rounds up to ten"
        );
        assert_eq!(fmt_size(123 * 1024 + 300), "123 KB");
        assert_eq!(fmt_size(1536 * 1024), "1.5 MB");
        assert_eq!(fmt_size(8 << 20), "8.0 MB");
        assert_eq!(fmt_size(12 << 20), "12 MB");
        assert_eq!(fmt_size(3 << 30), "3.0 GB");
    }

    #[test]
    fn a_value_that_rounds_to_1024_moves_up_a_unit() {
        assert_eq!(fmt_size(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(fmt_size(1024 * 1024 - 400), "1.0 MB");
        assert_eq!(fmt_size(1000 * 1024), "1000 KB");
        assert_eq!(fmt_size(1023 * 1024 + 600), "1.0 MB");
    }

    #[test]
    fn the_largest_unit_does_not_overflow() {
        assert_eq!(fmt_size(u64::MAX), "16777216 TB");
    }
}

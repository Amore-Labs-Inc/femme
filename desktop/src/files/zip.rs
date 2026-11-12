//! A folder as a zip archive, for the `FEMME_FILES_FIXTURE` debug aid's answer to `FsZip` (the
//! daemon makes its own). Entries are stored, not compressed: the archive is for trying the
//! download path, and a stored zip needs nothing but a CRC.

use std::path::Path;

/// The CRC-32 (IEEE) of `data`, as zip wants it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// `dir` and everything below it as a zip, every name under `top/`; refused (`Bad:`) once it
/// would pass `cap` bytes. Symlinks are followed (like the fixture's listing); empty folders get an entry.
pub fn zip_dir(dir: &Path, top: &str, cap: u64) -> Result<Vec<u8>, String> {
    let mut files = Vec::new();
    walk(dir, top, &mut files).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut central = Vec::new();
    let count = files.len();
    for (name, data) in files {
        let crc = crc32(&data);
        let at = out.len() as u32;
        let len = data.len() as u32;
        let header = |sig: u32, central: bool| {
            let mut h = Vec::with_capacity(46 + name.len());
            h.extend(sig.to_le_bytes());
            if central {
                h.extend(20u16.to_le_bytes()); // made by
            }
            h.extend(20u16.to_le_bytes()); // needed to extract
            h.extend(0x0800u16.to_le_bytes()); // flags: names are UTF-8
            h.extend(0u16.to_le_bytes()); // stored
            h.extend([0, 0, 0x21, 0]); // 00:00, 1980-01-01
            h.extend(crc.to_le_bytes());
            h.extend(len.to_le_bytes());
            h.extend(len.to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // extra
            if central {
                h.extend(0u16.to_le_bytes()); // comment
                h.extend(0u16.to_le_bytes()); // disk
                h.extend(0u16.to_le_bytes()); // internal attributes
                let external: u32 = if name.ends_with('/') { 0x10 } else { 0 };
                h.extend(external.to_le_bytes());
                h.extend(at.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(header(0x0403_4b50, false));
        out.extend(&data);
        central.extend(header(0x0201_4b50, true));
        if out.len() as u64 + central.len() as u64 > cap {
            return Err(format!("Bad: {top} zips to more than {cap} bytes"));
        }
    }
    let (cd_at, cd_len) = (out.len() as u32, central.len() as u32);
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]); // disk numbers
    out.extend((count as u16).to_le_bytes());
    out.extend((count as u16).to_le_bytes());
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_at.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    if out.len() as u64 > cap {
        return Err(format!("Bad: {top} zips to more than {cap} bytes"));
    }
    Ok(out)
}

/// The files of `dir` (names under `prefix/`), sorted, folders as `name/` entries when empty.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    if entries.is_empty() {
        out.push((format!("{prefix}/"), Vec::new()));
    }
    for e in entries {
        let name = format!("{prefix}/{}", e.file_name().to_string_lossy());
        let path = e.path();
        if std::fs::metadata(&path)?.is_dir() {
            walk(&path, &name, out)?;
        } else {
            out.push((name, std::fs::read(&path)?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_reference() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"hello"), 0x3610_a686);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_folder_zips_with_its_files_under_its_name() {
        let dir = std::env::temp_dir().join(format!("femme-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/empty")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("sub/b.txt"), b"bb").unwrap();
        let zip = zip_dir(&dir, "top", 1 << 20).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(&zip[..4], &[0x50, 0x4b, 3, 4]);
        let end = &zip[zip.len() - 22..];
        assert_eq!(&end[..4], &[0x50, 0x4b, 5, 6]);
        assert_eq!(u16::from_le_bytes([end[10], end[11]]), 3);
        let text = String::from_utf8_lossy(&zip);
        for name in ["top/a.txt", "top/sub/b.txt", "top/sub/empty/"] {
            assert!(text.contains(name), "{name}");
        }
        assert!(zip_dir(std::path::Path::new("/nonexistent-femme"), "x", 100).is_err());
    }

    #[test]
    fn the_cap_refuses_a_big_folder() {
        let dir = std::env::temp_dir().join(format!("femme-zipcap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), vec![7u8; 1000]).unwrap();
        let e = zip_dir(&dir, "d", 500).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(e.starts_with("Bad:"), "{e}");
    }
}

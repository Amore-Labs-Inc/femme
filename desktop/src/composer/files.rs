//! Reads a file the user picked or dropped. The size is checked before a byte is read, so a
//! dropped 4 GB image is refused instead of loaded.

use std::io::Read;
use std::path::Path;

use femme_proto::rpc::MAX_BLOB_PUT;

use super::draft::Refusal;

/// The file's name and bytes, or why it cannot be attached.
pub fn load(path: &Path) -> Result<(String, Vec<u8>), Refusal> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let unreadable = |why: String| Refusal::Unreadable {
        name: name.clone(),
        why,
    };

    let meta = std::fs::metadata(path).map_err(|e| unreadable(e.to_string()))?;
    if meta.is_dir() {
        return Err(Refusal::Folder { name });
    }
    if !meta.is_file() {
        return Err(unreadable("not a regular file".into()));
    }
    if meta.len() > MAX_BLOB_PUT as u64 {
        return Err(Refusal::TooBig {
            name,
            len: meta.len(),
        });
    }
    // `take`, so a file that grew since `metadata` still cannot run past the cap.
    let mut data = Vec::with_capacity(meta.len() as usize);
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_BLOB_PUT as u64 + 1).read_to_end(&mut data))
        .map_err(|e| unreadable(e.to_string()))?;
    if data.len() > MAX_BLOB_PUT {
        return Err(Refusal::TooBig {
            name,
            len: data.len() as u64,
        });
    }
    if data.is_empty() {
        return Err(Refusal::Empty { name });
    }
    Ok((name, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory under the system temp dir, removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir =
                std::env::temp_dir().join(format!("femme-composer-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }

        /// A sparse file of `len` bytes: costs no disk.
        fn sparse(&self, name: &str, len: u64) -> PathBuf {
            let p = self.0.join(name);
            std::fs::File::create(&p).unwrap().set_len(len).unwrap();
            p
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_comes_back_with_its_name_and_bytes() {
        let t = Tmp::new("ok");
        let p = t.file("notes.txt", b"hello");
        assert_eq!(load(&p).unwrap(), ("notes.txt".into(), b"hello".to_vec()));
    }

    #[test]
    fn the_cap_is_checked_before_reading() {
        let t = Tmp::new("cap");
        let over = t.sparse("big.bin", MAX_BLOB_PUT as u64 + 1);
        assert_eq!(
            load(&over).err(),
            Some(Refusal::TooBig {
                name: "big.bin".into(),
                len: MAX_BLOB_PUT as u64 + 1
            })
        );
        let exact = t.sparse("exact.bin", MAX_BLOB_PUT as u64);
        assert_eq!(load(&exact).unwrap().1.len(), MAX_BLOB_PUT);
    }

    #[test]
    fn empty_folder_and_missing_are_refused() {
        let t = Tmp::new("refuse");
        let empty = t.file("empty", b"");
        assert_eq!(
            load(&empty).err(),
            Some(Refusal::Empty {
                name: "empty".into()
            })
        );
        assert_eq!(
            load(&t.0).err(),
            Some(Refusal::Folder {
                name: t.0.file_name().unwrap().to_string_lossy().into()
            })
        );
        match load(&t.0.join("nope")) {
            Err(Refusal::Unreadable { name, why }) => {
                assert_eq!(name, "nope");
                assert!(why.contains("No such file"), "{why}");
            }
            other => panic!("{:?}", other.err()),
        }
    }
}

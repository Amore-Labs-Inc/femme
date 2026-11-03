//! The files opened last, kept so that going back to one is instant. Plain data, no gpui.

use std::collections::VecDeque;
use std::sync::Arc;

/// How many files the panel keeps.
pub const KEEP: usize = 8;

/// The most bytes the kept files may add up to; the oldest go first.
pub const BUDGET: usize = 48 << 20;

/// The last few files read, newest last: a path (relative to the session's directory) and its
/// bytes. A file bigger than the budget alone is not kept.
#[derive(Debug)]
pub struct FileCache {
    keep: usize,
    budget: usize,
    used: usize,
    items: VecDeque<(String, Arc<Vec<u8>>)>,
}

impl Default for FileCache {
    fn default() -> Self {
        Self::new(KEEP, BUDGET)
    }
}

impl FileCache {
    /// A cache of at most `keep` files and `budget` bytes.
    pub fn new(keep: usize, budget: usize) -> Self {
        Self {
            keep,
            budget,
            used: 0,
            items: VecDeque::new(),
        }
    }

    /// The bytes of `path`, which becomes the newest entry.
    pub fn get(&mut self, path: &str) -> Option<Arc<Vec<u8>>> {
        let at = self.items.iter().position(|(p, _)| p == path)?;
        let item = self.items.remove(at)?;
        let data = item.1.clone();
        self.items.push_back(item);
        Some(data)
    }

    /// Keeps `data` as the bytes of `path` (replacing an older copy), newest, and lets the oldest
    /// go while there are too many files or too many bytes.
    pub fn put(&mut self, path: &str, data: Arc<Vec<u8>>) {
        self.forget(path);
        if data.len() > self.budget || self.keep == 0 {
            return;
        }
        self.used += data.len();
        self.items.push_back((path.to_owned(), data));
        while self.items.len() > self.keep || self.used > self.budget {
            if let Some((_, old)) = self.items.pop_front() {
                self.used -= old.len();
            }
        }
    }

    /// Drops `path`.
    pub fn forget(&mut self, path: &str) {
        if let Some(at) = self.items.iter().position(|(p, _)| p == path) {
            if let Some((_, old)) = self.items.remove(at) {
                self.used -= old.len();
            }
        }
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.items.clear();
        self.used = 0;
    }

    /// How many files are kept.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The paths kept, oldest first.
    pub fn paths(&self) -> Vec<&str> {
        self.items.iter().map(|(p, _)| p.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(n: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![7; n])
    }

    #[test]
    fn the_last_eight_files_are_kept() {
        let mut c = FileCache::default();
        for i in 0..10 {
            c.put(&format!("f{i}"), bytes(10));
        }
        assert_eq!(c.len(), KEEP);
        assert_eq!(c.paths().first(), Some(&"f2"));
        assert!(c.get("f0").is_none() && c.get("f1").is_none());
        assert_eq!(c.get("f9").map(|d| d.len()), Some(10));
    }

    #[test]
    fn a_read_makes_the_file_the_newest() {
        let mut c = FileCache::new(3, 1 << 20);
        for p in ["a", "b", "c"] {
            c.put(p, bytes(1));
        }
        assert!(c.get("a").is_some());
        c.put("d", bytes(1));
        // `b` was the oldest after `a` was read.
        assert_eq!(c.paths(), ["c", "a", "d"]);
    }

    #[test]
    fn putting_a_path_again_replaces_it() {
        let mut c = FileCache::new(3, 100);
        c.put("a", bytes(60));
        c.put("a", bytes(30));
        assert_eq!(c.len(), 1);
        assert_eq!(c.get("a").map(|d| d.len()), Some(30));
        // The byte count followed: another 60 fits next to it.
        c.put("b", bytes(60));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn too_many_bytes_push_the_oldest_out() {
        let mut c = FileCache::new(8, 100);
        c.put("a", bytes(40));
        c.put("b", bytes(40));
        c.put("c", bytes(40));
        assert_eq!(c.paths(), ["b", "c"]);
    }

    #[test]
    fn a_file_over_the_budget_is_not_kept_and_a_stale_copy_goes() {
        let mut c = FileCache::new(8, 100);
        c.put("big", bytes(50));
        c.put("big", bytes(101));
        assert!(c.is_empty());
        c.put("a", bytes(10));
        c.put("huge", bytes(1000));
        assert_eq!(c.paths(), ["a"]);
    }

    #[test]
    fn clear_and_forget() {
        let mut c = FileCache::default();
        c.put("a", bytes(3));
        c.put("b", bytes(3));
        c.forget("a");
        assert_eq!(c.paths(), ["b"]);
        c.clear();
        assert!(c.is_empty() && c.get("b").is_none());
        // The byte count is back to zero: a budget-sized file fits.
        let mut c = FileCache::new(2, 10);
        c.put("a", bytes(10));
        c.clear();
        c.put("b", bytes(10));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_cache_that_keeps_nothing_keeps_nothing() {
        let mut c = FileCache::new(0, 100);
        c.put("a", bytes(1));
        assert!(c.is_empty());
    }
}

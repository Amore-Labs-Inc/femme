//! What a click on a file does before any request goes out: show it from the cache, wait for the
//! read that is on its way, refuse it, ask first, or read it. Plain data, no gpui.

use super::source::MAX_READ;

/// Files over this many bytes ask before they are read.
pub const BIG: u64 = 8 << 20;

/// The first step of opening a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The bytes are in the cache: show them.
    Show,
    /// The same file is being read already: nothing to do.
    Wait,
    /// More than the daemon reads at once: say so, do not offer.
    Refuse,
    /// Big: wait for a yes.
    Ask,
    /// Send `FsRead`.
    Read,
}

/// What to do for a file of `len` bytes (as listed). `cached`: its bytes are held. `reading`:
/// a read of it is out. `confirmed`: the yes to a big file was given.
pub fn plan(len: u64, cached: bool, reading: bool, confirmed: bool) -> Plan {
    if cached {
        Plan::Show
    } else if reading {
        Plan::Wait
    } else if len > MAX_READ {
        Plan::Refuse
    } else if len > BIG && !confirmed {
        Plan::Ask
    } else {
        Plan::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_files_are_read_at_once() {
        assert_eq!(plan(0, false, false, false), Plan::Read);
        assert_eq!(
            plan(BIG, false, false, false),
            Plan::Read,
            "8 MiB is not over 8 MiB"
        );
    }

    #[test]
    fn a_file_over_8_mib_asks_first_and_reads_after_the_yes() {
        assert_eq!(plan(BIG + 1, false, false, false), Plan::Ask);
        assert_eq!(plan(MAX_READ, false, false, false), Plan::Ask);
        assert_eq!(plan(BIG + 1, false, false, true), Plan::Read);
        assert_eq!(plan(MAX_READ, false, false, true), Plan::Read);
    }

    #[test]
    fn a_file_the_daemon_would_refuse_is_never_offered() {
        assert_eq!(plan(MAX_READ + 1, false, false, false), Plan::Refuse);
        assert_eq!(plan(MAX_READ + 1, false, false, true), Plan::Refuse);
        assert_eq!(plan(u64::MAX, false, false, true), Plan::Refuse);
    }

    #[test]
    fn the_cache_comes_first_then_a_read_already_out() {
        // Whatever the size: it was read before, the bytes are here.
        assert_eq!(plan(MAX_READ + 1, true, false, false), Plan::Show);
        assert_eq!(plan(BIG + 1, true, true, false), Plan::Show);
        assert_eq!(plan(BIG + 1, false, true, false), Plan::Wait);
        assert_eq!(plan(1, false, true, true), Plan::Wait);
    }
}

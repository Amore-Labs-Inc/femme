//! A program that starts an OSC and never ends it must not make the emulator hold everything it
//! sends: the parser keeps every payload byte of the OSC it is in. Its own file because it reads
//! the process's resident memory, which other tests running in the same process would disturb.
#![cfg(all(feature = "emulator", target_os = "linux"))]

use femme_term::Emulator;

fn rss_mib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .expect("VmRSS in /proc/self/status");
    kib / 1024
}

/// Feed `head`, then 64 MiB of payload in the holder's read size, and return how much memory grew.
fn flood(head: &[u8]) -> (Emulator, u64) {
    let mut e = Emulator::new(80, 24, 100);
    let before = rss_mib();
    e.feed(head);
    let chunk = vec![b'y'; 8192];
    for _ in 0..(64 * 1024 * 1024 / chunk.len()) {
        e.feed(&chunk);
    }
    let grown = rss_mib().saturating_sub(before);
    (e, grown)
}

#[test]
fn an_unterminated_osc_does_not_grow_memory() {
    let (mut e, grown) = flood(b"\x1b]0;");
    assert!(
        grown < 8,
        "memory grew by {grown} MiB after 64 MiB of one OSC"
    );
    // and the OSC still ends where the program ends it
    e.feed(b"\x07hi");
    assert_eq!(e.screen_text().lines().next(), Some("hi"));
    assert!(e.title().is_some_and(|t| t.len() <= 1024));

    // the parser starts an OSC after control bytes in its escape state too
    let (_, grown) = flood(b"\x1b\x01]0;");
    assert!(grown < 8, "behind a control byte: grew by {grown} MiB");
}

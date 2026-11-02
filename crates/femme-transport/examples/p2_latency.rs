//! P2: does keystroke latency stay low while a bulk transfer shares the connection?
//!
//! One process, the daemon and the desktop in separate 2-thread runtimes, over loopback:
//!
//! - **A** QUIC, idle: a keystroke every 50 ms on a `Term` stream; the server echoes a 300-byte
//!   "frame". Round trip recorded per keystroke.
//! - **B** QUIC with the server pushing bulk data on a `Bulk` stream (prio -10) all the while. A
//!   50 MB transfer takes about 0.1 s on loopback, so the push repeats until the run ends.
//! - **C** the same two runs over the SSH mux on an in-memory pipe (`C-idle`, `C-bulk`). The pipe is
//!   instant and unlimited unless `--mux-delay-ms` / `--mux-mbps` make it behave like `ssh` over a
//!   real link (latency, a bandwidth cap, and 1 MiB of buffering in the middle).
//!
//! ```text
//! cargo run --release -p femme-transport --example p2_latency -- [--keys 2000] [--interval-ms 50]
//!     [--phases A,B,C] [--rtt-ms 80] [--mux-delay-ms 40 --mux-mbps 5]
//!     [--emu 40,5,1] [--dup-gap-ms 10]
//! ```
//!
//! For the P2 target (keystroke p99 < RTT+15 ms idle, < RTT+30 ms during bulk, at 80 ms RTT ±10 ms
//! and 1% loss) run it under `scripts/netem.sh` (needs root) or, without root, give the QUIC phases
//! a userspace netem with `--emu DELAY_MS,JITTER_MS,LOSS_PCT`: a UDP relay on its own thread that
//! delays every datagram, each way, by DELAY +- JITTER ms (uniform per datagram, so it reorders as
//! netem does) and drops LOSS% of them. Pass `--rtt-ms 80` and the verdict lines compare against
//! the target. Progress goes to stderr every 250 keystrokes, results to stdout.
//!
//! `--dup-gap-ms N` makes the QUIC phases also send each keystroke as a datagram, twice, N ms
//! apart ([`Conn::send_datagram_dup`]); the server answers a datagram keystroke with the same frame
//! as a datagram, twice. The `Term` stream is unchanged and still carries everything, so the
//! datagrams only race it: a keystroke is timed to whichever echo arrives first. The mux phases
//! never use datagrams (TCP retransmits on its own).
//!
//! Measured on the 4 vCPU build VPS with `--phases A,B --keys 1000 --emu 40,5,1 --rtt-ms 80
//! --dup-gap-ms 10` (the relay is one thread, so the bulk phase moves only 1.7 MB/s), in ms:
//!
//! ```text
//! phase          p50    p95     p99   p99.9     max   P2 target
//! A quic idle  81.15  88.51   91.33   94.91  102.46   p99 < 95
//! B quic+bulk  84.48  93.25  101.89  126.97  173.31   p99 < 110
//! ```
//!
//! Without `--dup-gap-ms` the same link gave p99 219 ms idle and 177 ms with bulk (the audit's run
//! of this harness before the change): 1% loss each way hits about 2% of the round trips and the
//! `Term` stream then waits out a loss-recovery round. Copies sent back to back (gap 0) are lost
//! together and gave 211 ms idle; 10 ms apart they are not.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use femme_proto::term::TermUp;
use femme_proto::{Id, PaneId};
use femme_transport::quic::Allow;
use femme_transport::{
    listen, mux_client, mux_server, prio, Conn, DeviceIdentity, QuicClient, Rx, ServerIdentity,
    StreamKind, Tx,
};
use hdrhistogram::Histogram;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio::time::sleep_until;

const WARMUP: usize = 20;

struct Args {
    keys: usize,
    interval: Duration,
    phases: String,
    rtt_ms: Option<f64>,
    mux_delay: Duration,
    mux_mbps: Option<f64>,
    /// `--emu`: one-way delay ms, jitter ms and loss % of the UDP relay in front of the daemon.
    emu: Option<(f64, f64, f64)>,
    /// `--dup-gap-ms`: send keystrokes and their echoes as datagram pairs this far apart.
    dup_gap: Option<Duration>,
}

fn args() -> Args {
    let mut a = Args {
        keys: 2000,
        interval: Duration::from_millis(50),
        phases: "A,B,C".into(),
        rtt_ms: None,
        mux_delay: Duration::ZERO,
        mux_mbps: None,
        emu: None,
        dup_gap: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--keys" => {
                a.keys = value()
                    .parse()
                    .unwrap_or_else(|_| usage("--keys: not a number"))
            }
            "--interval-ms" => {
                a.interval = Duration::from_millis(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--interval-ms: not a number")),
                )
            }
            "--phases" => a.phases = value().to_uppercase(),
            "--rtt-ms" => {
                a.rtt_ms = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--rtt-ms: not a number")),
                )
            }
            "--mux-delay-ms" => {
                a.mux_delay = Duration::from_millis(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--mux-delay-ms: not a number")),
                )
            }
            "--mux-mbps" => {
                a.mux_mbps = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--mux-mbps: not a number")),
                )
            }
            "--emu" => {
                let v: Vec<f64> = value()
                    .split(',')
                    .map(|x| {
                        x.parse()
                            .unwrap_or_else(|_| usage("--emu: want DELAY_MS,JITTER_MS,LOSS_PCT"))
                    })
                    .collect();
                let [delay, jitter, loss] = v[..] else {
                    usage("--emu: want DELAY_MS,JITTER_MS,LOSS_PCT")
                };
                a.emu = Some((delay, jitter, loss));
            }
            "--dup-gap-ms" => {
                a.dup_gap = Some(Duration::from_millis(
                    value()
                        .parse()
                        .unwrap_or_else(|_| usage("--dup-gap-ms: not a number")),
                ))
            }
            _ => usage(&format!("unknown flag {flag}")),
        }
    }
    a
}

fn usage(why: &str) -> ! {
    eprintln!("{why}\nusage: p2_latency [--keys N] [--interval-ms MS] [--phases A,B,C] [--rtt-ms MS] [--mux-delay-ms MS] [--mux-mbps MB/s] [--emu DELAY_MS,JITTER_MS,LOSS_PCT] [--dup-gap-ms MS]");
    std::process::exit(2);
}

fn fill_random(buf: &mut [u8], state: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
}

// ---------------------------------------------------------------------------------------------
// the daemon side

/// What `femmed` does on a connection: answer every keystroke on a `Term` stream with a 300-byte
/// frame, and push bulk blocks on a `Bulk` stream until the client hangs up. With `dup_gap`, also
/// answer keystroke datagrams ([`echo_datagrams`]).
async fn serve(conn: Arc<dyn Conn>, dup_gap: Option<Duration>) {
    if let Some(gap) = dup_gap {
        tokio::spawn(echo_datagrams(conn.clone(), gap));
    }
    while let Ok((kind, tx, rx)) = conn.accept().await {
        match kind {
            StreamKind::Term(_) => {
                tokio::spawn(echo(tx, rx));
            }
            StreamKind::Bulk(_) => {
                tokio::spawn(push_bulk(tx, rx));
            }
            _ => {}
        }
    }
}

async fn echo(mut tx: Tx, mut rx: Rx) {
    let mut frame = vec![0u8; 300];
    let mut state = 0x1234_5678_9abc_def1;
    fill_random(&mut frame, &mut state);
    while let Ok(Some(msg)) = rx.recv_msg::<TermUp>().await {
        if let TermUp::Bytes { seq, .. } = msg {
            frame[..4].copy_from_slice(&seq.to_le_bytes());
            if tx.send_bytes(&frame).await.is_err() {
                break;
            }
        }
    }
}

/// The datagram twin of [`echo`]: a keystroke datagram is a sequence number (4 bytes, LE) and the
/// key; the answer is the same 300-byte frame as a datagram pair. The client sends each keystroke
/// twice, so the second copy of a sequence number is ignored.
async fn echo_datagrams(conn: Arc<dyn Conn>, gap: Duration) {
    let mut frame = vec![0u8; 300];
    let mut state = 0x1234_5678_9abc_def1;
    fill_random(&mut frame, &mut state);
    let mut answered = HashSet::new();
    while let Ok(d) = conn.recv_datagram().await {
        let Some(seq) = d
            .get(..4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        else {
            continue;
        };
        if answered.insert(seq) {
            frame[..4].copy_from_slice(&seq.to_le_bytes());
            conn.clone()
                .send_datagram_dup(Bytes::copy_from_slice(&frame), gap);
        }
    }
}

async fn push_bulk(mut tx: Tx, _rx: Rx) {
    let (mut block, mut state) = (vec![0u8; 64 << 10], 0x9e37_79b9_7f4a_7c15);
    loop {
        // Fresh random bytes every time, so compression cannot make the transfer free.
        fill_random(&mut block, &mut state);
        if tx.send_bytes(&block).await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// transports

/// What the emulated link does to one datagram: `None` if it is lost, else how long it takes.
fn fate(rng: &Mutex<u64>, (delay, jitter, loss): (f64, f64, f64)) -> Option<Duration> {
    let mut state = rng.lock().unwrap();
    let mut uniform = || {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 11) as f64 / (1u64 << 53) as f64
    };
    if uniform() * 100.0 < loss {
        return None;
    }
    Some(Duration::from_secs_f64(
        (delay + (uniform() * 2.0 - 1.0) * jitter).max(0.0) / 1000.0,
    ))
}

/// A userspace netem for when `scripts/netem.sh` is not an option: a UDP relay on `rt` that sits in
/// front of `server` and returns the address to connect to instead. Every datagram, each way, goes
/// through [`fate`]. ponytail: one thread, so it tops out near 1.5 MB/s and the bulk phase is a
/// trickle; a real link has more, but the keystroke path does not depend on it.
fn emulator(rt: &Runtime, server: SocketAddr, link: (f64, f64, f64)) -> SocketAddr {
    rt.block_on(async move {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let back = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        back.connect(server).await.unwrap();
        let addr = front.local_addr().unwrap();
        let client = Arc::new(Mutex::new(None::<SocketAddr>));
        let rng = Arc::new(Mutex::new(0x2545_f491_4f6c_dd1du64));
        {
            let (front, back, client, rng) =
                (front.clone(), back.clone(), client.clone(), rng.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                while let Ok((n, from)) = front.recv_from(&mut buf).await {
                    *client.lock().unwrap() = Some(from);
                    if let Some(delay) = fate(&rng, link) {
                        let (back, datagram) = (back.clone(), buf[..n].to_vec());
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = back.send(&datagram).await;
                        });
                    }
                }
            });
        }
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = back.recv(&mut buf).await {
                let (Some(delay), Some(to)) = (fate(&rng, link), *client.lock().unwrap()) else {
                    continue;
                };
                let (front, datagram) = (front.clone(), buf[..n].to_vec());
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = front.send_to(&datagram, to).await;
                });
            }
        });
        addr
    })
}

fn quic_pair(
    server_rt: &Runtime,
    client_rt: &Runtime,
    emu_rt: &Runtime,
    a: &Args,
) -> Arc<dyn Conn> {
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("p2");
    let spki = device.spki_sha256().unwrap();
    let allow: Allow = Arc::new(move |h| (*h == spki).then(Id::new));
    let fp = server.fingerprint();
    let listener = server_rt
        .block_on(listen("127.0.0.1:0".parse().unwrap(), &server, allow))
        .unwrap();
    let mut bound = listener.local_addr().unwrap();
    if let Some(link) = a.emu {
        bound = emulator(emu_rt, bound, link);
    }
    let dup_gap = a.dup_gap;
    server_rt.spawn(async move {
        while let Ok((_, conn)) = listener.accept().await {
            tokio::spawn(serve(conn, dup_gap));
        }
    });
    client_rt.block_on(async move {
        QuicClient::connect(bound, fp, &device, None).await.unwrap() as Arc<dyn Conn>
    })
}

/// Copy `from` into `to` the way a slow link would: each chunk takes `len / bytes_per_sec` to get
/// through the bottleneck and then `delay` to arrive, and at most `buffer` bytes wait in the middle.
async fn shape<R: AsyncRead + Unpin, W: AsyncWrite + Unpin + Send + 'static>(
    mut from: R,
    mut to: W,
    delay: Duration,
    bytes_per_sec: f64,
    buffer: usize,
) {
    const CHUNK: usize = 16 << 10;
    let (queue, mut arrivals) =
        mpsc::channel::<(tokio::time::Instant, Bytes)>((buffer / CHUNK).max(1));
    tokio::spawn(async move {
        while let Some((at, bytes)) = arrivals.recv().await {
            sleep_until(at).await;
            if to.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let (mut buf, mut link_free) = (vec![0u8; CHUNK], tokio::time::Instant::now());
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 {
            break;
        }
        link_free = link_free.max(tokio::time::Instant::now())
            + Duration::from_secs_f64(n as f64 / bytes_per_sec);
        if queue
            .send((link_free + delay, Bytes::copy_from_slice(&buf[..n])))
            .await
            .is_err()
        {
            break;
        }
    }
}

fn mux_pair(server_rt: &Runtime, client_rt: &Runtime, a: &Args) -> Arc<dyn Conn> {
    let (client_end, server_end) = if a.mux_delay.is_zero() && a.mux_mbps.is_none() {
        // A Linux pipe holds 64 KiB.
        tokio::io::duplex(64 << 10)
    } else {
        let (c_end, c_relay) = tokio::io::duplex(64 << 10);
        let (s_end, s_relay) = tokio::io::duplex(64 << 10);
        let (c_in, c_out) = tokio::io::split(c_relay);
        let (s_in, s_out) = tokio::io::split(s_relay);
        let rate = a.mux_mbps.map_or(f64::INFINITY, |m| m * 1e6);
        // ponytail: 1 MiB in the middle; ssh's channel window alone is 2 MiB, TCP adds more.
        let (delay, buffer) = (a.mux_delay, 1 << 20);
        server_rt.spawn(shape(c_in, s_out, delay, rate, buffer));
        server_rt.spawn(shape(s_in, c_out, delay, rate, buffer));
        (c_end, s_end)
    };
    {
        let _in_server = server_rt.enter();
        let (r, w) = tokio::io::split(server_end);
        tokio::spawn(serve(mux_server(r, w), None));
    }
    let _in_client = client_rt.enter();
    let (r, w) = tokio::io::split(client_end);
    mux_client(r, w)
}

// ---------------------------------------------------------------------------------------------
// the measurement

struct Report {
    name: String,
    hist: Histogram<u64>,
    bulk_bytes: Option<u64>,
    secs: f64,
}

/// Type `a.keys` keystrokes at `a.interval` on a fresh `Term` stream and time each echo. With
/// `bulk`, a `Bulk` stream is open the whole time and the server keeps it full. With `dup`, each
/// keystroke also goes out as a datagram pair that far apart and the server answers the same way;
/// a keystroke is then timed to whichever echo, stream or datagram, arrives first.
async fn run_phase(
    name: &str,
    conn: Arc<dyn Conn>,
    bulk: bool,
    dup: Option<Duration>,
    a: &Args,
) -> Report {
    let n = a.keys + WARMUP;
    let (mut tx, mut rx) = conn
        .open_kind(StreamKind::Term(PaneId {
            sid: Id::new(),
            no: 0,
        }))
        .await
        .unwrap();
    let bulk_bytes = Arc::new(AtomicU64::new(0));
    if bulk {
        let (_up, mut down) = conn.open(StreamKind::Bulk(1), prio::BULK).await.unwrap();
        let count = bulk_bytes.clone();
        tokio::spawn(async move {
            while let Ok(Some(b)) = down.recv().await {
                count.fetch_add(b.len() as u64, Ordering::Relaxed);
            }
        });
        // Let the bulk stream ramp up before the clock starts.
        while bulk_bytes.load(Ordering::Relaxed) < 1 << 20 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    let sent: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());
    // When each echo first arrived, whichever way it came (0 = not yet).
    let arrived: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());
    let t0 = Instant::now();
    if dup.is_some() {
        let (conn, arrived) = (conn.clone(), arrived.clone());
        tokio::spawn(async move {
            while let Ok(d) = conn.recv_datagram().await {
                let seq = u32::from_le_bytes(d[..4].try_into().unwrap()) as usize;
                let _ = arrived[seq].compare_exchange(
                    0,
                    t0.elapsed().as_nanos() as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        });
    }
    let name_owned = name.to_string();
    let reader = {
        let (sent, arrived) = (sent.clone(), arrived.clone());
        tokio::spawn(async move {
            let mut hist = Histogram::<u64>::new_with_bounds(1, 600_000_000, 3).unwrap();
            for got in 1..=n {
                let frame = rx
                    .recv()
                    .await
                    .expect("receive")
                    .expect("the server ended the stream");
                let seq = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
                let _ = arrived[seq].compare_exchange(
                    0,
                    t0.elapsed().as_nanos() as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                let micros = arrived[seq]
                    .load(Ordering::Relaxed)
                    .saturating_sub(sent[seq].load(Ordering::Relaxed))
                    / 1000;
                if seq >= WARMUP {
                    hist.record(micros.max(1)).unwrap();
                }
                if got % 250 == 0 {
                    eprintln!(
                        "[{name_owned}] {got}/{n} keystrokes answered, p99 so far {:.2} ms",
                        hist.value_at_quantile(0.99) as f64 / 1000.0
                    );
                }
            }
            hist
        })
    };
    let bulk_start = bulk_bytes.load(Ordering::Relaxed);
    let start = tokio::time::Instant::now();
    for seq in 0..n {
        sleep_until(start + a.interval * seq as u32).await;
        sent[seq].store(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        tx.send(&TermUp::Bytes {
            seq: seq as u32,
            data: vec![b'k'; 12],
        })
        .await
        .unwrap();
        if let Some(gap) = dup {
            conn.clone().send_datagram_dup(
                Bytes::from([&(seq as u32).to_le_bytes()[..], &[b'k'; 12]].concat()),
                gap,
            );
        }
    }
    let hist = tokio::time::timeout(Duration::from_secs(60), reader)
        .await
        .expect("replies stopped coming")
        .unwrap();
    let secs = t0.elapsed().as_secs_f64();
    let bulk_bytes = bulk.then(|| bulk_bytes.load(Ordering::Relaxed) - bulk_start);
    conn.close();
    Report {
        name: name.to_string(),
        hist,
        bulk_bytes,
        secs,
    }
}

fn ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

fn main() {
    let a = args();
    let server_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("daemon")
        .enable_all()
        .build()
        .unwrap();
    let client_rt = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("desktop")
        .enable_all()
        .build()
        .unwrap();
    let emu_rt = Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("emu")
        .enable_all()
        .build()
        .unwrap();
    let wants = |p: &str| a.phases.split(',').any(|x| x.trim() == p);
    let mut reports: Vec<Report> = Vec::new();
    let mut run = |name: &str, conn: Arc<dyn Conn>, bulk: bool, dup: Option<Duration>| {
        eprintln!("== {name}: {} keystrokes at {:?}", a.keys, a.interval);
        let r = client_rt.block_on(run_phase(name, conn, bulk, dup, &a));
        eprintln!("== {name} done in {:.0} s", r.secs);
        reports.push(r);
    };
    if wants("A") {
        run(
            "A quic idle",
            quic_pair(&server_rt, &client_rt, &emu_rt, &a),
            false,
            a.dup_gap,
        );
    }
    if wants("B") {
        run(
            "B quic+bulk",
            quic_pair(&server_rt, &client_rt, &emu_rt, &a),
            true,
            a.dup_gap,
        );
    }
    if wants("C") {
        run(
            "C mux idle",
            mux_pair(&server_rt, &client_rt, &a),
            false,
            None,
        );
        run(
            "C mux+bulk",
            mux_pair(&server_rt, &client_rt, &a),
            true,
            None,
        );
    }

    println!(
        "\nkeystroke -> 300 B echo round trip, ms (n = {} per phase, {:?} apart)",
        a.keys, a.interval
    );
    println!(
        "{:<14}{:>9}{:>9}{:>9}{:>9}{:>9}{:>9}{:>12}{:>9}",
        "phase", "p50", "p95", "p99", "p99.9", "max", "mean", "bulk MB", "MB/s"
    );
    for r in &reports {
        let h = &r.hist;
        let (bulk, rate) = match r.bulk_bytes {
            Some(b) => (
                format!("{:.0}", b as f64 / 1e6),
                format!("{:.1}", b as f64 / 1e6 / r.secs),
            ),
            None => ("-".into(), "-".into()),
        };
        println!(
            "{:<14}{:>9.2}{:>9.2}{:>9.2}{:>9.2}{:>9.2}{:>9.2}{:>12}{:>9}",
            r.name,
            ms(h.value_at_quantile(0.50)),
            ms(h.value_at_quantile(0.95)),
            ms(h.value_at_quantile(0.99)),
            ms(h.value_at_quantile(0.999)),
            ms(h.max()),
            h.mean() / 1000.0,
            bulk,
            rate
        );
    }
    println!();
    let p99 = |name: &str| {
        reports
            .iter()
            .find(|r| r.name == name)
            .map(|r| ms(r.hist.value_at_quantile(0.99)))
    };
    for (idle, busy) in [("A quic idle", "B quic+bulk"), ("C mux idle", "C mux+bulk")] {
        if let (Some(i), Some(b)) = (p99(idle), p99(busy)) {
            let verdict = if b <= 2.0 * i { "within" } else { "OVER" };
            println!(
                "{busy}: p99 {b:.2} ms vs {idle} {i:.2} ms = {:.2}x ({verdict} the 2x target)",
                b / i
            );
        }
    }
    if let Some(rtt) = a.rtt_ms {
        for (name, slack) in [("A quic idle", 15.0), ("B quic+bulk", 30.0)] {
            if let Some(p) = p99(name) {
                let verdict = if p < rtt + slack { "meets" } else { "MISSES" };
                println!(
                    "{name}: p99 {p:.2} ms {verdict} the P2 target RTT({rtt}) + {slack} = {:.0} ms",
                    rtt + slack
                );
            }
        }
    }
}

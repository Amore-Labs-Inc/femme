//! A real agent process wired to a driver, for `#[ignore]` live tests. Threads and std::process
//! only: the library itself stays sans-IO. Every stdout line goes through `Driver::on_line` and
//! whatever the driver writes goes to the process's stdin, exactly like the session actor will.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use femme_agents::{AgentEvent, Driver, Error, Launch, Out};

/// A running agent process plus its driver.
pub struct Live {
    pub child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Option<Vec<u8>>>,
    pub driver: Box<dyn Driver>,
    /// Every event so far, in order.
    pub events: Vec<AgentEvent>,
    /// Every stdout line + `\n` (what the holder's spool file would hold).
    pub spool: Vec<u8>,
    /// Everything written to the process's stdin.
    pub stdin_log: Vec<u8>,
    pub stderr: std::sync::Arc<std::sync::Mutex<String>>,
    offset: u64,
    exited: bool,
}

impl Live {
    /// Spawn per `driver.spawn(l)`, then run `driver.on_start`.
    pub fn start(driver: Box<dyn Driver>, l: &Launch) -> Live {
        let sp = driver.spawn(l);
        let mut cmd = Command::new(&sp.cmd[0]);
        cmd.args(&sp.cmd[1..])
            .current_dir(&sp.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &sp.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn agent");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut buf = Vec::new();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) => {
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                        }
                        if tx.send(Some(buf)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let se = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(se).lines().map_while(Result::ok) {
                let mut s = sink.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
        });
        let mut live = Live {
            child,
            stdin,
            rx,
            driver,
            events: vec![],
            spool: vec![],
            stdin_log: vec![],
            stderr,
            offset: 0,
            exited: false,
        };
        let mut out = Out::default();
        live.driver.on_start(l, &mut out);
        live.flush(out);
        live
    }

    fn flush(&mut self, mut out: Out) {
        let b = out.take();
        if b.is_empty() {
            return;
        }
        self.stdin_log.extend_from_slice(&b);
        if let Some(s) = self.stdin.as_mut() {
            let _ = s.write_all(&b).and_then(|_| s.flush());
        }
    }

    /// Call a driver method and write whatever it produced.
    pub fn call(
        &mut self,
        f: impl FnOnce(&mut dyn Driver, &mut Out) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut out = Out::default();
        let r = f(self.driver.as_mut(), &mut out);
        self.flush(out);
        r
    }

    /// Read lines for up to `timeout`, feeding the driver, until `until` returns true for a new
    /// event (returns true) or the process exits / time runs out (false).
    pub fn pump(&mut self, timeout: Duration, mut until: impl FnMut(&AgentEvent) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(Some(line)) => {
                    let mut out = Out::default();
                    let evs = self.driver.on_line(self.offset, &line, &mut out);
                    self.spool.extend_from_slice(&line);
                    self.spool.push(b'\n');
                    self.offset += line.len() as u64 + 1;
                    self.flush(out);
                    let mut hit = false;
                    for e in evs {
                        hit |= until(&e);
                        self.events.push(e);
                    }
                    if hit {
                        return true;
                    }
                }
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    if !self.exited {
                        self.exited = true;
                        let code = self.child.wait().ok().and_then(|s| s.code());
                        let evs = self.driver.on_exit(code);
                        self.events.extend(evs);
                    }
                    return false;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Pump until an event matches, panicking with the tail of the events and stderr on timeout.
    pub fn expect(
        &mut self,
        what: &str,
        timeout: Duration,
        mut pred: impl FnMut(&AgentEvent) -> bool,
    ) {
        if !self.pump(timeout, |e| pred(e)) {
            let tail: Vec<_> = self.events.iter().rev().take(12).collect();
            panic!(
                "timed out waiting for {what}\nlast events: {tail:#?}\nstderr: {}",
                self.stderr.lock().unwrap()
            );
        }
    }

    /// Close stdin, wait up to `grace` for exit, else kill.
    pub fn finish(&mut self, grace: Duration) {
        self.stdin = None;
        let _ = self.pump(grace, |_| false);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

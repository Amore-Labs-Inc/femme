//! One request at a time over one connection: write a line, read a line. The connection is kept
//! between calls and dropped on any transport error, so the next call reconnects.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub enum Error {
    /// The socket is gone, refused, timed out, or spoke garbage.
    Io(String),
    /// herdr answered with an error object.
    Api { code: String, message: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Api { code, message } => write!(f, "{message} ({code})"),
        }
    }
}

impl Error {
    pub fn code(&self) -> &str {
        match self {
            Error::Io(_) => "io",
            Error::Api { code, .. } => code,
        }
    }
}

pub struct Client {
    path: PathBuf,
    conn: Option<BufReader<UnixStream>>,
    next: u64,
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

impl Client {
    pub fn new(path: impl Into<PathBuf>) -> Client {
        Client { path: path.into(), conn: None, next: 0 }
    }

    fn connect(&mut self) -> Result<&mut BufReader<UnixStream>, Error> {
        if self.conn.is_none() {
            let s = UnixStream::connect(&self.path).map_err(|e| Error::Io(format!("{}: {e}", self.path.display())))?;
            let _ = s.set_write_timeout(Some(DEFAULT_TIMEOUT));
            self.conn = Some(BufReader::new(s));
        }
        Ok(self.conn.as_mut().expect("just connected"))
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        self.call_timeout(method, params, DEFAULT_TIMEOUT)
    }

    /// `agent.start` waits for the agent to come up, so it gets a longer timeout than the rest.
    pub fn call_timeout(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        self.next += 1;
        let id = format!("femme_{}", self.next);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        let reply = match self.connect() {
            Ok(conn) => exchange(conn, &line, timeout),
            Err(e) => return Err(e),
        };
        let text = match reply {
            Ok(t) => t,
            Err(e) => {
                self.conn = None;
                return Err(Error::Io(format!("herdr: {e}")));
            }
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::Io(format!("herdr sent no JSON: {e}")))?;
        if let Some(err) = v.get("error") {
            return Err(Error::Api {
                code: err["code"].as_str().unwrap_or("error").to_string(),
                message: err["message"].as_str().unwrap_or("").to_string(),
            });
        }
        Ok(v["result"].clone())
    }
}

fn exchange(conn: &mut BufReader<UnixStream>, line: &str, timeout: Duration) -> std::io::Result<String> {
    conn.get_mut().set_read_timeout(Some(timeout))?;
    conn.get_mut().write_all(line.as_bytes())?;
    conn.get_mut().write_all(b"\n")?;
    let mut out = String::new();
    if conn.read_line(&mut out)? == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "herdr closed the connection"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A one-shot fake herdr: answers each line with `reply(request)`.
    fn fake(name: &str, reply: impl Fn(&Value) -> String + Send + 'static) -> PathBuf {
        let path = std::env::temp_dir().join(format!("femme-tui-test-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let mut w = &stream;
                    w.write_all(reply(&req).as_bytes()).unwrap();
                    w.write_all(b"\n").unwrap();
                    line.clear();
                }
            }
        });
        path
    }

    #[test]
    fn result_and_error_round_trip() {
        let path = fake("ok", |req| {
            if req["method"] == "ping" {
                json!({"id": req["id"], "result": {"type": "pong", "version": "0.9.5"}}).to_string()
            } else {
                json!({"id": req["id"], "error": {"code": "not_found", "message": "pane not found"}}).to_string()
            }
        });
        let mut c = Client::new(&path);
        assert_eq!(c.call("ping", json!({})).unwrap()["version"], "0.9.5");
        match c.call("pane.read", json!({"pane_id": "w9:p9"})) {
            Err(Error::Api { code, message }) => {
                assert_eq!(code, "not_found");
                assert_eq!(message, "pane not found");
            }
            other => panic!("expected an api error, got {other:?}"),
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_socket_is_an_io_error() {
        let mut c = Client::new("/nonexistent/herdr.sock");
        assert!(matches!(c.call("ping", json!({})), Err(Error::Io(_))));
    }
}

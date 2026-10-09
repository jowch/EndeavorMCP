//! R's adapter (`runtime/r/adapter.R`), which runs Ember in its own R
//! process. It answers `POST /adapter` as Julia's does. httpuv can't stream a
//! response, so its notifications come by long-polling `GET
//! /notifications?after=<seq>`, which `notifications` turns back into the
//! stream of `data:` lines the core follows.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Value, json};

use super::Upstream;
use crate::http::{self, Head};

/// How long the adapter holds a poll with nothing to say, and how much longer we wait for it.
const POLL: Duration = Duration::from_secs(25 + 15);

pub struct R {
    port: u16,
    token: String,
}

impl R {
    pub fn new(port: u16, token: String) -> R {
        R { port, token }
    }
}

impl Upstream for R {
    fn adapter(&self, raw: &[u8]) -> io::Result<String> {
        let authorization = format!("Bearer {}", self.token);
        let headers = [("Authorization", authorization.as_str()), ("Content-Type", "application/json")];
        let (status, body) = http::post(self.port, "/adapter", &headers, raw)?;
        if status != 200 {
            return Err(io::Error::other(format!("R's /adapter answered {status}")));
        }
        String::from_utf8(body).map_err(|_| io::ErrorKind::InvalidData.into())
    }

    /// From now on: the core reads every notebook again when a stream begins.
    fn notifications(&self) -> io::Result<Box<dyn BufRead + Send>> {
        let reply: Value = serde_json::from_str(&self.adapter(json!({ "method": "status", "params": {} }).to_string().as_bytes())?)?;
        let after = reply["result"]["seq"].as_u64().ok_or_else(|| io::Error::other(format!("R's status has no seq: {reply}")))?;
        Ok(Box::new(BufReader::new(Polls { port: self.port, token: self.token.clone(), after, lines: Vec::new(), at: 0 })))
    }
}

/// The notifications as `data:` lines, one poll after another; the end when a poll fails.
struct Polls {
    port: u16,
    token: String,
    after: u64,
    lines: Vec<u8>,
    at: usize,
}

impl Polls {
    /// The notifications after `after`, once there are some or the adapter gives up waiting.
    fn poll(&self) -> io::Result<Vec<Value>> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(POLL))?;
        write!(stream, "GET /notifications?after={} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n", self.after, self.port, self.token)?;
        let mut reader = BufReader::new(stream);
        let head = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if head.status() != 200 {
            return Err(io::Error::other(format!("R's /notifications answered {}", head.status())));
        }
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        serde_json::from_slice(&body).map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

impl Read for Polls {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.lines.len() {
            let Ok(notes) = self.poll() else { return Ok(0) };
            self.lines.clear();
            self.at = 0;
            for note in notes {
                self.after = self.after.max(note["seq"].as_u64().unwrap_or(0));
                self.lines.extend_from_slice(format!("data: {note}\n").as_bytes());
            }
        }
        let n = buf.len().min(self.lines.len() - self.at);
        buf[..n].copy_from_slice(&self.lines[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    use std::sync::{Arc, Mutex};

    /// A stand-in adapter: `status` says seq 4, then each poll gets the next of `polls`, then the
    /// connection closes unanswered. The polls' targets.
    fn adapter(polls: Vec<&'static str>) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let targets = Arc::new(Mutex::new(Vec::new()));
        let seen = targets.clone();
        std::thread::spawn(move || {
            let mut polls = polls.into_iter();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let head = Head::read(&mut reader).unwrap().unwrap();
                assert_eq!(head.header("Authorization"), Some("Bearer t"));
                let body = if head.method() == "POST" {
                    let _ = http::read_body(&mut reader, head.request_body().unwrap());
                    r#"{"result":{"seq":4}}"#
                } else {
                    seen.lock().unwrap().push(head.target().to_owned());
                    let Some(next) = polls.next() else { continue };
                    next
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (port, targets)
    }

    #[test]
    fn polls_become_the_stream_the_core_reads_from_the_latest_seq_on() {
        let (port, targets) = adapter(vec![r#"[{"method":"cell_state","params":{},"seq":5},{"method":"execution_done","params":{},"seq":6}]"#, "[]", r#"[{"method":"file_saved","params":{},"seq":7}]"#]);
        let stream = R::new(port, "t".into()).notifications().unwrap();
        let lines: Vec<String> = stream.lines().map(Result::unwrap).collect();
        assert_eq!(lines, [r#"data: {"method":"cell_state","params":{},"seq":5}"#, r#"data: {"method":"execution_done","params":{},"seq":6}"#, r#"data: {"method":"file_saved","params":{},"seq":7}"#]);
        let after: Vec<String> = targets.lock().unwrap().clone();
        assert_eq!(after, ["/notifications?after=4", "/notifications?after=6", "/notifications?after=6", "/notifications?after=7"], "the stream ends when a poll fails");
    }
}

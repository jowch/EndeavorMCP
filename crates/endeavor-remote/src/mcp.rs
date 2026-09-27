//! The agent's MCP connection, MCP over SSE: `GET /sse` opens a session's
//! stream, and each `POST /message?sessionId=…` carries one JSON-RPC message,
//! whose reply goes out on that stream. Each agent session's messages carry
//! `X-Endeavor-Session` (its key) and, on a server, `X-Endeavor-Host`. What the
//! core doesn't answer itself goes to Julia's `/dispatch` with those headers.

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::http::{self, Head};

const KEEPALIVE: Duration = Duration::from_secs(15);

/// Replies waiting for a session's stream; a full queue holds up the next POST.
const QUEUE: usize = 64;

/// What every client connection shares.
pub struct Bridge {
    /// Julia's bridge port, once it answers.
    pub upstream: OnceLock<u16>,
    pub token: String,
    sessions: Mutex<HashMap<String, SyncSender<String>>>,
}

/// Who sent a message: the agent session's key and the server it works on,
/// both empty for the app.
#[derive(Default)]
pub struct Caller {
    pub owner: String,
    pub host: String,
}

impl Caller {
    fn of(request: &Head) -> Caller {
        let header = |name| request.header(name).unwrap_or_default().to_owned();
        Caller { owner: header("X-Endeavor-Session"), host: header("X-Endeavor-Host") }
    }
}

impl Bridge {
    pub fn new(token: String) -> Bridge {
        Bridge { upstream: OnceLock::new(), token, sessions: Mutex::default() }
    }

    /// Serve `GET /sse`: a new session, and its stream until the client goes.
    pub fn stream(&self, request: &Head, mut client: TcpStream) -> io::Result<()> {
        let id = session_id()?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        self.sessions.lock().unwrap().insert(id.clone(), tx);
        let chunked = request.keeps_alive();
        let result = client
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{}\r\n",
                    if chunked { "Transfer-Encoding: chunked\r\n" } else { "" }
                )
                .as_bytes(),
            )
            .and_then(|_| send_events(&mut client, chunked, &id, &rx, KEEPALIVE));
        self.sessions.lock().unwrap().remove(&id);
        result
    }

    /// Serve `POST /message?sessionId=…`: the reply goes out on the session's
    /// stream before this answers 202. Whether the connection can carry another request.
    pub fn post(&self, request: &Head, reader: &mut BufReader<TcpStream>, client: &mut TcpStream) -> io::Result<bool> {
        let body = http::read_body(reader, request.request_body()?)?;
        let keep_alive = request.keeps_alive();
        let id = query_param(request.target(), "sessionId").unwrap_or_default();
        let Some(session) = self.sessions.lock().unwrap().get(id).cloned() else {
            http::respond(client, "404 Not Found", None, br#"{"error":"Session not found"}"#, keep_alive)?;
            return Ok(keep_alive);
        };
        let Some(message) = serde_json::from_slice::<Value>(&body).ok().filter(Value::is_object) else {
            http::respond(client, "400 Bad Request", None, br#"{"error":"Invalid JSON"}"#, keep_alive)?;
            return Ok(keep_alive);
        };
        match self.dispatch(&message, &body, &Caller::of(request)) {
            Ok(reply) => {
                // A session whose stream has gone drops the reply.
                if let Some(reply) = reply {
                    let _ = session.send(reply);
                }
                http::respond(client, "202 Accepted", None, b"", keep_alive)?;
                Ok(keep_alive)
            }
            Err(_) => {
                let body = json!({ "error": "Julia's bridge isn't answering" }).to_string();
                http::respond(client, "502 Bad Gateway", Some("application/json"), body.as_bytes(), false)?;
                Ok(false)
            }
        }
    }

    /// The reply to one JSON-RPC message (`raw` is its text), if it gets one.
    fn dispatch(&self, message: &Value, raw: &[u8], caller: &Caller) -> io::Result<Option<String>> {
        // Notifications get no reply, and Julia had nothing to do for them.
        let id = match message.get("id") {
            None | Some(Value::Null) => return Ok(None),
            Some(id) => id,
        };
        match message["method"].as_str().unwrap_or_default() {
            "ping" => Ok(Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })))),
            _ => self.ask_julia(raw, caller).map(Some),
        }
    }

    /// Julia's reply to a message, from its `/dispatch`.
    fn ask_julia(&self, raw: &[u8], caller: &Caller) -> io::Result<String> {
        let port = *self.upstream.get().ok_or(io::ErrorKind::NotConnected)?;
        let authorization = format!("Bearer {}", self.token);
        let headers = [
            ("Authorization", authorization.as_str()),
            ("Content-Type", "application/json"),
            ("X-Endeavor-Session", caller.owner.as_str()),
            ("X-Endeavor-Host", caller.host.as_str()),
        ];
        let headers: Vec<_> = headers.into_iter().filter(|(_, value)| !value.is_empty()).collect();
        let (status, body) = http::post(port, "/dispatch", &headers, raw)?;
        if status != 200 {
            return Err(io::Error::other(format!("Julia's /dispatch answered {status}")));
        }
        String::from_utf8(body).map_err(|_| io::ErrorKind::InvalidData.into())
    }
}

/// Write a session's events: where to post, then each reply as it comes, with
/// a comment line after `keepalive` of quiet so proxies keep the stream open.
/// Returns when a write fails: the client has gone.
fn send_events(out: &mut impl Write, chunked: bool, id: &str, replies: &Receiver<String>, keepalive: Duration) -> io::Result<()> {
    let mut event = |text: String| -> io::Result<()> {
        if chunked {
            write!(out, "{:x}\r\n{text}\r\n", text.len())?;
        } else {
            out.write_all(text.as_bytes())?;
        }
        out.flush()
    };
    event(format!("event: endpoint\ndata: /message?sessionId={id}\n\n"))?;
    loop {
        match replies.recv_timeout(keepalive) {
            Ok(reply) => event(format!("event: message\ndata: {reply}\n\n"))?,
            Err(RecvTimeoutError::Timeout) => event(": keepalive\n\n".into())?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// A random (version 4) UUID.
fn session_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
}

/// `name`'s value in a target's query, as given (not percent-decoded).
fn query_param<'a>(target: &'a str, name: &str) -> Option<&'a str> {
    let (_, query) = target.split_once('?')?;
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name).map(|(_, value)| value)
}

/// JSON the way Julia's JSON.jl writes it, byte for byte: object keys sorted
/// by their bytes, and DEL escaped. serde_json's own key order depends on a
/// feature another crate in the workspace turns on.
pub fn to_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value);
    out
}

fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_json(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::String(text) => write_string(out, text),
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push_str(&Value::from(text).to_string().replace('\x7f', "\\u007f"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_as_julia_does() {
        let value = json!({ "b": 1, "A": [true, null, 2.5], "a": { "z": "x\u{7f}\u{1}/\"é", "_": {} }, "aa": [] });
        assert_eq!(to_json(&value), r#"{"A":[true,null,2.5],"a":{"_":{},"z":"x\u007f\u0001/\"é"},"aa":[],"b":1}"#);
    }

    #[test]
    fn finds_query_params() {
        assert_eq!(query_param("/message?sessionId=abc&x=1", "sessionId"), Some("abc"));
        assert_eq!(query_param("/message?x=1&sessionId=", "sessionId"), Some(""));
        assert_eq!(query_param("/message", "sessionId"), None);
    }

    #[test]
    fn session_ids_are_random_uuids() {
        let (a, b) = (session_id().unwrap(), session_id().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!((&a[14..15], a.matches('-').count()), ("4", 4));
    }

    /// A writer the test can read back while `send_events` still holds it.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn streams_the_endpoint_replies_and_keepalives() {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let out = Shared::default();
        let mut writer = out.clone();
        let stream = std::thread::spawn(move || send_events(&mut writer, true, "s1", &rx, Duration::from_millis(100)));
        tx.send(r#"{"id":1}"#.into()).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        drop(tx);
        stream.join().unwrap().unwrap();
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let endpoint = "event: endpoint\ndata: /message?sessionId=s1\n\n";
        let reply = "event: message\ndata: {\"id\":1}\n\n";
        let expected = format!("{:x}\r\n{endpoint}\r\n{:x}\r\n{reply}\r\n", endpoint.len(), reply.len());
        assert!(text.starts_with(&expected), "{text:?}");
        let keepalives = text[expected.len()..].matches("d\r\n: keepalive\n\n\r\n").count();
        assert!(keepalives >= 1, "{text:?}");
    }

    #[test]
    fn a_failed_write_ends_the_stream() {
        struct Gone;
        impl Write for Gone {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_tx, rx) = mpsc::sync_channel::<String>(1);
        assert!(send_events(&mut Gone, false, "s1", &rx, Duration::from_millis(10)).is_err());
    }
}

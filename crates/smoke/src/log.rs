//! The proxy's log: one JSON object per line, `{"pid": ..., "t": Unix seconds, "dir": "in"|"out", "msg": ...}`,
//! "in" from the agent and "out" from the server. A line that isn't JSON is kept as a string.
//! `pid` is the proxy's: an agent that restarts its server starts a second proxy on the
//! same log, whose request ids start over.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub struct Log {
    file: File,
}

impl Log {
    pub fn create(path: &Path) -> Log {
        let file = File::options().create(true).append(true).open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Log { file }
    }

    pub fn write(&mut self, dir: &str, line: &str) {
        let msg = serde_json::from_str(line).unwrap_or_else(|_| json!(line));
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let entry = json!({ "pid": std::process::id(), "t": t, "dir": dir, "msg": msg });
        let _ = writeln!(self.file, "{entry}");
    }
}

/// A tool call the agent made, with its reply.
#[derive(Debug, Clone)]
pub struct Call {
    pub tool: String,
    pub args: Value,
    /// The reply's text, parsed as JSON where it is JSON.
    pub reply: Value,
    pub is_error: bool,
    /// When the request was sent, in Unix seconds.
    pub at: f64,
}

/// The tool calls in a log, in the order they were made. A call with no reply
/// (the agent gave up on it) has a null reply.
pub fn calls(path: &Path) -> Vec<Call> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    // Keyed by (proxy pid, request id).
    let mut calls: Vec<((Value, Value), Call)> = Vec::new();
    for entry in BufReader::new(file).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str::<Value>(&l).ok()) {
        let msg = &entry["msg"];
        match entry["dir"].as_str() {
            Some("in") if msg["method"] == "tools/call" => calls.push((
                (entry["pid"].clone(), msg["id"].clone()),
                Call {
                    tool: msg["params"]["name"].as_str().unwrap_or_default().to_owned(),
                    args: msg["params"]["arguments"].clone(),
                    reply: Value::Null,
                    is_error: false,
                    at: entry["t"].as_f64().unwrap_or(0.0),
                },
            )),
            Some("out") if !msg["id"].is_null() => {
                let key = (entry["pid"].clone(), msg["id"].clone());
                if let Some((_, call)) = calls.iter_mut().rev().find(|(k, c)| *k == key && c.reply.is_null()) {
                    call.is_error = msg["result"]["isError"] == true || !msg["error"].is_null();
                    let text = msg["result"]["content"][0]["text"].as_str().or(msg["error"]["message"].as_str()).unwrap_or_default();
                    call.reply = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
                }
            }
            _ => {}
        }
    }
    calls.into_iter().map(|(_, c)| c).collect()
}

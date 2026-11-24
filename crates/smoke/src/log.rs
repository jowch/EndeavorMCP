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

#[derive(Debug, Clone)]
pub struct Call {
    pub tool: String,
    pub args: Value,
    pub reply: Value,
    pub is_error: bool,
    pub reply_bytes: usize,
    pub at: f64,
}

pub fn calls(path: &Path) -> Vec<Call> {
    let Ok(file) = File::open(path) else { return Vec::new() };
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
                    reply_bytes: 0,
                    at: entry["t"].as_f64().unwrap_or(0.0),
                },
            )),
            Some("out") if !msg["id"].is_null() => {
                let key = (entry["pid"].clone(), msg["id"].clone());
                if let Some((_, call)) = calls.iter_mut().rev().find(|(k, c)| *k == key && c.reply.is_null()) {
                    call.is_error = msg["result"]["isError"] == true || !msg["error"].is_null();
                    let text = msg["result"]["content"][0]["text"].as_str().or(msg["error"]["message"].as_str()).unwrap_or_default();
                    call.reply_bytes = text.len();
                    call.reply = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
                }
            }
            _ => {}
        }
    }
    calls.into_iter().map(|(_, c)| c).collect()
}

pub fn injections(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    BufReader::new(file).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str::<Value>(&l).ok()).filter(|e| e["dir"] == "inject").map(|e| e["msg"].clone()).collect()
}

pub fn turns(path: &Path) -> Vec<f64> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    BufReader::new(file).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str::<Value>(&l).ok()).filter(|e| e["dir"] == "user").filter_map(|e| e["t"].as_f64()).collect()
}

pub fn questions(path: &Path) -> Vec<String> {
    let Ok(file) = File::open(path) else { return Vec::new() };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|e| e["dir"] == "out" && e["msg"]["method"] == "elicitation/create")
        .map(|e| e["msg"]["params"]["message"].as_str().unwrap_or_default().to_owned())
        .collect()
}

//! An `endeavor mcp` as a harness runs it: JSON-RPC lines on its stdin and stdout.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

pub struct Front {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    replies: Receiver<String>,
    stderr: Arc<Mutex<Vec<String>>>,
    id: u64,
}

impl Front {
    /// Run `command` (an `endeavor mcp …`, with its environment as the test wants it) with its stdio piped.
    pub fn spawn(mut command: Command) -> Front {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let (tx, replies) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let (log, err) = (stderr.clone(), child.stderr.take().unwrap());
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                log.lock().unwrap().push(line);
            }
        });
        let stdin = child.stdin.take();
        Front { child: Some(child), stdin, replies, stderr, id: 0 }
    }

    pub fn send(&mut self, message: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{message}").unwrap();
    }

    /// A request and its reply, whatever else arrives meanwhile.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = self.replies.recv_timeout(Duration::from_secs(180)).unwrap_or_else(|e| panic!("no reply to {method}: {e}; stderr: {:?}", self.stderr.lock().unwrap()));
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == json!(id) {
                return reply;
            }
        }
    }

    pub fn initialize(&mut self) -> Value {
        let reply = self.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "Test Agent", "version": "0" } }));
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        reply
    }

    /// A tool call: whether it failed, and the text of its first content as JSON (else as a string).
    pub fn call(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let (failed, content) = self.contents(name, arguments);
        (failed, content[0].clone())
    }

    /// A tool call: whether it failed, and each content's text as JSON (else as a string).
    pub fn contents(&mut self, name: &str, arguments: Value) -> (bool, Vec<Value>) {
        let reply = self.request("tools/call", json!({ "name": name, "arguments": arguments }));
        let content = reply["result"]["content"].as_array().unwrap_or_else(|| panic!("{name}: {reply}"));
        let texts = content.iter().map(|c| c["text"].as_str().map_or(Value::Null, |t| serde_json::from_str(t).unwrap_or_else(|_| Value::String(t.to_owned())))).collect();
        (reply["result"]["isError"] == true, texts)
    }

    pub fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let (failed, result) = self.call(name, arguments.clone());
        assert!(!failed, "{name}({arguments}): {result}\nstderr: {:?}", self.stderr.lock().unwrap());
        result
    }

    /// Close its input, as a harness does when the session ends, and wait for it to exit.
    pub fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.take().unwrap().wait().unwrap();
        assert!(status.success(), "{status}");
    }

    pub fn said(&self) -> Vec<String> {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}


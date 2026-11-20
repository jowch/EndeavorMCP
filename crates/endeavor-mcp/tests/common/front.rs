//! An `endeavor mcp` as a harness runs it: JSON-RPC lines on its stdin and stdout.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// How the client answers a dialog the front shows (MCP elicitation): "accept", "decline" or
    /// "cancel", or "error" for a client that says it can show one and answers with an error (#381).
    /// None: the client can't show one.
    pub dialog: Option<&'static str>,
    /// The dialogs the front asked for, in order.
    pub asked: Vec<Value>,
    /// What the user clicks on an install page the front opens in the browser, where the client
    /// can't show a dialog: "install" (the default, as a user who agreed) or "decline"; None clicks nothing.
    pub page: Arc<Mutex<Option<&'static str>>>,
    /// The install pages the front opened, in order.
    pub pages: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
}

/// The file of install pages of a front that no test gave one, so that they can be clicked.
fn browser_file() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("endeavor-test-browser-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)))
}

/// Click `answer` on the install page at `url`, as the user's browser does.
pub fn click(url: &str, answer: &str) -> (u16, String) {
    let rest = url.strip_prefix("http://127.0.0.1:").unwrap_or_else(|| panic!("not an install page: {url}"));
    let (port, path) = rest.split_once('/').unwrap();
    let body = format!("answer={answer}");
    let mut socket = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(socket, "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    let _ = socket.read_to_string(&mut reply);
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0), body.to_owned())
}

impl Front {
    /// Run `command` (an `endeavor mcp …`, with its environment as the test wants it) with its stdio piped.
    pub fn spawn(mut command: Command) -> Front {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // Where the front "opens" its install pages, unless the test chose the file.
        let browser = match command.get_envs().find(|(name, _)| *name == "ENDEAVOR_TEST_INSTALL_PAGES").and_then(|(_, value)| value) {
            Some(file) => PathBuf::from(file),
            None => {
                let file = browser_file();
                let _ = std::fs::remove_file(&file);
                command.env("ENDEAVOR_TEST_INSTALL_PAGES", &file);
                file
            }
        };
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
        let (page, pages, done) = (Arc::new(Mutex::new(Some("install"))), Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
        // The user, at the browser: each install page the front opens is clicked as `page` says.
        {
            let (page, pages, done) = (page.clone(), pages.clone(), done.clone());
            std::thread::spawn(move || {
                let mut seen = 0;
                while !done.load(Ordering::SeqCst) {
                    let opened = std::fs::read_to_string(&browser).unwrap_or_default();
                    let lines: Vec<&str> = opened.lines().collect();
                    for url in lines.iter().skip(seen) {
                        pages.lock().unwrap().push(url.to_string());
                        if let Some(answer) = *page.lock().unwrap() {
                            click(url, answer);
                        }
                    }
                    seen = lines.len();
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        }
        Front { child: Some(child), stdin, replies, stderr, id: 0, dialog: None, asked: Vec::new(), page, pages, done }
    }

    /// Set what the user clicks on the install pages from now on.
    pub fn click_pages(&self, answer: Option<&'static str>) {
        *self.page.lock().unwrap() = answer;
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
            if reply["method"] == "elicitation/create" {
                self.asked.push(reply.clone());
                let action = self.dialog.expect("a dialog from a front told the client can't show one");
                if action == "error" {
                    self.send(json!({ "jsonrpc": "2.0", "id": reply["id"], "error": { "code": -32601, "message": "Method not found" } }));
                } else {
                    self.send(json!({ "jsonrpc": "2.0", "id": reply["id"], "result": { "action": action, "content": {} } }));
                }
                continue;
            }
            if reply["id"] == json!(id) {
                return reply;
            }
        }
    }

    pub fn initialize(&mut self) -> Value {
        let capabilities = if self.dialog.is_some() { json!({ "elicitation": { "form": {} } }) } else { json!({}) };
        let reply = self.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": capabilities, "clientInfo": { "name": "Test Agent", "version": "0" } }));
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
        self.done.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}


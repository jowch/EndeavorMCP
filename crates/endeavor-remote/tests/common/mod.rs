//! A stand-in for Julia under `endeavor-remote core`: a script that writes the
//! state boot.jl would and then sleeps, naming a bridge and a Pluto served by
//! this test process. The bridge answers like Julia's (chunked responses to
//! HTTP/1.1, close-delimited to HTTP/1.0, the adapter's calls on `/adapter` and
//! its notifications on `/notifications`); Pluto wants its secret in a cookie,
//! echoes bodies on `/echo`, streams events on `/stream` and echoes a
//! WebSocket's bytes after an upgrade. Both record what they were sent.
//! `helper` drives `endeavor-remote connect` as the app does.

#![allow(dead_code)]

pub mod helper;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
/// The secret the fake Pluto requires.
pub const PLUTO_SECRET: &str = "s3cret";

/// For the end-to-end tests, the julia to test with, and the app's folder when it's the app's.
pub fn find_julia() -> Option<(PathBuf, Option<PathBuf>)> {
    if let Some(julia) = std::env::var_os("ENDEAVOR_E2E_JULIA") {
        return Some((julia.into(), None));
    }
    let app = PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/endeavor");
    let mut installed: Vec<PathBuf> = std::fs::read_dir(&app)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("julia-")))
        .map(|p| p.join("bin/julia"))
        .filter(|p| p.is_file())
        .collect();
    installed.sort();
    if let Some(julia) = installed.pop() {
        return Some((julia, Some(app)));
    }
    let found = std::process::Command::new("/bin/sh").args(["-lc", "command -v julia"]).output().ok()?;
    let path = String::from_utf8(found.stdout).ok()?.lines().last()?.trim().to_owned();
    path.starts_with('/').then(|| (PathBuf::from(path), None))
}

/// A julia that says it's 1.12, records its arguments, writes its state for the
/// core naming `bridge`'s ports, and sleeps as its own pid.
pub fn serving_julia(dir: &Path, bridge: &FakeBridge) -> PathBuf {
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let julia = bin.join("julia");
    let script = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && {{ echo 'julia version 1.12.0'; exit 0; }}
echo "$@" > "{dir}/julia.args"
echo "booting"
printf '{{"launcher":"%s","node":"%s","pid":%s,"pluto_port":{pluto},"mcp_port":{mcp},"token":"%s","pluto_secret":"{PLUTO_SECRET}","job":""}}' "$ENDEAVOR_LAUNCHER" "$(hostname)" $$ "$ENDEAVOR_TOKEN" > "$ENDEAVOR_STATE.tmp"
mv "$ENDEAVOR_STATE.tmp" "$ENDEAVOR_STATE"
exec sleep 600
"#,
        dir = dir.display(),
        pluto = bridge.pluto_port,
        mcp = bridge.port,
    );
    std::fs::write(&julia, script).unwrap();
    std::fs::set_permissions(&julia, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    julia
}

pub fn state_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-remote-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A request as the bridge received it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Shared {
    seen: Mutex<Vec<Seen>>,
    /// What Pluto was sent.
    pluto_seen: Mutex<Vec<Seen>>,
    /// Said when Pluto's end of a WebSocket closes.
    socket_closed: Mutex<Option<Sender<()>>>,
    /// The open `/stream` stream: events to write, or `None` to end it abruptly.
    events: Mutex<Option<Sender<Option<String>>>>,
    /// Said when the core closes the `/stream` stream's upstream connection.
    closed: Mutex<Option<Sender<()>>>,
    /// The open `/notifications` streams.
    notifications: Mutex<Vec<Sender<String>>>,
    /// What `snapshot` reports: each open notebook.
    notebooks: Mutex<Vec<serde_json::Value>>,
}

pub struct FakeBridge {
    pub port: u16,
    pub pluto_port: u16,
    shared: Arc<Shared>,
    closed: Receiver<()>,
    socket_closed: Receiver<()>,
    /// The state folder, where the core keeps Julia's pid (`endeavor/shutdown` ends it).
    dir: PathBuf,
}

impl FakeBridge {
    pub fn start(dir: &Path) -> FakeBridge {
        let (closed_tx, closed) = mpsc::channel();
        let (socket_closed_tx, socket_closed) = mpsc::channel();
        let shared = Arc::new(Shared { closed: Mutex::new(Some(closed_tx)), socket_closed: Mutex::new(Some(socket_closed_tx)), ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (s, d) = (shared.clone(), dir.to_path_buf());
        std::thread::spawn(move || {
            for socket in listener.incoming().map_while(Result::ok) {
                let (s, d) = (s.clone(), d.clone());
                std::thread::spawn(move || serve(socket, &s, &d));
            }
        });
        let pluto = TcpListener::bind("127.0.0.1:0").unwrap();
        let pluto_port = pluto.local_addr().unwrap().port();
        let s = shared.clone();
        std::thread::spawn(move || {
            for socket in pluto.incoming().map_while(Result::ok) {
                let s = s.clone();
                std::thread::spawn(move || pluto_serve(socket, &s));
            }
        });
        FakeBridge { port, pluto_port, shared, closed, socket_closed, dir: dir.to_path_buf() }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    pub fn pluto_seen(&self) -> Vec<Seen> {
        self.shared.pluto_seen.lock().unwrap().clone()
    }

    /// Wait for Pluto's end of a WebSocket to close.
    pub fn socket_closed(&self) -> bool {
        self.socket_closed.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    /// Write an event on the open `/stream` stream, waiting for one to open.
    pub fn event(&self, text: &str) {
        wait_for("a /stream stream", || self.shared.events.lock().unwrap().is_some());
        self.shared.events.lock().unwrap().as_ref().unwrap().send(Some(text.to_owned())).unwrap();
    }

    /// End the open `/stream` stream without its final chunk, like a crash.
    pub fn drop_events(&self) {
        self.shared.events.lock().unwrap().take().unwrap().send(None).unwrap();
    }

    /// Send a notification to the core, waiting for it to listen.
    pub fn notify(&self, message: serde_json::Value) {
        wait_for("a /notifications stream", || !self.shared.notifications.lock().unwrap().is_empty());
        self.shared.notifications.lock().unwrap().retain(|n| n.send(message.to_string()).is_ok());
    }

    /// The notebooks the adapter reports from now on.
    pub fn set_notebooks(&self, notebooks: Vec<serde_json::Value>) {
        *self.shared.notebooks.lock().unwrap() = notebooks;
    }

    /// Wait for the core to close the `/stream` stream's connection.
    pub fn events_closed(&self) -> bool {
        self.closed.recv_timeout(Duration::from_secs(5)).is_ok()
    }
}

fn serve(socket: TcpStream, shared: &Arc<Shared>, dir: &Path) {
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut socket = socket;
    while let Some(seen) = read_request(&mut reader) {
        shared.seen.lock().unwrap().push(seen.clone());
        let http10 = seen.line.ends_with("HTTP/1.0");
        let target = seen.line.split(' ').nth(1).unwrap_or_default().to_owned();
        let authorized = seen.header("Authorization") == Some(&format!("Bearer {TOKEN}"));
        if target == "/health" {
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        } else if !authorized {
            let _ = socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        } else if target == "/notifications" {
            return notifications(socket, shared);
        } else if target == "/adapter" {
            let call: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
            let notebooks = shared.notebooks.lock().unwrap().clone();
            let id = &call["params"]["notebook_id"];
            let found = notebooks.iter().find(|nb| nb["notebook_id"] == *id).cloned();
            let reply = match (call["method"].as_str().unwrap(), found) {
                ("snapshot", _) if id.is_null() => serde_json::json!({ "result": { "notebooks": notebooks } }),
                ("snapshot", Some(nb)) => serde_json::json!({ "result": nb }),
                ("graph", Some(_)) => serde_json::json!({ "result": { "cells": [] } }),
                _ => serde_json::json!({ "error": format!("KeyError: key \"notebook_not_found::No notebook with id '{}' in the current session\" not found", id.as_str().unwrap_or_default()) }),
            }
            .to_string();
            let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}", reply.len());
        } else if target == "/call" {
            let body = String::from_utf8_lossy(&seen.body).into_owned();
            let reply = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": { "said": seen.line, "body": body } }).to_string();
            if http10 {
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{reply}");
            } else {
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{reply}\r\n0\r\n\r\n", reply.len());
            }
            if body.contains("endeavor/shutdown") {
                let pid = read_json(&dir.join("julia.json"))["pid"].as_i64().unwrap() as i32;
                // SAFETY: plain syscall.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        } else {
            let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        }
        if http10 {
            return;
        }
    }
}

/// Pluto, which wants its secret in a cookie and sets it again on its reply,
/// as Pluto does.
fn pluto_serve(socket: TcpStream, shared: &Arc<Shared>) {
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut socket = socket;
    while let Some(seen) = read_request(&mut reader) {
        shared.pluto_seen.lock().unwrap().push(seen.clone());
        let target = seen.line.split(' ').nth(1).unwrap_or_default().to_owned();
        if seen.header("Cookie") != Some(&format!("secret={PLUTO_SECRET}")) {
            let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\n\r\nForbidden");
        } else if seen.header("Upgrade") == Some("websocket") {
            let _ = socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
            let _ = std::io::copy(&mut reader, &mut socket);
            let _ = shared.socket_closed.lock().unwrap().as_ref().unwrap().send(());
            return;
        } else if target == "/stream" {
            return events(socket, shared);
        } else if target == "/echo" {
            let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", seen.body.len());
            let _ = socket.write_all(&seen.body);
        } else {
            let body = format!("Pluto: {}", seen.line);
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nSet-Cookie: secret={PLUTO_SECRET}; SameSite=Strict; HttpOnly\r\nSet-Cookie: theme=dark\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
        if seen.line.ends_with("HTTP/1.0") {
            return;
        }
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Seen> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(": ").unwrap();
        headers.push((name.to_owned(), value.to_owned()));
    }
    let mut seen = Seen { line: line.trim_end().to_owned(), headers, body: Vec::new() };
    if seen.header("Transfer-Encoding") == Some("chunked") {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim_end(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk[..if size == 0 { 2 } else { size + 2 }]).ok()?;
            if size == 0 {
                break;
            }
            seen.body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = seen.header("Content-Length") {
        seen.body = vec![0; length.parse().unwrap()];
        reader.read_exact(&mut seen.body).ok()?;
    }
    Some(seen)
}

fn chunk(socket: &mut TcpStream, text: &str) -> std::io::Result<()> {
    write!(socket, "{:x}\r\n{text}\r\n", text.len())
}

fn events(mut socket: TcpStream, shared: &Arc<Shared>) {
    let (tx, rx) = mpsc::channel();
    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
    *shared.events.lock().unwrap() = Some(tx);
    let mut watch = socket.try_clone().unwrap();
    let s = shared.clone();
    std::thread::spawn(move || {
        let _ = watch.read(&mut [0; 1]);
        s.events.lock().unwrap().take();
        let _ = s.closed.lock().unwrap().as_ref().unwrap().send(());
    });
    while let Ok(Some(event)) = rx.recv() {
        if chunk(&mut socket, &format!("data: {event}\n\n")).is_err() {
            return;
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
}

fn notifications(mut socket: TcpStream, shared: &Arc<Shared>) {
    let (tx, rx) = mpsc::channel();
    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
    shared.notifications.lock().unwrap().push(tx);
    for message in rx {
        if write!(socket, "data: {message}\n\n").is_err() {
            return;
        }
    }
}

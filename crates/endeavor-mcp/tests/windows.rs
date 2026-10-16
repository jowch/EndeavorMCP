//! The local start on Windows, with a stand-in Julia (`examples/stand_in_julia.rs`, copied in as
//! `julia.exe`) and the stand-in bridge and Pluto of `common`: `endeavor serve` with a given Julia and
//! with the one on the PATH, the state folder (`runtime.json`, `runtime.log`, `starting.lock` with the
//! core's pid readable while it holds the lock), a Julia that ends, `endeavor stop`, and an agent's
//! call through `endeavor mcp` on this computer. The Unix tests cover the same with a `#!/bin/sh`
//! stand-in, which Windows can't run (EndeavorMCP #9).

#![cfg(windows)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::front::Front;
use common::*;
use serde_json::json;

/// The stand-in Julia, as `julia.exe` in `dir`'s `fakebin`.
fn stand_in(dir: &Path) -> PathBuf {
    let built = Path::new(env!("CARGO_BIN_EXE_endeavor")).parent().unwrap().join("examples").join("stand_in_julia.exe");
    assert!(built.is_file(), "{} isn't built: `cargo test` builds the examples", built.display());
    let bin = dir.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    let julia = bin.join("julia.exe");
    std::fs::copy(&built, &julia).unwrap();
    julia
}

/// What the stand-in and the runtime are given: where the stand-in records what it was run with, the
/// stand-in bridge's ports, and the account's folders, all in `dir`.
fn env_for(command: &mut Command, dir: &Path, bridge: &FakeBridge) {
    for folder in ["profile", "local", "roaming"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    command
        .env("STAND_IN_DIR", dir)
        .env("STAND_IN_MCP_PORT", bridge.port.to_string())
        .env("STAND_IN_PLUTO_PORT", bridge.pluto_port.to_string())
        .env("USERPROFILE", dir.join("profile"))
        .env("LOCALAPPDATA", dir.join("local"))
        .env("APPDATA", dir.join("roaming"));
}

/// What a test that fails part way leaves running, ended when it unwinds: `endeavor stop --force` on
/// its state folder, then each process it started, with what they started.
struct Cleanup {
    dir: PathBuf,
    pids: Vec<u32>,
}

impl Cleanup {
    fn new(dir: &Path) -> Cleanup {
        Cleanup { dir: dir.to_owned(), pids: Vec::new() }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let _ = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--force").arg("--state-dir").arg(&self.dir).output();
        for pid in &self.pids {
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
        }
    }
}

/// `endeavor serve` with its state in `dir` and `julia` (none: it looks on the PATH, with `dir`'s
/// `fakebin` first), once it has said how to connect.
fn serve(dir: &Path, bridge: &FakeBridge, julia: Option<&Path>, cleanup: &mut Cleanup) -> Child {
    // The stand-in bridge takes only its own token.
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.arg("serve").arg("--state-dir").arg(dir).arg("--depot").arg(dir.join("depot")).arg("--folder").arg(dir);
    match julia {
        Some(julia) => {
            command.arg("--julia").arg(julia);
        }
        None => {
            let path = std::env::join_paths(std::iter::once(dir.join("fakebin")).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap();
            command.env("PATH", path);
        }
    }
    env_for(&mut command, dir, bridge);
    let mut serve = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    cleanup.pids.push(serve.id());
    // Read on a thread, so a serve that hangs before it is up fails the test rather than the job.
    let out = BufReader::new(serve.stdout.take().unwrap());
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in out.lines() {
            let Ok(line) = line else { break };
            let _ = tx.send(line);
        }
    });
    loop {
        match lines.recv_timeout(Duration::from_secs(60)) {
            Ok(line) if line == "Press Ctrl-C to stop Endeavor." => return serve,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("serve wasn't up within a minute"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("serve ended before it was up"),
        }
    }
}

/// Wait for `serve` to end: its exit code and the last line it wrote to stderr.
fn ended(serve: Child) -> (Option<i32>, String) {
    let output = serve.wait_with_output().unwrap();
    let said = String::from_utf8(output.stderr).unwrap();
    (output.status.code(), said.lines().last().unwrap_or_default().to_owned())
}

/// A page of the runtime's, as a browser with the pages' token asks for it.
fn page(port: u16, pages: &str, target: &str) -> String {
    let mut page = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(page, "GET {target} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: text/html\r\nAuthorization: Bearer {pages}\r\n\r\n").unwrap();
    let mut said = String::new();
    let _ = page.read_to_string(&mut said);
    said
}

/// Start Julia from the runtime's page and wait until the stand-in Pluto answers through it: Julia's pid.
fn start_julia(dir: &Path) -> u32 {
    let record = read_json(&dir.join("runtime.json"));
    let (port, pages) = (record["port"].as_u64().unwrap() as u16, record["page_token"].as_str().unwrap().to_owned());
    let said = page(port, &pages, "/?start-julia");
    assert!(said.starts_with("HTTP/1.1 503") && said.contains("Julia is starting"), "{said}");
    wait_for("Julia to be ready", || page(port, &pages, "/edit").contains("Pluto: GET /edit"));
    let julia = read_json(&dir.join("julia.json"))["pid"].as_u64().unwrap() as u32;
    assert_eq!(julia_pids(dir), [julia as i32], "the stand-in is the Julia the core started");
    julia
}

fn running(pid: u32) -> bool {
    let listed = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output().unwrap();
    String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\""))
}

#[test]
fn serve_starts_julia_when_asked_and_stop_ends_both() {
    let dir = state_dir("windows-serve-stop");
    let mut cleanup = Cleanup::new(&dir);
    let bridge = FakeBridge::start(&dir);
    let julia = stand_in(&dir);
    let serve = serve(&dir, &bridge, Some(&julia), &mut cleanup);
    let core = read_json(&dir.join("runtime.json"))["pid"].as_u64().unwrap();
    assert!(dir.join("runtime.log").is_file(), "the log is there");
    assert_eq!(std::fs::read_to_string(dir.join("starting.lock")).unwrap(), "", "recorded, the core let go of the start lock");
    assert!(!dir.join("julia.pids").exists(), "serve starts no Julia until something needs it");

    let julia = start_julia(&dir);
    assert!(std::fs::read_to_string(dir.join("julia.args")).unwrap().contains("boot.jl"), "Julia was given the runtime to run");
    assert_eq!(engine_token(&dir).len(), 64, "Julia was given the key to its bridge");

    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--state-dir").arg(&dir).output().unwrap();
    assert_eq!(String::from_utf8(stop.stdout).unwrap(), format!("Stopped Endeavor (pid {core}).\n"));
    assert_eq!(ended(serve), (Some(0), "Endeavor was stopped with `endeavor stop`.".to_owned()));
    wait_for("Julia to end with the core", || !running(julia));
    assert!(!running(core as u32), "the core ended");
}

#[test]
fn serve_with_no_julia_given_starts_the_julia_exe_on_the_path() {
    let dir = state_dir("windows-serve-path");
    let mut cleanup = Cleanup::new(&dir);
    let bridge = FakeBridge::start(&dir);
    stand_in(&dir);
    let serve = serve(&dir, &bridge, None, &mut cleanup);
    let julia = start_julia(&dir);
    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--state-dir").arg(&dir).output().unwrap();
    assert!(stop.status.success(), "{stop:?}");
    assert_eq!(ended(serve).0, Some(0));
    wait_for("Julia to end with the core", || !running(julia));
}

#[test]
fn julia_ending_ends_serve_with_an_error() {
    let dir = state_dir("windows-serve-crash");
    let mut cleanup = Cleanup::new(&dir);
    let bridge = FakeBridge::start(&dir);
    let julia = stand_in(&dir);
    let serve = serve(&dir, &bridge, Some(&julia), &mut cleanup);
    let julia = start_julia(&dir);
    let killed = Command::new("taskkill").args(["/F", "/PID", &julia.to_string()]).output().unwrap();
    assert!(killed.status.success(), "{killed:?}");
    assert_eq!(ended(serve), (Some(1), format!("Endeavor stopped. Its log is {}.", dir.join("runtime.log").display())));
}

/// A core that waits for Julia before it records itself (no `--julia-when-needed`) holds starting.lock
/// meanwhile: a reader sees it held (`runtime::lock_state` asks for a shared lock over the whole file)
/// and still reads the core's pid, because the core locks one byte past what the file holds. Once
/// `runtime.json` is written it lets go and empties the file.
#[test]
fn the_core_holds_starting_lock_with_its_pid_readable_until_it_is_recorded() {
    let dir = state_dir("windows-start-lock");
    let mut cleanup = Cleanup::new(&dir);
    let bridge = FakeBridge::start(&dir);
    let julia = stand_in(&dir);
    std::fs::write(dir.join("hold"), "").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.arg("core").arg("--state-dir").arg(&dir).arg("--julia").arg(&julia).arg("--runtime").arg(dir.join("runtime")).arg("--depot").arg(dir.join("depot"));
    command.env("ENDEAVOR_TOKEN", TOKEN).env("ENDEAVOR_LAUNCHER", "process").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    env_for(&mut command, &dir, &bridge);
    let mut core = command.spawn().unwrap();
    cleanup.pids.push(core.id());
    wait_for("Julia to be started", || !julia_pids(&dir).is_empty());
    let lock = std::fs::File::open(dir.join("starting.lock")).unwrap();
    assert!(lock.try_lock_shared().is_err(), "a reader sees the start under way");
    // The core empties the file before it writes it, so read until it parses.
    let mut said = serde_json::Value::Null;
    wait_for("starting.lock to name the core", || {
        said = std::fs::read_to_string(dir.join("starting.lock")).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        !said.is_null()
    });
    assert_eq!(said["pid"].as_u64(), Some(core.id() as u64), "{said}");
    assert!(!dir.join("runtime.json").exists());

    std::fs::remove_file(dir.join("hold")).unwrap();
    wait_for("the core to record itself", || dir.join("runtime.json").exists());
    wait_for("the core to let go of the lock", || std::fs::read_to_string(dir.join("starting.lock")).unwrap_or_default().is_empty());
    assert!(lock.try_lock_shared().is_ok(), "no start under way");
    drop(lock);
    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--state-dir").arg(&dir).output().unwrap();
    assert!(stop.status.success(), "{stop:?}");
    core.wait().unwrap();
}

/// What failed for the Antigravity plugin on Windows before #10: an agent's notebook call through
/// `endeavor mcp` on this computer, which starts the runtime and Julia and reaches Pluto.
#[test]
fn an_agents_notebook_call_through_the_front_reaches_julia() {
    require_debug_build();
    let dir = state_dir("windows-front");
    let _cleanup = Cleanup::new(&dir.join("state"));
    let (state, project) = (dir.join("state"), dir.join("project"));
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let bridge = FakeBridge::start(&state);
    std::fs::write(state.join("token"), TOKEN).unwrap();
    let julia = stand_in(&dir);
    let notebook = project.join("a.jl");
    std::fs::write(&notebook, "### A Pluto.jl notebook ###\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command.args(["mcp", "--skills", "plugin", "--folder"]).arg(&project).arg("--julia").arg(&julia).arg("--depot").arg(dir.join("depot")).arg("--state-dir").arg(&state);
    command.current_dir(&project).env("ENDEAVOR_START_WAIT_SECS", "30");
    env_for(&mut command, &dir, &bridge);
    // The stand-in records what it was run with next to the state, where the bridge looks for its token.
    command.env("STAND_IN_DIR", &state);
    let mut front = Front::spawn(command);
    front.initialize();
    assert_eq!(front.ok("use_machine", json!({ "machine": "local" }))["state"], "ready", "{:?}", front.said());
    let (_, said) = front.call("open_notebook", json!({ "path": "a.jl" }));
    // The stand-in Pluto opens nothing it wasn't given, so its own answer shows the call reached it.
    assert!(said.to_string().contains("not a notebook here"), "{said}; the front said:\n{}", front.said().join("\n"));
    let opened = bridge.seen().iter().any(|s| s.line.starts_with("POST /adapter") && String::from_utf8_lossy(&s.body).contains(r#""method":"open""#));
    assert!(opened, "the open reached the bridge");
    front.finish();
    let _ = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--state-dir").arg(&state).output();
}

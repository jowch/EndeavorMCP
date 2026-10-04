//! `endeavor serve` with a stand-in Julia (see `common`): how it ends when
//! the runtime it started goes away.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use common::*;

/// `endeavor serve` with its state in `dir`, once it has said how to connect.
fn serve(dir: &Path, bridge: &FakeBridge) -> Child {
    // The stand-in bridge takes only its own token.
    std::fs::write(dir.join("token"), TOKEN).unwrap();
    let julia = serving_julia(dir, bridge);
    let mut serve = Command::new(env!("CARGO_BIN_EXE_endeavor"))
        .arg("serve")
        .arg("--state-dir")
        .arg(dir)
        .arg("--julia")
        .arg(&julia)
        .args(["--depot", "/opt/depot:"])
        .arg("--folder")
        .arg(dir)
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(serve.stdout.take().unwrap());
    let mut line = String::new();
    while line.trim_end() != "Press Ctrl-C to stop Julia." {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "serve ended before Julia was up");
    }
    serve
}

/// Wait for `serve` to end: its exit code and the last line it wrote to stderr.
fn ended(serve: Child) -> (Option<i32>, String) {
    let output = serve.wait_with_output().unwrap();
    let said = String::from_utf8(output.stderr).unwrap();
    (output.status.code(), said.lines().last().unwrap_or_default().to_owned())
}

#[test]
fn stop_from_another_terminal_ends_serve_without_an_error() {
    let dir = state_dir("serve-stop");
    let bridge = FakeBridge::start(&dir);
    let serve = serve(&dir, &bridge);
    let core = read_json(&dir.join("runtime.json"))["pid"].as_i64().unwrap();

    let stop = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg("stop").arg("--state-dir").arg(&dir).output().unwrap();
    assert_eq!(String::from_utf8(stop.stdout).unwrap(), format!("Stopped Julia (pid {core}).\n"));
    assert_eq!(ended(serve), (Some(0), "Julia was stopped with `endeavor stop`.".to_owned()));
}

#[test]
fn julia_dying_ends_serve_with_an_error() {
    let dir = state_dir("serve-crash");
    let bridge = FakeBridge::start(&dir);
    let serve = serve(&dir, &bridge);
    let julia = read_json(&dir.join("julia.json"))["pid"].as_i64().unwrap() as i32;

    // SAFETY: plain syscall.
    unsafe { libc::kill(julia, libc::SIGKILL) };
    assert_eq!(ended(serve), (Some(1), format!("Julia stopped. Its log is {}.", dir.join("runtime.log").display())));
}

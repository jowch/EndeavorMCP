//! The recording proxy. The plugin's launcher runs `$ENDEAVOR_BIN mcp ARGS`; with
//! ENDEAVOR_BIN set to this binary, it starts the real `endeavor mcp ARGS` with
//! the task's own state, Julia and depot, passes stdin and stdout through, and
//! writes every message to the task's log. Its settings come from the runner in
//! SMOKE_* variables, since an agent passes a plugin's server only what the
//! plugin's own command line says. A task with an `inject.json` also gets a
//! second person, at the moment it names (inject.rs).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::inject::Injection;
use crate::log::Log;

pub fn main(args: &[String]) -> i32 {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from).unwrap_or_else(|| panic!("{name} isn't set: the runner sets it"));
    let (real, work, project) = (var("SMOKE_ENDEAVOR"), var("SMOKE_WORK"), var("SMOKE_PROJECT"));
    let log = Arc::new(Mutex::new(Log::create(&work.join("mcp.jsonl"))));
    let injection = Arc::new(Injection::load(&work, &real, &project));
    // The tool each pending request called, by its id, to know the reply's.
    let pending: Arc<Mutex<HashMap<String, String>>> = Arc::default();
    let mut child = match Command::new(&real)
        .arg("mcp")
        .args(args)
        .args(crate::run::runtime_args(&work))
        .envs(crate::run::runtime_env(&work))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("endeavor-smoke: can't start {}: {e}", real.display());
            return 1;
        }
    };
    let mut to_child = child.stdin.take().unwrap();
    let from_child = child.stdout.take().unwrap();
    let inbound = {
        let (log, injection, pending) = (log.clone(), injection.clone(), pending.clone());
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                log.lock().unwrap().write("in", &line);
                if let Some((id, tool)) = tool_call(&line) {
                    if let Some(injection) = injection.as_ref().as_ref().filter(|i| i.matches("before", &tool)) {
                        injection.run(&log);
                    }
                    pending.lock().unwrap().insert(id, tool);
                }
                if writeln!(to_child, "{line}").is_err() {
                    break;
                }
            }
        })
    };
    let mut out = std::io::stdout().lock();
    for line in BufReader::new(from_child).lines().map_while(Result::ok) {
        log.lock().unwrap().write("out", &line);
        if let Some(injection) = injection.as_ref() {
            let msg: Value = serde_json::from_str(&line).unwrap_or_default();
            let tool = pending.lock().unwrap().remove(&msg["id"].to_string());
            let ok = msg["error"].is_null() && msg["result"]["isError"] != true;
            if tool.is_some_and(|t| ok && injection.matches("after", &t)) {
                injection.run(&log);
            }
        }
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    drop(inbound);
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}

/// A `tools/call` request's id (as JSON text) and tool.
fn tool_call(line: &str) -> Option<(String, String)> {
    let msg: Value = serde_json::from_str(line).ok()?;
    (msg["method"] == "tools/call").then(|| (msg["id"].to_string(), msg["params"]["name"].as_str().unwrap_or_default().to_owned()))
}

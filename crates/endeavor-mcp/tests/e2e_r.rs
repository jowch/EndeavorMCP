//! An R notebook end to end, with real R and no Julia: `endeavor serve`, then
//! an agent opens an Ember notebook, which starts R's adapter with Ember in it;
//! it reads, edits and runs a cell, and the browser reaches Ember's page at
//! `/ember/` through the runtime's port.
//!
//! Julia is given as a path that doesn't exist, so the test fails if anything
//! tries to start it: an R user needs no Julia.
//!
//! Ignored by default, like e2e_julia; R is `Rscript` on the PATH. The first run installs Ember's latest build into
//! ~/.cache/endeavor/r/ember, which needs Ember's r-universe repository, and CRAN for any package R lacks
//! (a debug build installs from `ENDEAVOR_TEST_EMBER_REPOSITORY` instead when it's set). A second test
//! runs `runtime/r/install.R` against stand-in builds: an update, a build that doesn't load, and offline.
//!
//!     cargo test -p endeavor-mcp --test e2e_r -- --ignored --nocapture

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn fresh(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn recorded_pid(state: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(state.join("runtime.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["pid"].as_i64().map(|p| p as i32).filter(|&p| p > 1)
}

/// Whatever happens, no Julia stays behind.
struct Cleanup {
    state: PathBuf,
    children: Vec<Child>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(pid) = recorded_pid(&self.state) {
            // SAFETY: plain syscall; the core leads Julia's process group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
    }
}

/// `endeavor ARGS` with the test's own state, cache, depot and julia.
fn command(args: &[&str], work: &Path, julia: &Path, depot: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_endeavor"));
    command
        .args(args)
        .arg("--state-dir")
        .arg(work.join("state"))
        .env("XDG_CACHE_HOME", work.join("cache"))
        .env("XDG_STATE_HOME", work.join("state-home"))
        .env("XDG_CONFIG_HOME", work.join("config"))
        .env("ENDEAVOR_IDLE_CHECK_SECS", "1");
    if args[0] != "stop" {
        command.args(["--julia", julia.to_str().unwrap(), "--depot", depot]);
    }
    command
}

/// Lines of a child's stream as they come.
fn lines(stream: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// POST a JSON-RPC message to `/mcp` with the bearer token and, once it has
/// one, the session id, as an agent configured from serve's output does: the
/// response's head and body.
fn post(port: u16, token: &str, session: Option<&str>, message: &Value) -> (String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(300))).unwrap();
    let body = message.to_string();
    let session = session.map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));
    write!(
        socket,
        "POST /mcp HTTP/1.0\r\nHost: localhost:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.to_owned(), body.to_owned())
}

fn get(port: u16, target: &str, headers: &str) -> (String, String, String) {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(socket, "GET {target} HTTP/1.0\r\nHost: localhost:{port}\r\n{headers}\r\n").unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).unwrap();
    let reply = String::from_utf8_lossy(&reply).into_owned();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
    (head.lines().next().unwrap_or_default().to_owned(), head.to_owned(), body.to_owned())
}

struct Agent {
    port: u16,
    token: String,
    id: u64,
    /// The `Mcp-Session-Id` from `initialize`, sent back on every request after.
    session: Option<String>,
}

impl Agent {
    fn new(port: u16, token: &str) -> Agent {
        Agent { port, token: token.to_owned(), id: 0, session: None }
    }

    fn mcp(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let (head, body) = post(self.port, &self.token, self.session.as_deref(), &json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params }));
        assert_eq!(head.lines().next(), Some("HTTP/1.1 200 OK"), "{method}: {body}");
        if method == "initialize" {
            self.session = head.lines().find_map(|line| line.strip_prefix("Mcp-Session-Id: ")).map(str::to_owned);
        }
        serde_json::from_str(&body).unwrap()
    }

    fn initialize(&mut self) -> Value {
        self.mcp("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "0" } }))
    }

    fn call(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let reply = self.mcp("tools/call", json!({ "name": name, "arguments": arguments }));
        let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {reply}"));
        (reply["result"]["isError"] == true, serde_json::from_str(text).unwrap())
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        let (failed, result) = self.call(name, arguments.clone());
        assert!(!failed, "{name}({arguments}): {result}");
        result
    }
}

fn step<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    eprintln!("── {name}");
    let result = f();
    eprintln!("   {:.1}s", started.elapsed().as_secs_f64());
    result
}

const A: &str = "0b7d0000-0000-4000-8000-000000000001";
const B: &str = "0b7d0000-0000-4000-8000-000000000002";

#[test]
#[ignore = "starts real R: cargo test -p endeavor-mcp --test e2e_r -- --ignored"]
fn an_r_notebook_through_the_runtime() {
    if !Command::new("Rscript").arg("--version").stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        eprintln!("SKIPPED: no Rscript on the PATH.");
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-r");
    let julia = work.join("no julia here");
    let depot = format!("{}:", work.join("depot").display());
    if let Some(pid) = recorded_pid(&work.join("state")) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    let mut cleanup = Cleanup { state: state.clone(), children: Vec::new() };
    std::fs::write(
        folder.join("growth.R"),
        format!("### An Ember notebook ###\n# /// environment\n# on_cell_change = \"lazy\"\n# ///\n\n# %% id={A}\nx <- 20\n\n# %% id={B}\ny <- x + 1\n\n# /// cell order\n# {A}\n# {B}\n# ///\n"),
    )
    .unwrap();

    // R comes from a shell line, as `module load R` would give it: the line puts a folder on the PATH whose
    // Rscript says it ran and then runs R's own.
    let real = String::from_utf8(Command::new("sh").args(["-c", "command -v Rscript"]).output().unwrap().stdout).unwrap().trim().to_owned();
    let module = fresh(work.join("module r"));
    let ran = module.join("ran");
    std::fs::write(module.join("Rscript"), format!("#!/bin/sh\necho \"$LOADED\" >> '{}'\nexec '{real}' \"$@\"\n", ran.display())).unwrap();
    std::fs::set_permissions(module.join("Rscript"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let line = format!("export PATH=\"{}:$PATH\" LOADED=by-the-line; echo the line ran", module.display());
    let mut serve = command(&["serve", "--folder", folder.to_str().unwrap(), "--r-shell", &line], &work, &julia, &depot)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(serve.stdout.take().unwrap());
    let err = lines(serve.stderr.take().unwrap());
    let printed = step("serve starts without Julia", || {
        let mut printed = Vec::new();
        loop {
            match out.recv_timeout(Duration::from_secs(900)) {
                Ok(line) if line == "Press Ctrl-C to stop Endeavor." => break printed,
                Ok(line) => printed.push(line),
                Err(_) => panic!("serve printed {printed:?}; its log:\n{}", err.try_iter().collect::<Vec<_>>().join("\n")),
            }
        }
    });
    cleanup.children.push(serve);
    let link = printed.iter().find_map(|l| l.trim().strip_prefix("http://localhost:")).filter(|l| l.contains("/?token=")).unwrap();
    let (port, token) = link.split_once("/?token=").unwrap();
    let (port, token): (u16, String) = (port.parse().unwrap(), token.to_owned());
    let mut agent = Agent::new(port, &token);
    agent.initialize();

    let notebook = step("opening an R notebook starts R and Ember, in safe preview", || {
        // The first time, Ember installs in the background into a library of Endeavor's own.
        let deadline = Instant::now() + Duration::from_secs(1200);
        let opened = loop {
            let (failed, opened) = agent.call("open_notebook", json!({ "path": "growth.R" }));
            if !failed {
                break opened;
            }
            assert!(opened["error"] == "r_installing" && Instant::now() < deadline, "{opened}");
            std::thread::sleep(Duration::from_secs(5));
        };
        // A warning here is `ember_previous`: Ember's newest build didn't work, which the daily run is for.
        assert_eq!(opened.get("warnings"), None, "{opened}");
        let notebook = opened["notebook_id"].as_str().unwrap().to_owned();
        assert_eq!((&opened["path"], &opened["execution_allowed"]), (&json!(folder.join("growth.R").display().to_string()), &json!(false)), "{opened}");
        assert_eq!(opened["browser_url"], json!(format!("http://localhost:{port}/ember/edit?id={notebook}")));
        let r_state = std::fs::read_to_string(state.join("r.json")).expect("R's adapter wrote its state");
        assert!(!r_state.contains("ember_secret"), "the core made Ember's secret and R doesn't write it back: {r_state}");
        let ran = std::fs::read_to_string(&ran).unwrap_or_default();
        assert!(!ran.is_empty() && ran.lines().all(|l| l == "by-the-line"), "R ran from --r-shell's line, with what it set: {ran:?}");
        let listed = agent.ok("list_notebooks", json!({}));
        assert_eq!(listed.as_array().unwrap().iter().map(|nb| nb["notebook_id"].clone()).collect::<Vec<_>>(), [json!(notebook)], "{listed}");
        notebook
    });

    step("read, edit and run a cell; the output comes back", || {
        let order = agent.ok("get_cell_order", json!({ "notebook_id": notebook }));
        assert_eq!(order["cell_ids"], json!([A, B]), "{order}");
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": A }));
        assert_eq!(read["code"], "x <- 20", "{read}");
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": A, "code": "x <- 41\n" }));
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": B, "code": "x + 1" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": B, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        assert_eq!(read["errored"], false, "{read}");
        assert!(read["output"].as_str().unwrap().contains("42"), "{read}");
    });

    step("an ancestor run alone leaves its dependent's result stale", || {
        agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": A }));
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": A, "code": "x <- 50" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": A, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        // growth.R is in Ember's lazy mode, so a run leaves its dependents stale; in autorun Ember would
        // rerun B on its own and the flag would clear a moment later. B still shows its result from x = 41.
        assert!(read["stale"] == true && read["output"].as_str().unwrap().contains("42"), "{read}");
        let code = agent.ok("read_notebook_code", json!({ "notebook_id": notebook }));
        assert_eq!(code["stale_cell_ids"], json!([B]), "{code}");
        // Back as it was, run, for the steps after.
        agent.ok("edit_cell", json!({ "notebook_id": notebook, "cell_id": A, "code": "x <- 41" }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": A, "wait_for_completion": true }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": B, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": B }));
        assert!(read["stale"] == false && read["output"].as_str().unwrap().contains("42"), "{read}");
    });

    step("new_notebook makes an R notebook from an .R path; a warning reads as one", || {
        // A session works on one notebook, so another session makes this one.
        let mut agent = Agent::new(port, &token);
        agent.initialize();
        let made = agent.ok("new_notebook", json!({ "path": "fresh.R" }));
        assert!(folder.join("fresh.R").exists() && made["created"] == true && made.get("warnings").is_none(), "{made}");
        assert!(wire::backend::Backend::of_file(&folder.join("fresh.R")) == Some(wire::backend::Backend::Ember), "an Ember file");
        let (id, cell) = (made["notebook_id"].as_str().unwrap(), made["cell_ids"][0].as_str().unwrap());
        agent.ok("edit_cell", json!({ "notebook_id": id, "cell_id": cell, "code": "warning(\"careful\")\n7", "run_after": true }));
        common::wait_for("the new cell to run", || agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }))["output"].as_str().is_some_and(|o| o.contains("7")));
        let read = agent.ok("read_cell", json!({ "notebook_id": id, "cell_id": cell }));
        assert!(read["output"].as_str().unwrap().starts_with("Warning: careful"), "{read}");
    });

    step("the browser link reaches Ember's page through the port", || {
        let (status, head, _) = get(port, &format!("/ember/edit?id={notebook}&token={token}"), "");
        assert_eq!(status, "HTTP/1.1 303 See Other", "{head}");
        let set = head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap_or_else(|| panic!("no cookie: {head}"));
        let cookie = format!("Cookie: {}\r\n", set.split(';').next().unwrap());
        let (status, head, page) = get(port, &format!("/ember/edit?id={notebook}"), &format!("{cookie}Sec-Fetch-Site: none\r\n"));
        assert_eq!(status, "HTTP/1.1 200 OK", "{head}");
        assert!(!head.contains("ember_secret"), "Ember's cookie stays behind the port: {head}");
        assert!(page.to_lowercase().contains("ember"), "{}", &page[..page.len().min(300)]);
        assert!(get(port, &format!("/ember/edit?id={notebook}"), "").0.starts_with("HTTP/1.1 401"));
    });

    step("when R ends, its notebook goes, and opening it again starts R again", || {
        let r_pid = std::fs::read_to_string(state.join("r.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).unwrap();
        // SAFETY: plain syscall.
        unsafe { libc::kill(r_pid as i32, libc::SIGKILL) };
        common::wait_for("R's notebook to leave the list", || agent.ok("list_notebooks", json!({})) == json!([]));
        let opened = agent.ok("open_notebook", json!({ "path": "growth.R" }));
        let read = agent.ok("read_cell", json!({ "notebook_id": opened["notebook_id"], "cell_id": A }));
        assert_eq!(read["code"], "x <- 41", "Ember saved the edit: {read}");
    });

    step("Ember's newest build installed and loaded: none failed", || {
        let builds = PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/endeavor/r/ember/ember.dcf");
        let builds = std::fs::read_to_string(&builds).unwrap();
        assert!(builds.lines().any(|line| line.trim_end() == "Failed:"), "{builds}");
    });

    step("nothing started Julia", || {
        assert!(!state.join("julia.json").exists(), "Julia's state file is there");
        assert!(!work.join("cache/endeavor").read_dir().into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().starts_with("julia")), "a Julia was downloaded");
    });

    step("Ctrl-C stops R", || {
        let r_pid = std::fs::read_to_string(state.join("r.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["pid"].as_i64()).unwrap();
        let mut serve = cleanup.children.pop().unwrap();
        // SAFETY: plain syscall.
        unsafe { libc::kill(serve.id() as i32, libc::SIGINT) };
        assert!(serve.wait().unwrap().success());
        common::wait_for("R's adapter to end", || !common::pid_alive(r_pid as i32));
        assert!(!state.join("r.json").exists());
    });
}

/// A repository like Ember's r-universe one, with one build of a stand-in `ember` whose file
/// has SHA256 `sha`; a broken build doesn't load.
fn stand_in_repository(repository: &Path, sha: &str, broken: bool) {
    let source = fresh(repository.with_extension("source")).join("ember");
    std::fs::create_dir_all(source.join("R")).unwrap();
    std::fs::write(source.join("DESCRIPTION"), format!("Package: ember\nVersion: 0.0.0.9000\nTitle: Stand-in\nDescription: Stand-in.\nLicense: MIT\nImports: jsonlite\nSHA256: {sha}\nRemoteSha: {sha}\n")).unwrap();
    let on_load = if broken { ".onLoad <- function(...) stop(\"a broken build\")\n" } else { "" };
    std::fs::write(source.join("R/build.R"), format!("build <- function() \"{sha}\"\n{on_load}")).unwrap();
    std::fs::write(source.join("NAMESPACE"), "export(build)\n").unwrap();
    let contrib = fresh(repository.join("src/contrib"));
    assert!(Command::new("R").args(["CMD", "build", "--no-manual"]).arg(&source).current_dir(&contrib).stdout(Stdio::null()).status().unwrap().success());
    assert!(Command::new("Rscript").args(["-e", "tools::write_PACKAGES('.', fields = 'SHA256')"]).current_dir(&contrib).status().unwrap().success());
}

#[test]
#[ignore = "starts real R: cargo test -p endeavor-mcp --test e2e_r -- --ignored"]
fn ember_updates_from_its_repository_and_keeps_the_build_before() {
    if !Command::new("Rscript").arg("--version").stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        eprintln!("SKIPPED: no Rscript on the PATH.");
        return;
    }
    let work = fresh(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-r-update"));
    let (repository, folder) = (work.join("repository"), work.join("ember"));
    let url = format!("file://{}", repository.display());
    let install = |url: &str| {
        let status = Command::new("Rscript").args(["--vanilla", concat!(env!("CARGO_MANIFEST_DIR"), "/../../runtime/r/install.R")]).arg(&folder).arg(url).status().unwrap();
        // As R writes it: an empty field has no space after its colon.
        let state = std::fs::read_to_string(folder.join("ember.dcf")).unwrap_or_default();
        (status.code(), state.lines().map(str::trim_end).collect::<Vec<_>>().join(" | "))
    };
    let build = |name: &str| {
        let out = Command::new("Rscript").args(["-e", "cat(ember::build())"]).env("R_LIBS", format!("{}:{}", folder.join(name).display(), folder.join("deps").display())).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };

    step("offline with nothing installed: no Ember", || {
        assert_eq!(install("https://nowhere.invalid").0, Some(1));
    });
    step("the first install", || {
        stand_in_repository(&repository, "aaaaaaaaaaaaaaaa", false);
        assert_eq!(install(&url), (Some(0), "Current: aaaaaaaaaaaa | Previous: | Failed:".into()));
        assert_eq!(build("aaaaaaaaaaaa"), "aaaaaaaaaaaaaaaa");
    });
    step("the newest already: nothing changes", || {
        assert_eq!(install(&url).1, "Current: aaaaaaaaaaaa | Previous: | Failed:");
    });
    step("a newer build, and the one before is kept", || {
        stand_in_repository(&repository, "bbbbbbbbbbbbbbbb", false);
        assert_eq!(install(&url), (Some(0), "Current: bbbbbbbbbbbb | Previous: aaaaaaaaaaaa | Failed:".into()));
        assert_eq!(build("bbbbbbbbbbbb"), "bbbbbbbbbbbbbbbb");
    });
    step("a build that doesn't load: the installed one stays, and the broken one isn't tried again", || {
        stand_in_repository(&repository, "cccccccccccccccc", true);
        assert_eq!(install(&url), (Some(3), "Current: bbbbbbbbbbbb | Previous: aaaaaaaaaaaa | Failed: cccccccccccc".into()));
        assert!(!folder.join("cccccccccccc").exists());
        assert_eq!(install(&url).0, Some(0));
    });
    step("offline: the installed Ember", || {
        assert_eq!(install("https://nowhere.invalid"), (Some(0), "Current: bbbbbbbbbbbb | Previous: aaaaaaaaaaaa | Failed: cccccccccccc".into()));
    });
}

/// What a call to the core's `/endeavor/call` answers, as the app and `use_machine` make them.
#[cfg(target_os = "macos")]
fn app_call(port: u16, token: &str, method: &str) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method }).to_string();
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(socket, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
}

/// On a Mac with no R, opening an R notebook offers Endeavor's own R and installs nothing; after the
/// user's yes it installs R from CRAN into Endeavor's folder, and the notebook runs on that R, loading
/// nothing from the user's R library. No shell startup file is written or changed, here or in the real home folder.
///
/// It runs with a scratch home folder and a login shell whose PATH has no R, so the R the Mac has, if
/// any, isn't found. It downloads R (about 105 MB) and installs Ember, which needs a compiler:
///
///     cargo test -p endeavor-mcp --test e2e_r endeavors_own_r -- --ignored --nocapture
#[cfg(target_os = "macos")]
#[test]
#[ignore = "downloads and installs R: cargo test -p endeavor-mcp --test e2e_r endeavors_own_r -- --ignored"]
fn endeavors_own_r_on_a_mac() {
    const STARTUP: [&str; 6] = [".zshrc", ".zprofile", ".zshenv", ".profile", ".bash_profile", ".bashrc"];
    let startup = |home: &Path| STARTUP.map(|name| std::fs::read(home.join(name)).ok());
    let real_home = std::env::home_dir().unwrap();
    let real_before = startup(&real_home);

    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-own-r");
    let julia = work.join("no julia here");
    let depot = format!("{}:", work.join("depot").display());
    if let Some(pid) = recorded_pid(&work.join("state")) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let state = fresh(work.join("state"));
    let folder = fresh(work.join("project"));
    let home = fresh(work.join("home"));
    let mut cleanup = Cleanup { state: state.clone(), children: Vec::new() };
    // Which R runs the cell, and whether any library it loads from is the user's own R's. (Ember gives each
    // notebook a library of its own, so that is what R_LIBS_USER is inside the cell.)
    let users_library = real_home.join("Library/R");
    let cell = format!("paste(R.version$major, R.version$minor, R.home(), any(startsWith(.libPaths(), \"{}\")))", users_library.display());
    std::fs::write(folder.join("which.R"), format!("### An Ember notebook ###\n\n# %% id={A}\n{cell}\n\n# /// cell order\n# {A}\n# ///\n")).unwrap();
    // A login shell whose PATH has no R.
    let shell = work.join("shell without r");
    std::fs::write(&shell, "#!/bin/sh\nshift\nPATH=/usr/bin:/bin exec /bin/sh -c \"$1\"\n").unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let mut serve = command(&["serve", "--folder", folder.to_str().unwrap()], &work, &julia, &depot)
        .env("HOME", &home)
        .env("SHELL", &shell)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = lines(serve.stdout.take().unwrap());
    let err = lines(serve.stderr.take().unwrap());
    let mut printed = Vec::new();
    loop {
        match out.recv_timeout(Duration::from_secs(300)) {
            Ok(line) if line == "Press Ctrl-C to stop Endeavor." => break,
            Ok(line) => printed.push(line),
            Err(_) => panic!("serve printed {printed:?}; its log:\n{}", err.try_iter().collect::<Vec<_>>().join("\n")),
        }
    }
    cleanup.children.push(serve);
    let link = printed.iter().find_map(|l| l.trim().strip_prefix("http://localhost:")).filter(|l| l.contains("/?token=")).unwrap();
    let (port, token) = link.split_once("/?token=").unwrap();
    let (port, token): (u16, String) = (port.parse().unwrap(), token.to_owned());
    let mut agent = Agent::new(port, &token);
    agent.initialize();
    let own = home.join(".cache/endeavor/R-4.6.1");

    step("with no R, opening an R notebook asks for Endeavor's own and installs nothing", || {
        let (failed, said) = agent.call("open_notebook", json!({ "path": "which.R" }));
        assert!(failed && said["error"] == "r_not_found", "{said}");
        let message = said["message"].as_str().unwrap_or_default();
        assert!(message.contains("only if the user agrees") && message.contains("R 4.6.1") && message.contains(&own.display().to_string()), "{said}");
        assert!(!own.exists());
    });

    let notebook = step("after the user's yes, R installs and the notebook opens", || {
        assert_eq!(app_call(port, &token, "endeavor/allow_r_install")["result"], json!({}));
        let deadline = Instant::now() + Duration::from_secs(1800);
        loop {
            let (failed, opened) = agent.call("open_notebook", json!({ "path": "which.R" }));
            if !failed {
                break opened["notebook_id"].as_str().unwrap().to_owned();
            }
            assert!(opened["error"] == "r_installing" && Instant::now() < deadline, "{opened}; the log:\n{}", err.try_iter().collect::<Vec<_>>().join("\n"));
            std::thread::sleep(Duration::from_secs(5));
        }
    });

    step("the notebook runs on Endeavor's R, and loads nothing from the user's R library", || {
        agent.ok("allow_execution", json!({ "notebook_id": notebook, "run_notebook": false }));
        agent.ok("execute_cell", json!({ "notebook_id": notebook, "cell_id": A, "wait_for_completion": true }));
        let read = agent.ok("read_cell", json!({ "notebook_id": notebook, "cell_id": A }));
        let output = read["output"].as_str().unwrap_or_default();
        assert!(read["errored"] == false && output.contains(&format!("4 6.1 {} FALSE", own.display())), "{read}");
    });

    step("no shell startup file was written or changed", || {
        assert_eq!(startup(&home), [None, None, None, None, None, None], "in the scratch home");
        assert!(!home.join(".local").exists(), "nothing in ~/.local");
        assert!(startup(&real_home) == real_before, "in the real home folder");
    });
}

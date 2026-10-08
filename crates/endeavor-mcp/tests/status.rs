//! `endeavor status` as a user runs it: the built binary, with `HOME` and the four XDG variables in
//! a scratch folder. It reports and changes nothing, and never prints the runtime's token.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::{Value, json};

const TOKEN: &str = "tok-4f9a1c7e2b8d0a63";

/// A scratch `HOME` and XDG folders, all empty, and a working folder.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(name: &str) -> Home {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("status-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for folder in ["home", "state", "cache", "config", "data", "cwd"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        Home { root }
    }

    fn path(&self, folder: &str) -> PathBuf {
        self.root.join(folder)
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_endeavor"))
            .arg("status")
            .args(args)
            .current_dir(self.path("cwd"))
            .env_clear()
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
        output
    }

    /// The plain output, and the JSON output parsed. Neither holds the token.
    fn both(&self, args: &[&str]) -> (String, Value) {
        let text = String::from_utf8(self.run(args).stdout).unwrap();
        let json_out = String::from_utf8(self.run(&[args, &["--json"]].concat()).stdout).unwrap();
        for out in [&text, &json_out] {
            assert!(!out.contains(TOKEN) && !out.contains("token"), "the token is printed:\n{out}");
        }
        (text, serde_json::from_str(&json_out).unwrap())
    }

    /// The folders of `HOME` and the XDG variables hold nothing.
    fn assert_untouched(&self) {
        for folder in ["home", "state", "cache", "config", "data", "cwd"] {
            let found: Vec<_> = std::fs::read_dir(self.path(folder)).unwrap().flatten().map(|e| e.path()).collect();
            assert!(found.is_empty(), "status made {found:?}");
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn hostname() -> String {
    let uname = Command::new("uname").arg("-n").output().unwrap();
    String::from_utf8_lossy(&uname.stdout).trim_end().to_owned()
}

/// A `runtime.json` in `dir` for a runtime of this computer.
fn record(dir: &Path, fields: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let mut state = json!({ "launcher": "process", "node": hostname(), "token": TOKEN });
    for (key, value) in fields.as_object().unwrap() {
        state[key] = value.clone();
    }
    std::fs::write(dir.join("runtime.json"), state.to_string()).unwrap();
}

/// A port that answers every call with 200, for as long as the test runs.
fn answering() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut socket in listener.incoming().flatten() {
            let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
            while socket.read(&mut [0; 1024]).is_ok_and(|n| n > 0) {}
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    port
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn with_nothing_there_it_says_so_and_makes_nothing() {
    let home = Home::new("nothing");
    let (text, json_out) = home.both(&[]);
    assert!(text.contains("Not running.") && text.contains("Machines file:") && text.contains("  none"), "{text}");
    assert!(text.contains("(0 remembered)"), "{text}");
    assert_eq!(json_out["runtime"]["state"], "not_running");
    assert_eq!(json_out["machines"]["exists"], false);
    assert_eq!(json_out["projects"]["count"], 0);
    assert_eq!(json_out["folders"]["server_root"]["exists"], false);
    assert_eq!(json_out["cluster"], Value::Null);
    assert!(json_out["program"].as_str().unwrap().ends_with("endeavor"));
    home.assert_untouched();
}

#[test]
fn it_reports_the_machines_the_projects_and_a_dead_runtime() {
    let home = Home::new("report");
    let config = home.path("config").join("endeavor");
    std::fs::create_dir_all(&config).unwrap();
    let machines = json!({ "schema": 1, "machines": [
        { "id": "lab", "name": "Lab", "ssh_host": "ada@lab.example.org", "port": 2222 },
        { "id": "hpc", "name": "Cluster", "ssh_host": "login.hpc.example.org", "cluster": {} },
    ] })
    .to_string();
    std::fs::write(config.join("machines.json"), &machines).unwrap();
    let state = home.path("state").join("endeavor");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("projects.json"), json!({ "/a": { "machine": "lab" }, "/b": { "machine": "hpc", "folder": "x" } }).to_string()).unwrap();
    record(&state.join("serve").join(hostname()), json!({ "pid": i32::MAX, "port": 4321, "build": "0.1.0-aaaa" }));
    let build = home.path("data").join("endeavor/bin/0123456789ab");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(build.join("endeavor"), [0u8; 10]).unwrap();
    let cluster = state.join("cluster");
    std::fs::create_dir_all(&cluster).unwrap();
    std::fs::write(cluster.join("job.json"), "{}").unwrap();
    std::fs::write(state.join("serve").join(hostname()).join("runtime.log"), "booting\n").unwrap();

    let (text, json_out) = home.both(&[]);
    assert!(text.contains("Recorded, but its process is gone (pid 2147483647)"), "{text}");
    assert!(text.contains("Lab: ada@lab.example.org:2222, Julia in Slurm jobs: no"), "{text}");
    assert!(text.contains("Cluster: login.hpc.example.org, Julia in Slurm jobs: yes"), "{text}");
    assert!(text.contains("(2 remembered)"), "{text}");
    assert!(text.contains("10 B; builds: 0123456789ab"), "{text}");
    assert!(text.contains("runtime.log") && text.contains("Cluster state folder:") && text.contains("holds job.json"), "{text}");
    assert_eq!(json_out["runtime"]["state"], "stale");
    assert_eq!(json_out["machines"]["machines"][1], json!({ "id": "hpc", "name": "Cluster", "host": "login.hpc.example.org", "slurm": true }));
    assert_eq!(json_out["projects"]["count"], 2);
    assert_eq!(json_out["folders"]["plugin_binaries"]["builds"], json!(["0123456789ab"]));
    assert_eq!(json_out["folders"]["plugin_binaries"]["size_bytes"], 10);
    assert_eq!(json_out["cluster"]["files"], json!(["job.json"]));

    assert_eq!(std::fs::read_to_string(config.join("machines.json")).unwrap(), machines, "the machines file is not rewritten");
    let held: Vec<_> = std::fs::read_dir(state.join("serve").join(hostname())).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert_eq!(held.len(), 2, "{held:?}");
    assert!(state.join("serve").join(hostname()).join("runtime.json").exists(), "a stale record is left");
    assert!(!state.join("serve").join(hostname()).join("starting.lock").exists());
    assert!(!home.path("home").join(".cache").exists() && !home.path("cache").join("endeavor").exists());
}

#[test]
fn a_machines_file_from_a_newer_schema_or_not_json_lists_nothing() {
    let home = Home::new("schema");
    let config = home.path("config").join("endeavor");
    std::fs::create_dir_all(&config).unwrap();
    for (contents, said) in [
        (json!({ "schema": 2, "machines": [{ "id": "lab", "name": "Lab", "ssh_host": "lab.example.org" }] }).to_string(), "A newer Endeavor wrote"),
        ("{ not json".to_owned(), "isn't valid"),
    ] {
        std::fs::write(config.join("machines.json"), &contents).unwrap();
        let (text, json_out) = home.both(&[]);
        assert!(text.contains("Not listed:") && text.contains(said) && !text.contains("lab.example.org") && !text.contains("Lab"), "{text}");
        assert!(json_out["machines"]["error"].as_str().unwrap().contains(said));
        assert_eq!(json_out["machines"]["machines"], json!([]));
        assert_eq!(std::fs::read_to_string(config.join("machines.json")).unwrap(), contents);
    }
}

#[test]
fn a_running_runtime_is_described_without_its_token() {
    let home = Home::new("running");
    let dir = home.path("state").join("elsewhere");
    // This test's own process is the live one the record names.
    let me = std::process::id();
    record(&dir, json!({ "pid": me, "port": answering(), "build": "0.1.0-aaaa", "folder": "/work/notebooks", "exits_when_idle": true }));
    let dir_arg = dir.to_str().unwrap();
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains(&format!("Running: pid {me}, port ")), "{text}");
    assert!(text.contains("started by another version of endeavor (build 0.1.0-aaaa; this is build "), "{text}");
    assert!(text.contains("Notebooks folder: /work/notebooks") && text.contains("Ends itself when idle: yes") && text.contains("Answers: yes"), "{text}");
    assert_eq!(json_out["runtime"]["state"], "running");
    assert_eq!(json_out["runtime"]["answers"], true);
    assert_eq!(json_out["runtime"]["other_build"], true);
    assert_eq!(json_out["runtime"]["exits_when_idle"], true);
    assert_eq!(json_out["runtime"]["pid"], me);

    record(&dir, json!({ "pid": me, "port": closed_port() }));
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Ends itself when idle: not recorded") && text.contains("Answers: no, though its process is alive"), "{text}");
    assert!(text.contains("(an earlier build; this is build "), "{text}");
    assert_eq!(json_out["runtime"]["answers"], false);
    assert_eq!(json_out["runtime"]["exits_when_idle"], Value::Null);

    record(&dir, json!({ "pid": me, "port": answering(), "node": "another-computer" }));
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Recorded by another-computer, not this computer"), "{text}");
    assert_eq!(json_out["runtime"]["state"], "other_computer");
    assert_eq!(json_out["runtime"]["answers"], Value::Null);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "only runtime.json");
}

#[test]
fn a_start_under_way_is_reported_beside_whatever_is_recorded() {
    let home = Home::new("starting");
    let dir = home.path("state").join("starting");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_arg = dir.to_str().unwrap();
    let lock = File::create(dir.join("starting.lock")).unwrap();
    lock.lock().unwrap();
    let runtime = |json_out: &Value| json_out["runtime"].clone();

    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Not running.") && text.contains("Start under way: yes"), "{text}");
    assert_eq!((runtime(&json_out)["state"].clone(), runtime(&json_out)["starting"].clone(), runtime(&json_out)["pid"].clone()), (json!("not_running"), json!(true), Value::Null));

    assert_eq!(runtime(&json_out)["starting_pid"], Value::Null, "no process is named in the file");

    // A core names itself in the file it holds.
    let core = std::process::id();
    let names = |pid: u32, node: &str| std::fs::write(dir.join("starting.lock"), json!({ "pid": pid, "started": null, "boot": null, "node": node }).to_string()).unwrap();
    names(core, &hostname());
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains(&format!("Start under way: yes (pid {core} holds starting.lock")), "{text}");
    assert_eq!(runtime(&json_out)["starting_pid"], json!(core));
    // A pid that is no process is not named.
    names(i32::MAX as u32, &hostname());
    assert_eq!(runtime(&home.both(&["--state-dir", dir_arg]).1)["starting_pid"], Value::Null);
    // A pid of another computer is not named either.
    names(core, "another-computer");
    assert_eq!(runtime(&home.both(&["--state-dir", dir_arg]).1)["starting_pid"], Value::Null);

    // A record whose process is gone is still named stale; the start is separate.
    record(&dir, json!({ "pid": i32::MAX, "port": 4321 }));
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Recorded, but its process is gone (pid 2147483647)") && text.contains("Start under way: yes"), "{text}");
    let r = runtime(&json_out);
    assert_eq!((&r["state"], &r["starting"], &r["pid"], &r["port"]), (&json!("stale"), &json!(true), &json!(i32::MAX), &Value::Null));

    // A runtime that is alive and silent, and another computer's record.
    record(&dir, json!({ "pid": std::process::id(), "port": closed_port() }));
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Answers: no") && text.contains("Start under way: yes"), "{text}");
    let r = runtime(&json_out);
    assert_eq!((&r["state"], &r["starting"], &r["answers"]), (&json!("running"), &json!(true), &json!(false)));

    record(&dir, json!({ "pid": std::process::id(), "port": closed_port(), "node": "another-computer" }));
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Recorded by another-computer") && text.contains("Start under way: yes"), "{text}");
    let r = runtime(&json_out);
    assert_eq!((&r["state"], &r["starting"], &r["pid"], &r["port"], &r["node"]), (&json!("other_computer"), &json!(true), &Value::Null, &Value::Null, &json!("another-computer")));
    drop(lock);

    std::fs::remove_file(dir.join("runtime.json")).unwrap();
    let (text, json_out) = home.both(&["--state-dir", dir_arg]);
    assert!(text.contains("Start under way: no"), "{text}");
    assert_eq!(json_out["runtime"]["starting"], false);

    let free = home.path("state").join("free");
    std::fs::create_dir_all(&free).unwrap();
    let (text, _) = home.both(&["--state-dir", free.to_str().unwrap()]);
    assert!(text.contains("Not running."), "{text}");
    assert_eq!(std::fs::read_dir(&free).unwrap().count(), 0, "no starting.lock is made");
}

#[test]
fn a_build_listed_by_a_link_is_not_a_build() {
    let home = Home::new("links");
    let bin = home.path("data").join("endeavor/bin");
    std::fs::create_dir_all(bin.join("0123456789ab")).unwrap();
    std::os::unix::fs::symlink(bin.join("0123456789ab"), bin.join("link")).unwrap();
    let (_, json_out) = home.both(&[]);
    assert_eq!(json_out["folders"]["plugin_binaries"]["builds"], json!(["0123456789ab"]));
}

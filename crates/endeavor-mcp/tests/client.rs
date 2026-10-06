//! The client library against the helper binary, with a local `sh` standing in
//! for ssh (`Transport::Shell`), and a `sleep` process with a small server on
//! its port standing in for the runtime, so no ssh and no Julia are needed:
//! installing the helper, reusing it, attaching, and each way a connection ends.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use endeavor_mcp::client::{Auth, Cancel, Event, Listener, Notice, Options, Running, Server, Transport, connect, connect_checked, no_helper, start};
use wire::files;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn helper_binary(_os: &str, _arch: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(env!("CARGO_BIN_EXE_endeavor")))
}

/// A fresh folder under target/tmp with `root`, `state` and `home` in it.
struct Place {
    root: PathBuf,
    state: PathBuf,
    home: PathBuf,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("client-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let place = Place { root: dir.join("root"), state: dir.join("state"), home: dir.join("home") };
        std::fs::create_dir_all(&place.home).unwrap();
        std::fs::create_dir_all(&place.state).unwrap();
        place
    }

    fn options(&self) -> Options<'static> {
        Options { auth: Auth::Batch, root: self.root.display().to_string(), state: self.state.display().to_string(), depot: String::new(), exit_idle: false, allow_install: true, helper: &helper_binary }
    }

    fn transport(&self) -> Transport {
        Transport::Shell { env: vec![("HOME".into(), self.home.display().to_string())], ask: None }
    }

    fn installed(&self) -> PathBuf {
        self.root.join(endeavor_mcp::embedded::BUILD_VERSION)
    }
}

/// The helpers running with `state` as their state folder: the one kind of process that has it in its arguments.
fn helper_pids(state: &Path) -> Vec<i32> {
    let found = Command::new("pgrep").arg("-f").arg("--").arg(state.display().to_string()).output().unwrap();
    String::from_utf8_lossy(&found.stdout).split_whitespace().filter_map(|p| p.parse().ok()).collect()
}

/// Wait for every helper of `state` to be gone.
fn no_helper_left(state: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !helper_pids(state).is_empty() {
        assert!(std::time::Instant::now() < deadline, "a helper for {} is still running", state.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn events() -> (Arc<Mutex<Vec<Event>>>, impl Fn(Event)) {
    let seen: Arc<Mutex<Vec<Event>>> = Arc::default();
    let s = seen.clone();
    (seen, move |e| s.lock().unwrap().push(e))
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into `buf`.
    unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// A runtime as the helper sees one: a live pid on this node, and a bridge that answers `ping`.
struct FakeRuntime {
    process: Arc<Mutex<std::process::Child>>,
}

impl FakeRuntime {
    fn start(state_dir: &Path) -> FakeRuntime {
        let process = Command::new("sleep").arg("600").process_group(0).spawn().unwrap();
        let bridge = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = bridge.local_addr().unwrap().port();
        let pid = process.id() as i32;
        let process = Arc::new(Mutex::new(process));
        let p = process.clone();
        std::thread::spawn(move || {
            for mut socket in bridge.incoming().map_while(Result::ok) {
                let p = p.clone();
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut buf = [0; 4096];
                    while let Ok(n) = socket.read(&mut buf) {
                        request.extend(&buf[..n]);
                        if n == 0 || String::from_utf8_lossy(&request).contains("\"params\"") {
                            break;
                        }
                    }
                    if String::from_utf8_lossy(&request).contains("endeavor/shutdown") {
                        let mut process = p.lock().unwrap();
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
                });
            }
        });
        std::fs::create_dir_all(state_dir).unwrap();
        let state = serde_json::json!({ "launcher": "process", "node": hostname(), "pid": pid, "port": port, "token": TOKEN });
        std::fs::write(state_dir.join("runtime.json"), state.to_string()).unwrap();
        FakeRuntime { process }
    }

    fn alive(&self) -> bool {
        self.process.lock().unwrap().try_wait().unwrap().is_none()
    }
}

impl Drop for FakeRuntime {
    fn drop(&mut self) {
        let mut process = self.process.lock().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }
}

/// The bridge's `ping` through a local port.
fn ping(port: u16, token: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    write!(socket, "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    reply.lines().next().unwrap_or_default().to_owned()
}

#[test]
fn the_helper_is_installed_then_reused_and_attaches() {
    let place = Place::new("bootstrap");
    let fake = FakeRuntime::start(&place.state);
    let server = Server { ssh_host: "lab".into(), ..Default::default() };
    let options = place.options();
    let transport = place.transport();

    let (seen, on) = events();
    let (channel, hello) = connect(&server, &transport, &options, &Cancel::default(), &on).expect("first connect");
    assert_eq!((hello.node.as_str(), hello.home.as_path()), (hostname().as_str(), place.home.as_path()));
    let installed = place.installed();
    assert!(installed.join("endeavor").is_file() && installed.join("runtime/boot.jl").is_file());

    // Its files before any runtime: a folder under the (fake) home.
    std::fs::create_dir_all(place.home.join("decay-fits")).unwrap();
    std::fs::write(place.home.join("decay-fits/fit.jl"), "### A Pluto.jl notebook ###\n").unwrap();
    let listed = channel.files(files::Request::List { path: "~".into() }).expect("list");
    assert!(matches!(&listed, files::Reply::List { entries, .. } if entries.iter().any(|e| e.name == "decay-fits" && e.dir)), "{listed:?}");
    let found = channel.files(files::Request::Notebooks { path: "~/decay-fits".into() }).expect("scan");
    assert!(matches!(&found, files::Reply::Notebooks { found } if found.len() == 1), "{found:?}");
    assert!(channel.files(files::Request::Preview { path: "~/nope.jl".into() }).is_err());

    // Started on request, reachable through a listener.
    let listener = Listener::start("test").unwrap();
    let runtime = start(&channel, &listener, None, &on, |_| {}).expect("start");
    assert!(runtime.reattached);
    assert_eq!((runtime.token.as_str(), runtime.node.as_str()), (TOKEN, hostname().as_str()));
    assert_eq!((runtime.port, runtime.page_url.as_str()), (listener.port(), format!("http://127.0.0.1:{}/?token={TOKEN}", listener.port()).as_str()));
    assert_eq!(runtime.mcp_url, format!("http://127.0.0.1:{}/mcp", listener.port()));
    let seen = seen.lock().unwrap().clone();
    let uname = if cfg!(target_os = "macos") { "Darwin" } else { "Linux" };
    assert!(matches!(&seen[0], Event::Connected { os, .. } if os == uname), "{seen:?}");
    assert_eq!(seen[1], Event::Helper { installed: true });
    assert!(matches!(&seen[2], Event::Started { reattached: true, .. }), "{seen:?}");
    assert_eq!(ping(listener.port(), TOKEN), "HTTP/1.1 200 OK");
    channel.detach();
    assert!(fake.alive(), "detaching leaves it running");

    let (seen, on) = events();
    let (channel, _) = connect(&server, &transport, &options, &Cancel::default(), &on).expect("second connect");
    assert_eq!(seen.lock().unwrap()[1], Event::Helper { installed: false });
    start(&channel, &listener, None, &on, |_| {}).expect("start again");
    channel.stop().expect("stop");
    assert!(!fake.alive(), "Stop reaches the runtime's bridge");
    // The helper stays connected after a stop.
    assert!(channel.files(files::Request::List { path: "~".into() }).is_ok());
    channel.detach();
}

#[test]
fn a_relative_state_folder_is_under_the_install_folder() {
    let place = Place::new("state-relative");
    std::fs::create_dir_all(place.root.join("state-rel")).unwrap();
    let fake = FakeRuntime::start(&place.root.join("state-rel"));
    let options = Options { state: "state-rel".into(), ..place.options() };
    let (channel, _) = connect(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).expect("connect");
    let runtime = start(&channel, &Listener::start("test").unwrap(), None, &|_| {}, |_| {}).expect("attached to the runtime recorded there");
    assert!(runtime.reattached);
    channel.detach();
    assert!(fake.alive());
}

#[test]
fn a_helper_that_ends_with_no_julia_is_a_drop_and_a_detach_is_not() {
    let place = Place::new("closed");
    let server = Server::default();
    let transport = place.transport();
    let options = place.options();
    let (channel, _) = connect(&server, &transport, &options, &Cancel::default(), &|_| {}).expect("connect");
    let channel = Arc::new(channel);
    let (heard_tx, heard) = mpsc::channel();
    std::thread::spawn({
        let channel = channel.clone();
        move || heard_tx.send(channel.closed()).unwrap()
    });
    assert!(heard.recv_timeout(Duration::from_millis(500)).is_err(), "nothing while it's up");
    let pids = helper_pids(&place.state);
    assert!(!pids.is_empty(), "the helper was running");
    for pid in pids {
        // SAFETY: plain syscall, on the helper this test started.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let notice = heard.recv_timeout(Duration::from_secs(10)).expect("its end is heard");
    assert!(matches!(&notice, Some(Notice::Lost(reason)) if reason == "The connection closed unexpectedly."), "{notice:?}");
    assert!(channel.files(files::Request::List { path: "~".into() }).is_err());

    let (channel, _) = connect(&server, &transport, &options, &Cancel::default(), &|_| {}).expect("connect again");
    channel.detach();
    assert!(channel.closed().is_none(), "the client let it go");
}

#[test]
fn a_server_without_a_helper_build_is_refused_plainly() {
    let place = Place::new("platform");
    let bin = place.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let uname = bin.join("uname");
    std::fs::write(&uname, "#!/bin/sh\n[ \"$1\" = -s ] && echo Plan9 || echo arm64\n").unwrap();
    std::fs::set_permissions(&uname, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let transport = Transport::Shell { env: vec![("HOME".into(), place.home.display().to_string()), ("PATH".into(), path)], ask: None };
    let asked: Mutex<Vec<(String, String)>> = Mutex::default();
    let helper = |os: &str, arch: &str| {
        asked.lock().unwrap().push((os.to_owned(), arch.to_owned()));
        Err(no_helper(os, arch))
    };
    let options = Options { helper: &helper, ..place.options() };
    let err = connect(&Server::default(), &transport, &options, &Cancel::default(), &|_| {}).err().expect("refused");
    assert_eq!(err, "Endeavor has no runtime helper for plan9 aarch64 servers.");
    assert_eq!(*asked.lock().unwrap(), [("plan9".to_owned(), "aarch64".to_owned())]);
    assert!(!place.installed().exists());
}

#[test]
fn a_helper_that_cannot_be_read_is_named() {
    let place = Place::new("unreadable");
    let missing = |_: &str, _: &str| Ok(PathBuf::from("/no/such/endeavor"));
    let options = Options { helper: &missing, ..place.options() };
    let err = connect(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().expect("refused");
    assert_eq!(err, "/no/such/endeavor: No such file or directory (os error 2)");
}

#[test]
fn cancelling_ends_a_connect_that_waits_on_ssh() {
    let place = Place::new("cancel");
    // A sign-in that never finishes.
    let transport = Transport::Shell { env: Vec::new(), ask: Some("sleep 30".into()) };
    let cancel = Arc::new(Cancel::default());
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn({
        let (cancel, options) = (cancel.clone(), place.options());
        move || done_tx.send(connect(&Server::default(), &transport, &options, &cancel, &|_| {}).err()).unwrap()
    });
    assert!(done.recv_timeout(Duration::from_millis(500)).is_err(), "it waits");
    cancel.cancel();
    assert_eq!(done.recv_timeout(Duration::from_secs(10)).expect("connect returns").as_deref(), Some("Cancelled."));
}

#[test]
fn a_connect_cancelled_before_it_starts_runs_nothing() {
    let place = Place::new("cancelled-first");
    let transport = Transport::Shell { env: Vec::new(), ask: Some("sleep 30".into()) };
    let cancel = Cancel::default();
    cancel.cancel();
    let err = connect(&Server::default(), &transport, &place.options(), &cancel, &|_| {}).err().expect("cancelled");
    assert_eq!(err, "Cancelled.");
}

#[test]
fn a_start_that_fails_leaves_no_helper_behind() {
    let place = Place::new("leak-start");
    let server = Server { julia: Some("/no/such/julia".into()), ..Default::default() };
    let (channel, _) = connect(&server, &place.transport(), &place.options(), &Cancel::default(), &|_| {}).expect("connect");
    assert!(!helper_pids(&place.state).is_empty());
    let err = start(&channel, &Listener::start("test").unwrap(), None, &|_| {}, |_| {}).expect_err("no Julia there");
    assert!(!err.is_empty());
    drop(channel);
    no_helper_left(&place.state);
}

#[test]
fn a_connect_that_fails_after_the_helper_started_leaves_no_helper_behind() {
    let place = Place::new("leak-hello");
    let frames = place.root.with_file_name("frames");
    std::fs::create_dir_all(frames.parent().unwrap()).unwrap();
    std::fs::write(&frames, wire::ToApp::Error { message: "no hello for you".into() }.frame().encode()).unwrap();
    // Says an error instead of hello, then stays until the client sends it anything (a detach).
    let script = place.root.with_file_name("fake-helper");
    std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\nhead -c 1 >/dev/null\n", frames.display())).unwrap();
    let fake = |_: &str, _: &str| Ok(script.clone());
    let options = Options { helper: &fake, ..place.options() };
    let err = connect(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().expect("no hello");
    assert_eq!(err, "no hello for you");
    no_helper_left(&place.state);
}

#[test]
fn a_failed_install_is_reported_as_one_not_as_a_refused_sign_in() {
    let place = Place::new("unwritable");
    let shut = place.root.with_file_name("shut");
    std::fs::create_dir_all(&shut).unwrap();
    std::fs::set_permissions(&shut, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    // SAFETY: plain syscall.
    if unsafe { libc::geteuid() } == 0 {
        return eprintln!("skipped: root can write anywhere");
    }
    let options = Options { root: shut.join("root").display().to_string(), ..place.options() };
    let err = connect(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().expect("can't install");
    assert!(err.contains("installing into") && err.contains("failed") && !err.contains("refused the sign-in"), "{err}");
}

#[test]
fn text_that_is_not_utf8_before_the_bootstrap_line_is_skipped() {
    let place = Place::new("banner");
    let transport = Transport::Shell { env: vec![("HOME".into(), place.home.display().to_string())], ask: Some(r"printf 'caf\351 banner\n'".into()) };
    let (channel, _) = connect(&Server::default(), &transport, &place.options(), &Cancel::default(), &|_| {}).expect("connect");
    channel.detach();
}

#[test]
fn cancelling_after_the_connect_is_over_ends_the_connection_while_it_lasts() {
    let place = Place::new("cancel-late");
    let cancel = Cancel::default();
    let (channel, _) = connect(&Server::default(), &place.transport(), &place.options(), &cancel, &|_| {}).expect("connect");
    assert!(channel.files(files::Request::List { path: "~".into() }).is_ok());
    cancel.cancel();
    let notice = channel.closed();
    assert!(matches!(&notice, Some(Notice::Lost(_))), "{notice:?}");
    no_helper_left(&place.state);
}

#[test]
fn cancelling_a_test_while_its_start_waits_ends_it_at_once() {
    let place = Place::new("cancel-test-start");
    // Another start holds the lock, so the helper waits for it to say anything about Julia.
    let held = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(place.state.join("start.lock")).unwrap();
    // SAFETY: plain syscall on a file we hold open.
    assert_eq!(unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let cancel = Arc::new(Cancel::default());
    let (seen, on) = events();
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn({
        let (cancel, options, transport) = (cancel.clone(), place.options(), place.transport());
        move || done_tx.send(endeavor_mcp::client::test(&Server::default(), &transport, &options, &cancel, &on)).unwrap()
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !seen.lock().unwrap().iter().any(|e| matches!(e, Event::Progress(line) if line.contains("waiting for it"))) {
        assert!(std::time::Instant::now() < deadline, "the start never began waiting: {:?}", seen.lock().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "it waits for the start");
    cancel.cancel();
    assert_eq!(done.recv_timeout(Duration::from_secs(10)).expect("test returns"), Err("Cancelled.".to_owned()));
    no_helper_left(&place.state);
    drop(held);
}

#[test]
fn a_line_that_is_not_utf8_on_ssh_stderr_does_not_hide_the_reason_after_it() {
    let place = Place::new("stderr-latin1");
    let ask = r"printf 'caf\351 banner\n' >&2; echo 'jc@lab: Permission denied (publickey).' >&2; exit 255";
    let transport = Transport::Shell { env: Vec::new(), ask: Some(ask.into()) };
    let err = connect(&Server::default(), &transport, &place.options(), &Cancel::default(), &|_| {}).err().expect("refused");
    assert!(err.contains("refused the sign-in") && err.contains("ssh-add"), "{err}");
}

/// Everything under `dir`, as paths.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        found.push(entry.path());
        if entry.path().is_dir() {
            found.extend(tree(&entry.path()));
        }
    }
    found
}

#[test]
fn without_permission_a_server_that_lacks_the_helper_is_only_looked_at() {
    let place = Place::new("no-install");
    let options = Options { allow_install: false, ..place.options() };
    let (seen, on) = events();
    let err = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &on).err().expect("no connection");
    let needs = err.needs.expect("it says what it needs");
    assert!(!err.retry);
    assert_eq!(needs.folder, place.installed().display().to_string());
    assert!(needs.bytes > 10_000 && !needs.update && needs.running.is_none(), "{needs:?}");
    assert_eq!((needs.os.as_str(), needs.arch.as_str()), (String::from_utf8(Command::new("uname").arg("-s").output().unwrap().stdout).unwrap().trim(), String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout).unwrap().trim()));
    assert!(err.message.contains("isn't installed") && err.message.contains("wasn't allowed"), "{}", err.message);
    assert!(matches!(seen.lock().unwrap().as_slice(), [Event::Connected { .. }]), "it never says the helper is there: {:?}", seen.lock().unwrap());
    assert!(!place.root.exists() && !place.home.join(".cache").exists(), "nothing was written on the server: {:?}", tree(&place.home));
    no_helper_left(&place.state);
    // `connect` gives the message alone.
    let message = connect(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().expect("no connection");
    assert_eq!(message, err.message);
}

#[test]
fn a_runtime_recorded_and_alive_there_is_reported_without_the_helper() {
    let place = Place::new("no-install-running");
    let runtime = FakeRuntime::start(&place.state);
    let pid = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(place.state.join("runtime.json")).unwrap()).unwrap()["pid"].as_u64().unwrap() as u32;
    let options = Options { allow_install: false, ..place.options() };
    let needs = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert_eq!(needs.running, Some(Running::Process(pid)));
    // A record whose process is gone isn't reported.
    drop(runtime);
    wait_gone(pid);
    let needs = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert_eq!(needs.running, None);
    assert!(!place.root.exists());
}

fn wait_gone(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    // SAFETY: signal 0 only checks the process.
    while unsafe { libc::kill(pid as i32, 0) } == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_slurm_job_recorded_in_the_cluster_folder_is_reported() {
    let place = Place::new("no-install-job");
    std::fs::write(place.state.join("job.json"), r#"{"job":"4242","summary":"8 CPUs"}"#).unwrap();
    let cluster = Server { id: "hpc".into(), cluster: Some(Default::default()), ..Default::default() };
    let options = Options { allow_install: false, ..place.options() };
    let needs = connect_checked(&cluster, &place.transport(), &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert_eq!(needs.running, Some(Running::Job("4242".into())));
    // A plain server's folder isn't looked at for jobs.
    let needs = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert_eq!(needs.running, None);
}

#[test]
fn the_default_state_folders_are_looked_in_when_none_is_given() {
    let place = Place::new("no-install-default");
    let state_home = place.home.join("xdg-state");
    let node = hostname();
    let (process, cluster) = (state_home.join("endeavor/serve").join(&node), state_home.join("endeavor/cluster"));
    std::fs::create_dir_all(&process).unwrap();
    std::fs::create_dir_all(&cluster).unwrap();
    std::fs::write(process.join("runtime.json"), serde_json::json!({ "launcher": "process", "node": node, "pid": std::process::id(), "token": "t" }).to_string()).unwrap();
    std::fs::write(cluster.join("runtime.json"), serde_json::json!({ "launcher": "slurm", "node": "n1", "pid": 7, "job": "99", "token": "t" }).to_string()).unwrap();
    let transport = Transport::Shell { env: vec![("HOME".into(), place.home.display().to_string()), ("XDG_STATE_HOME".into(), state_home.display().to_string())], ask: None };
    let options = Options { allow_install: false, state: String::new(), ..place.options() };
    let needs = |server: &Server| connect_checked(server, &transport, &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert_eq!(needs(&Server::default()).running, Some(Running::Process(std::process::id())));
    assert_eq!(needs(&Server { id: "hpc".into(), cluster: Some(Default::default()), ..Default::default() }).running, Some(Running::Job("99".into())));
}

#[test]
fn an_older_helper_makes_it_an_update() {
    let place = Place::new("no-install-older");
    std::fs::create_dir_all(place.root.join("0.0.1-old")).unwrap();
    std::fs::write(place.root.join("0.0.1-old/endeavor"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(place.root.join("0.0.1-old/endeavor"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let options = Options { allow_install: false, ..place.options() };
    let needs = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &|_| {}).err().unwrap().needs.unwrap();
    assert!(needs.update);
    assert!(!place.installed().exists());
}

#[test]
fn with_permission_it_installs_as_before_and_a_helper_already_there_needs_none() {
    let place = Place::new("install-allowed");
    let (seen, on) = events();
    let (channel, _) = connect_checked(&Server::default(), &place.transport(), &place.options(), &Cancel::default(), &on).expect("connects");
    assert!(seen.lock().unwrap().contains(&Event::Helper { installed: true }));
    assert!(place.installed().join("endeavor").exists());
    channel.detach();
    no_helper_left(&place.state);
    // Without permission, with the helper there: it connects.
    let (seen, on) = events();
    let options = Options { allow_install: false, ..place.options() };
    let (channel, _) = connect_checked(&Server::default(), &place.transport(), &options, &Cancel::default(), &on).expect("connects");
    assert!(seen.lock().unwrap().contains(&Event::Helper { installed: false }));
    channel.detach();
    no_helper_left(&place.state);
}

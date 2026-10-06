//! Reaching a runtime on a server over SSH: one system `ssh` runs a short
//! bootstrap script that installs the helper and `runtime/` if this build's
//! aren't there yet, then becomes `endeavor connect`, whose frames use the
//! rest of ssh's stdin and stdout. From there on it's the same channel as a
//! helper on this computer's (`Channel`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use wire::ToApp;
use wire::slurm::JobRequest;

use super::channel::{Channel, Hello, Notice, Runtime};
use super::listener::Listener;
use super::machines::Server;

/// What happened so far while connecting.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The bootstrap script answered: ssh is in.
    Connected { os: String, arch: String },
    /// The helper and runtime for this build were just installed (or already there).
    Helper { installed: bool },
    FoundJulia { path: String, version: String },
    /// A line of Julia's download or the runtime's boot log.
    Progress(String),
    /// A cluster job for Julia was submitted.
    Submitted { job: String, summary: String },
    /// It waits in the queue (Slurm's state and reason).
    Queued { state: String, reason: String },
    /// The runtime is up (or was already running) and its bridge answered through the listener.
    Started { node: String, reattached: bool },
    /// `test` on a cluster: what Slurm says there.
    Slurm(wire::slurm::Scheduler),
    /// `test` is done with it: stopped, or left running as it was found.
    Finished { stopped: bool },
}

/// How ssh signs in.
#[derive(Clone, Debug, PartialEq)]
pub enum Auth {
    /// `-o BatchMode=yes`: ssh fails instead of asking anything. Keys, the
    /// agent and `~/.ssh/config` do the signing in.
    Batch,
    /// No batch mode, and these variables set on ssh: for a caller that answers
    /// ssh's prompts itself (through `SSH_ASKPASS`).
    Env(Vec<(String, String)>),
}

/// How the bootstrap script reaches the server.
#[derive(Clone, Debug)]
pub enum Transport {
    Ssh { host: String, port: Option<u16> },
    /// The script through a local `sh`, parsed the way a login shell on the
    /// server parses ssh's command: a stand-in for ssh in tests. `ask` is a
    /// shell line that runs first and has to succeed, as ssh's prompts and
    /// refusals come before the script.
    Shell { env: Vec<(String, String)>, ask: Option<String> },
}

impl Transport {
    pub fn for_server(server: &Server) -> Transport {
        Transport::Ssh { host: server.ssh_host.clone(), port: server.port }
    }

    fn command(&self, script: &str, auth: &Auth) -> Result<Command, String> {
        let remote = format!("sh -c '{script}'");
        let mut command = match self {
            Transport::Ssh { host, port } => {
                valid_host(host)?;
                let mut command = Command::new("ssh");
                // A keepalive every 10 s of silence, and ssh quits after two go
                // unanswered: a dead link ends the helper's channel within
                // about 30 s, even while nothing else is sent.
                command.args(["-T", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=2", "-o", "ConnectTimeout=20", "-o", "ForwardX11=no"]);
                if *auth == Auth::Batch {
                    command.args(["-o", "BatchMode=yes"]);
                }
                if let Some(port) = port {
                    command.arg("-p").arg(port.to_string());
                }
                command.arg("--").arg(host).arg(remote);
                command
            }
            Transport::Shell { env, ask } => {
                let mut command = Command::new("sh");
                let first = ask.as_ref().map(|a| format!("{a} && ")).unwrap_or_default();
                command.arg("-c").arg(format!("{first}{remote}")).envs(env.iter().map(|(k, v)| (k, v)));
                command
            }
        };
        if let Auth::Env(env) = auth {
            command.envs(env.iter().map(|(k, v)| (k, v)));
        }
        Ok(command)
    }

    fn host(&self) -> &str {
        match self {
            Transport::Ssh { host, .. } => host,
            Transport::Shell { .. } => "the local shell",
        }
    }

    /// What to type in a terminal to sign in to the server by hand.
    fn login_command(&self) -> String {
        match self {
            Transport::Ssh { host, port: Some(port) } => format!("ssh -p {port} {host}"),
            Transport::Ssh { host, port: None } => format!("ssh {host}"),
            Transport::Shell { .. } => "ssh".into(),
        }
    }
}

/// Whether `host` can be given to ssh as the destination: an alias, a name,
/// an address (IPv6 has colons) or `user@host`. A leading `-` would be an
/// option to ssh, and anything else is not a host name.
pub fn valid_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("Enter an SSH host: an alias from ~/.ssh/config, or user@host.".into());
    }
    if host.starts_with('-') || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':')) {
        return Err(format!("\"{host}\" isn't an SSH host name."));
    }
    Ok(())
}

/// What `connect` needs besides the server.
pub struct Options<'a> {
    pub auth: Auth,
    /// Where the helper and `runtime/` are installed on the server, in a
    /// folder named for this build. Empty is `~/.cache/endeavor`.
    pub root: String,
    /// The runtime's state folder on the server: an absolute path, or one
    /// relative to `root`. `Server::launcher` has the app's name for it.
    /// Empty is the helper's own default, the folder `serve` and `mcp` use.
    pub state: String,
    /// Julia's `JULIA_DEPOT_PATH` on the server. Empty is `<root>/depot:`.
    pub depot: String,
    /// The helper binary to send to a server whose `uname -s` is `os` and
    /// `uname -m` is `arch`, as `linux` and `x86_64` (`arm64` as `aarch64`).
    /// Asked only when the server has no helper of this build yet.
    pub helper: &'a (dyn Fn(&str, &str) -> Result<PathBuf, String> + Sync),
}

/// Why a server can't be set up when `Options::helper` has no binary for it.
pub fn no_helper(os: &str, arch: &str) -> String {
    format!("Endeavor has no runtime helper for {os} {arch} servers.")
}

/// The script ssh runs on the server, as one line: the server's login shell
/// (sh, bash, zsh, fish or csh) gets it inside single quotes, so it holds no
/// quote, backslash, `!` or newline.
///
/// It reads six lines (the install root, the state folder, the depot, the
/// helper's Julia flag and its value, and its launcher; see `Options` for the
/// empty ones), prints `ENDEAVOR <os> <arch> <have|need>`, and if it needs the
/// install, reads a byte count and then that many bytes of tar. Then it
/// becomes the helper. The values come over stdin and not in the script, so
/// no path can break it.
pub fn bootstrap_script(version: &str) -> String {
    [
        &format!("v={version}"),
        r#"read -r rt && read -r st && read -r dp && read -r jf && read -r jv && read -r ln || exit 1"#,
        r#"c="${rt:-$HOME/.cache/endeavor}""#,
        r#"d="$c/$v""#,
        r#"set --; if [ -n "$st" ]; then case "$st" in /*) sd="$st";; *) sd="$c/$st";; esac; set -- --state-dir "$sd"; fi"#,
        r#"[ -n "$dp" ] || dp="$c/depot:""#,
        r#"if [ -x "$d/endeavor" ] && [ -f "$d/runtime/boot.jl" ]; then s=have; else s=need; fi"#,
        r#"echo "ENDEAVOR $(uname -s) $(uname -m) $s""#,
        r#"if [ $s = need ]; then read -r n || exit 1; t="$d.part.$$"; rm -rf "$t"; mkdir -p "$t" && head -c "$n" | (cd "$t" && tar xf -) || { rm -rf "$t"; echo "Endeavor: installing into $d failed" >&2; exit 1; }; rm -rf "$d"; mv "$t" "$d"; fi"#,
        r#"exec "$d/endeavor" connect "$@" --launcher "$ln" "$jf" "$jv" --runtime "$d/runtime" --depot "$dp" --build "$v""#,
    ]
    .join("; ")
}

/// The six lines the script reads first.
fn preamble(server: &Server, options: &Options) -> Result<Vec<u8>, String> {
    let [flag, value] = server.julia_args();
    let [launcher, _] = server.launcher();
    let lines = [("install folder", &options.root), ("state folder", &options.state), ("depot", &options.depot), ("Julia setting", &flag), ("Julia setting", &value), ("launcher", &launcher)];
    for (what, line) in lines {
        if line.contains('\n') {
            return Err(format!("The {what} can't hold a line break."));
        }
    }
    Ok(lines.iter().map(|(_, line)| format!("{line}\n")).collect::<String>().into_bytes())
}

/// The files of `runtime/`, built into this crate, as (path in the tar,
/// contents, executable), sorted.
fn runtime_files() -> Vec<(String, Vec<u8>, bool)> {
    crate::embedded::RUNTIME_FILES.iter().map(|(path, contents)| ((*path).to_owned(), contents.to_vec(), false)).collect()
}

/// A tar stream (ustar) of the helper as `endeavor` plus `runtime/`.
fn install_tar(helper: &Path) -> Result<Vec<u8>, String> {
    let helper_bytes = std::fs::read(helper).map_err(|e| format!("{}: {e}", helper.display()))?;
    let mut entries = vec![("endeavor".to_owned(), helper_bytes, true)];
    entries.extend(runtime_files());
    let mut tar = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for (path, contents, executable) in &entries {
        // Parent folders first, each once.
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            let dir = &path[..at + slash];
            if !dirs.iter().any(|d| d == dir) {
                dirs.push(dir.to_owned());
                tar_entry(&mut tar, &format!("{dir}/"), b"", 0o755, b'5')?;
            }
            at += slash + 1;
        }
        tar_entry(&mut tar, path, contents, if *executable { 0o755 } else { 0o644 }, b'0')?;
    }
    tar.extend([0; 1024]);
    Ok(tar)
}

fn tar_entry(tar: &mut Vec<u8>, path: &str, contents: &[u8], mode: u32, kind: u8) -> Result<(), String> {
    let mut header = [0u8; 512];
    let (prefix, name) = match path.len() {
        0..=100 => ("", path),
        _ => {
            let split = path[..path.len().min(156)].rfind('/').filter(|&i| path.len() - i - 1 <= 100);
            let split = split.ok_or_else(|| format!("{path} is too long to install"))?;
            (&path[..split], &path[split + 1..])
        }
    };
    let field = |header: &mut [u8; 512], at: usize, bytes: &[u8]| header[at..at + bytes.len()].copy_from_slice(bytes);
    field(&mut header, 0, name.as_bytes());
    field(&mut header, 100, format!("{mode:07o}\0").as_bytes());
    field(&mut header, 108, b"0000000\0");
    field(&mut header, 116, b"0000000\0");
    field(&mut header, 124, format!("{:011o}\0", contents.len()).as_bytes());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    field(&mut header, 136, format!("{now:011o}\0").as_bytes());
    field(&mut header, 148, b"        ");
    header[156] = kind;
    field(&mut header, 257, b"ustar\0");
    field(&mut header, 263, b"00");
    field(&mut header, 345, prefix.as_bytes());
    let sum: u32 = header.iter().map(|&b| b as u32).sum();
    field(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());
    tar.extend(header);
    tar.extend(contents);
    tar.resize(tar.len().div_ceil(512) * 512, 0);
    Ok(())
}

/// Lets another thread give up on a connect, killing its ssh.
#[derive(Default)]
pub struct Cancel {
    pid: Mutex<Option<u32>>,
    cancelled: AtomicBool,
}

impl Cancel {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap() {
            kill_group(pid);
        }
    }

    fn started(&self, pid: u32) -> bool {
        *self.pid.lock().unwrap() = Some(pid);
        !self.cancelled.load(Ordering::SeqCst)
    }
}

/// ssh runs in its own process group, which ends with it: the askpass it may
/// be waiting on, or the shells of `Transport::Shell`.
#[cfg(unix)]
fn kill_group(pid: u32) {
    // SAFETY: plain syscall.
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
}

/// The child is moved into the channel's thread, so only its pid is left.
#[cfg(windows)]
fn kill_group(pid: u32) {
    let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// `uname`'s words as the helper's folders name platforms: `linux` and `aarch64`.
fn platform(os: &str, arch: &str) -> (String, String) {
    (os.to_lowercase(), if arch == "arm64" { "aarch64" } else { arch }.to_owned())
}

/// Run the bootstrap on `server` and wait for the helper's hello. The runtime
/// starts later, when the channel is asked to (`start`).
pub fn connect(server: &Server, transport: &Transport, options: &Options, cancel: &Cancel, on: &dyn Fn(Event)) -> Result<(Channel, Hello), String> {
    let preamble = preamble(server, options)?;
    let version = crate::embedded::BUILD_VERSION;
    let mut command = transport.command(&bootstrap_script(version), &options.auth)?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|e| format!("Couldn't run ssh: {e}"))?;
    if !cancel.started(child.id()) {
        kill_group(child.id());
    }
    let stderr = Stderr::collect(child.stderr.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let host = transport.host();
    let give_up = |mut child: Child, why: Option<String>| {
        kill_group(child.id());
        let status = child.wait().ok();
        why.unwrap_or_else(|| explain(transport, &options.auth, &stderr.finish(), status, cancel.cancelled.load(Ordering::SeqCst)))
    };

    // The script reads these before it says anything; a failed write shows as the script's end.
    let _ = stdin.write_all(&preamble).and_then(|_| stdin.flush());

    // Login scripts may print things before the script's own line.
    let (os, arch, have) = loop {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) | Err(_) => return Err(give_up(child, None)),
            Ok(_) => {
                let words: Vec<&str> = line.split_whitespace().collect();
                if let ["ENDEAVOR", os, arch, have @ ("have" | "need")] = words[..] {
                    break (os.to_owned(), arch.to_owned(), have == "have");
                }
                eprintln!("{host}: {}", line.trim_end());
            }
        }
    };
    on(Event::Connected { os: os.clone(), arch: arch.clone() });

    let install = match have {
        true => None,
        false => {
            let (os, arch) = platform(&os, &arch);
            match (options.helper)(&os, &arch).and_then(|helper| install_tar(&helper)) {
                Ok(tar) => Some(tar),
                Err(e) => return Err(give_up(child, Some(e))),
            }
        }
    };
    if let Some(tar) = &install {
        let mut send = format!("{}\n", tar.len()).into_bytes();
        send.extend(tar);
        if stdin.write_all(&send).and_then(|_| stdin.flush()).is_err() {
            return Err(give_up(child, None));
        }
    }
    on(Event::Helper { installed: install.is_some() });

    let channel = Channel::open(child, stdin, stdout);
    let hello = channel.wait_hello(|| explain(transport, &options.auth, &stderr.finish(), None, cancel.cancelled.load(Ordering::SeqCst)))?;
    Ok((channel, hello))
}

/// Start the runtime on a connected server's channel (on a cluster, `job` is
/// what to submit); `on` hears Julia being found, the job queueing, and its
/// log. `notice` hears if the runtime goes away later.
pub fn start(channel: &Channel, listener: &Arc<Listener>, job: Option<JobRequest>, on: &dyn Fn(Event), notice: impl FnOnce(Notice) + Send + 'static) -> Result<Runtime, String> {
    let runtime = channel.start_runtime(
        listener,
        job,
        &mut |message| match message {
            ToApp::Progress { line } => on(Event::Progress(line)),
            ToApp::FoundJulia { path, version } => on(Event::FoundJulia { path, version }),
            ToApp::Submitted { job, summary } => on(Event::Submitted { job, summary }),
            ToApp::Queued { state, reason, .. } => on(Event::Queued { state, reason }),
            _ => {}
        },
        notice,
    )?;
    on(Event::Started { node: runtime.node.clone(), reattached: runtime.reattached });
    Ok(runtime)
}

/// Test connection: connect, check the runtime answers through a listener,
/// then stop it (or leave it running if it already was). On a cluster, only
/// ask Slurm about itself: starting Julia there means a job.
pub fn test(server: &Server, transport: &Transport, options: &Options, cancel: &Cancel, on: &dyn Fn(Event)) -> Result<(), String> {
    let (channel, _) = connect(server, transport, options, cancel, on)?;
    if server.cluster.is_some() {
        let reply = channel.files(wire::files::Request::Slurm);
        channel.detach();
        return match reply? {
            wire::files::Reply::Slurm { scheduler } => {
                on(Event::Slurm(scheduler));
                Ok(())
            }
            other => Err(format!("The helper answered {other:?}.")),
        };
    }
    let listener = test_listener()?;
    let runtime = start(&channel, &listener, None, on, |_| {})?;
    let answered = bridge_ping(listener.port(), &runtime.token);
    if runtime.reattached {
        channel.detach();
    } else {
        channel.stop();
        channel.detach();
    }
    answered.map_err(|e| format!("Julia started on {}, but it didn't answer through Endeavor's connection ({e}).", runtime.node))?;
    on(Event::Finished { stopped: !runtime.reattached });
    Ok(())
}

/// One listener for every `test` (they run one at a time).
fn test_listener() -> Result<Arc<Listener>, String> {
    static LISTENER: OnceLock<Arc<Listener>> = OnceLock::new();
    if let Some(listener) = LISTENER.get() {
        return Ok(listener.clone());
    }
    let listener = Listener::start("the server")?;
    Ok(LISTENER.get_or_init(|| listener).clone())
}

/// The bridge's `ping`, through a local port.
fn bridge_ping(port: u16, token: &str) -> Result<(), String> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_secs(20))).map_err(|e| e.to_string())?;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    write!(
        socket,
        "POST /endeavor/call HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut status = String::new();
    BufReader::new(socket).read_line(&mut status).map_err(|e| e.to_string())?;
    match status.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        Some(code) => Err(format!("HTTP {code}")),
        None => Err("no answer".into()),
    }
}

/// ssh's stderr: logged, and its last lines kept to say what went wrong.
struct Stderr {
    lines: Arc<Mutex<Vec<String>>>,
    done: Mutex<mpsc::Receiver<()>>,
}

impl Stderr {
    fn collect(stderr: impl Read + Send + 'static) -> Stderr {
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn({
            let lines = lines.clone();
            move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("ssh: {line}");
                    let mut lines = lines.lock().unwrap();
                    lines.push(line);
                    let excess = lines.len().saturating_sub(40);
                    lines.drain(..excess);
                }
                let _ = done_tx.send(());
            }
        });
        Stderr { lines, done: Mutex::new(done) }
    }

    /// Everything it said, once it's done (or after a short wait).
    fn finish(&self) -> Vec<String> {
        let _ = self.done.lock().unwrap().recv_timeout(Duration::from_secs(2));
        self.lines.lock().unwrap().clone()
    }
}

/// What went wrong, in plain words, from ssh's stderr. In batch mode ssh can't
/// ask, so the sign-in failures say what to do instead.
fn explain(transport: &Transport, auth: &Auth, stderr: &[String], status: Option<ExitStatus>, cancelled: bool) -> String {
    if cancelled {
        return "Cancelled.".into();
    }
    let host = transport.host();
    let batch = *auth == Auth::Batch;
    let said = |needle: &str| stderr.iter().any(|l| l.contains(needle));
    let last = stderr.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty());
    if said("Could not resolve hostname") {
        format!("Couldn't find a server called {host}. Check the SSH host.")
    } else if said("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        format!("{host}'s identity (host key) changed since the last connection. If the server was reinstalled, remove its old key with `ssh-keygen -R {host}` in a terminal; otherwise ask its administrator.")
    } else if said("Host key verification failed") && batch {
        format!("{host} isn't one of the servers this computer has connected to before, so Endeavor can't check its identity (host key). Run `{}` once in a terminal and accept its host key, then try again.", transport.login_command())
    } else if said("Host key verification failed") {
        format!("{host}'s identity (host key) wasn't confirmed, so Endeavor didn't connect.")
    } else if said("Permission denied") && batch {
        format!("{host} refused the sign-in. Either your key isn't accepted there, or it has a passphrase and isn't in your ssh agent (run `ssh-add` in a terminal to add it). Or the server asks for a password or a code, which Endeavor can't ask for yet.")
    } else if said("Permission denied") {
        format!("{host} refused the sign-in. Check the user name, and your key or password.")
    } else if said("Connection refused") {
        format!("{host} refused the connection. Check the host name and port, and that it accepts SSH.")
    } else if said("timed out") || said("Operation timed out") {
        format!("{host} didn't answer (the connection timed out). Check the host name, and that you're on a network that can reach it (a VPN, perhaps).")
    } else if said("No route to host") || said("Network is unreachable") {
        format!("Couldn't reach {host} from this network.")
    } else if let Some(line) = last {
        format!("The connection to {host} ended: {line}")
    } else {
        let status = status.map(|s| format!(" ({s})")).unwrap_or_default();
        format!("The connection to {host} ended before Endeavor could start{status}.")
    }
}

#[cfg(test)]
mod tests;

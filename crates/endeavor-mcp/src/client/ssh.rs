//! Reaching a runtime on a server over SSH: one system `ssh` runs a short
//! bootstrap script that installs the helper and `runtime/` if this build's
//! aren't there yet, then becomes `endeavor connect`, whose frames use the
//! rest of ssh's stdin and stdout. From there on it's the same channel as a
//! helper on this computer's (`Channel`). Unless `Options::allow_install`, a
//! server without the helper is only looked at: nothing is written there.
//! Installing what a start needs (such as Julia) is not part of that: it is
//! asked for with each start (`StartOptions::install`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use wire::ToApp;

use super::channel::{Channel, Hello, Notice, Runtime, StartError, StartOptions};
use super::listener::Listener;
use crate::paths::PICK_STATE_DIR_SH;
use super::machines::{Launcher, Server};

/// What happened so far while connecting.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The bootstrap script answered: ssh is in.
    Connected { os: String, arch: String },
    /// The helper and runtime for this build were just installed (or already there).
    Helper { installed: bool },
    /// What the runtime starts with, such as Julia.
    Found { name: String, version: String, path: String },
    /// A line of Julia's download or the runtime's boot log.
    Progress(String),
    /// A cluster job for Julia was submitted.
    Submitted { job: String, summary: String },
    /// The job `job` waits in the queue (Slurm's state and reason); once it runs, state RUNNING and the
    /// node as `reason`.
    Queued { job: String, state: String, reason: String },
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
    /// ssh's prompts itself (through `SSH_ASKPASS`; `Asker::env` gives them). On
    /// Windows `SSH_ASKPASS_REQUIRE` is always `force`, so variables without an
    /// `SSH_ASKPASS` make a sign-in that needs an answer fail rather than wait.
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
                let mut command = Command::new(ssh_program());
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
            // Windows' ssh uses SSH_ASKPASS only when forced; otherwise it waits on a console nobody sees.
            if cfg!(windows) {
                command.env("SSH_ASKPASS_REQUIRE", "force");
            }
        }
        no_window(&mut command);
        Ok(command)
    }

    fn host(&self) -> &str {
        match self {
            Transport::Ssh { host, .. } => host,
            Transport::Shell { .. } => "the local shell",
        }
    }

    /// What to type in a terminal to sign in to the server by hand, with the ssh
    /// Endeavor runs (`ssh_program`): on Windows its path, with forward slashes
    /// so that PowerShell, cmd and Git Bash all take it.
    fn login_command(&self) -> String {
        let ssh = ssh_program().display().to_string().replace('\\', "/");
        match self {
            Transport::Ssh { host, port: Some(port) } => format!("{ssh} -p {port} {host}"),
            Transport::Ssh { host, port: None } => format!("{ssh} {host}"),
            Transport::Shell { .. } => "ssh".into(),
        }
    }
}

/// The ssh to run: on Windows, Windows' own OpenSSH when it is installed, and
/// otherwise the first `ssh` on the PATH. In Git Bash that first one is Git's,
/// which signs in with the Windows user name as it is spelled (`JDoe`) where
/// Windows' lowercases it, and may read another `~/.ssh` than `%USERPROFILE%\.ssh`.
fn ssh_program() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            let ssh = Path::new(&root).join("System32").join("OpenSSH").join("ssh.exe");
            if ssh.is_file() {
                return ssh;
            }
        }
    }
    PathBuf::from("ssh")
}

/// On Windows, start `command` without a console window: ssh is a console
/// program, and from the app, which has no console, each one would open a window.
/// The same goes for the curl or wget that fetches a server's helper (`release`).
#[cfg(windows)]
pub(crate) fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_window(_command: &mut Command) {}

/// Whether `host` can be given to ssh as the destination: an alias, a name,
/// an address (IPv6 has colons) or `user@host`, where the user, which may hold
/// an `@` itself, and the host are not empty. A leading `-` would be an option
/// to ssh, and anything else is not a host name.
pub fn valid_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("Enter an SSH host: an alias from ~/.ssh/config, or user@host.".into());
    }
    let (user, name) = match host.rsplit_once('@') {
        Some((user, name)) => (Some(user), name),
        None => (None, host),
    };
    // A name with a colon is an IPv6 address, which has at least two.
    let address = !name.contains(':') || name.matches(':').count() >= 2;
    if host.starts_with('-') || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':')) || name.is_empty() || user == Some("") || !address {
        return Err(format!("\"{host}\" isn't an SSH host name."));
    }
    Ok(())
}

/// What `connect` needs besides the server.
pub struct Options<'a> {
    pub auth: Auth,
    /// Where the helper and `runtime/` are installed on the server, in a
    /// folder named for this build. A leading `~/` is the server's home.
    /// Empty is `~/.cache/endeavor`.
    pub root: String,
    /// The runtime's state folder on the server: an absolute path, or one
    /// relative to `root`; a leading `~/` is the server's home. `Server::launcher`
    /// has the app's name for it.
    /// Empty is the helper's own default, the folder `serve` and `mcp` use.
    pub state: String,
    /// Julia's `JULIA_DEPOT_PATH` on the server. A leading `~/` in its first
    /// entry is the server's home. Empty is `<root>/depot:`.
    pub depot: String,
    /// A runtime this connect starts ends itself once no notebook has been
    /// open for the idle limit (`endeavor connect --exit-idle`). One that is
    /// already running is left as it was started.
    pub exit_idle: bool,
    /// The user agreed that Endeavor installs its helper on the server. Without
    /// it a server that lacks this build's helper is only looked at, and
    /// `connect` ends with `ConnectError::needs`. The app, which asks its user
    /// itself, passes true. (What a start needs is asked for by each start, `StartOptions::install`; `test` uses this for both.)
    pub allow_install: bool,
    /// How the helper runs the runtime; None is the server record's way (`Server::launcher`).
    /// `Hello::launcher` says what an `Auto` became.
    pub launcher: Option<Launcher>,
    /// The helper binary to send to a server whose `uname -s` is `os` and
    /// `uname -m` is `arch`, as `linux` and `x86_64` (`arm64` as `aarch64`).
    /// Asked only when the server has no helper of this build yet and
    /// `allow_install` is true.
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
/// empty ones), settles a launcher of `auto` (`PICK_LAUNCHER_SH`), and prints `ENDEAVOR <os> <arch> have`, or when this build's
/// helper isn't installed `ENDEAVOR <os> <arch> need <seen> <older|first> <folder>`,
/// the folder being the rest of the line:
/// `seen` is `none`, or for a runtime recorded in the state folder the helper
/// would use (read with `sh`, `cat`, `kill`, `ps` and `squeue` alone, as the
/// helper isn't there to ask): `process:PID` (alive, and its command is a
/// core's), `process-recorded:PID` (alive, and `ps` couldn't say what it is),
/// `job:ID` (Slurm lists it as pending or running) or `job-recorded:ID` (no
/// `squeue`, or it didn't answer). `older` says that a complete helper of
/// another build is installed, and `folder` where this build's would go. If it
/// needs the install, it then reads a byte count and that many bytes of tar,
/// unless the client ends it first. Then it becomes the helper. The values come
/// over stdin and not in the script, so no path can break it; the line is
/// written with `printf %s`, which reads no escapes, and the ids are checked to
/// be digits. `exit_idle` adds the helper's `--exit-idle`; being a fixed word, it
/// is in the script and not in the lines it reads.
pub fn bootstrap_script(version: &str, exit_idle: bool) -> String {
    [
        &format!("v={version}"),
        r#"read -r rt && read -r st && read -r dp && read -r jf && read -r jv && read -r ln || exit 1"#,
        PICK_LAUNCHER_SH,
        r#"case "$rt" in [~]|[~]/*) rt="$HOME${rt#?}";; esac"#,
        r#"case "$st" in [~]|[~]/*) st="$HOME${st#?}";; esac"#,
        r#"case "$dp" in [~]|[~]/*|[~]:*) dp="$HOME${dp#?}";; esac"#,
        r#"c="${rt:-$HOME/.cache/endeavor}""#,
        r#"d="$c/$v""#,
        r#"set --; if [ -n "$st" ]; then case "$st" in /*) sd="$st";; *) sd="$c/$st";; esac; set -- --state-dir "$sd"; fi"#,
        r#"[ -n "$dp" ] || dp="$c/depot:""#,
        r#"if [ -x "$d/endeavor" ] && [ -f "$d/runtime/boot.jl" ]; then s=have; else s=need; fi"#,
        r#"r=none; u=first"#,
        r#"num() { [ -n "$1" ] && [ -z "$(printf %s "$1" | tr -d 0-9)" ]; }"#,
        &format!(r#"if [ "$s" = need ]; then {PICK_STATE_DIR_SH}; for po in "$c"/*/endeavor; do pq=${{po%/endeavor}}; case "$pq" in "$d") ;; *) case "${{pq##*/}}" in *.part.*) ;; *) if [ -x "$po" ] && [ -f "$pq/runtime/boot.jl" ]; then u=older; fi;; esac;; esac; done; for pf in runtime.json job.json; do pj=$(cat "$pd/$pf" 2>/dev/null); if [ "$ln" = slurm ]; then case "$pj" in *job?:?[0-9]*) pk=${{pj#*job?:}}; pk=$(printf %s "$pk" | tr ",}}" "  " | cut -d" " -f1); pk=${{pk#?}}; pk=${{pk%?}}; if [ "$r" = none ] && num "$pk"; then if command -v squeue >/dev/null 2>&1 && pl=$(squeue -h -t PENDING,RUNNING,CONFIGURING -o %i -u "$(id -un)" 2>/dev/null); then if printf %s "$pl" | grep -qx "$pk"; then r=job:$pk; fi; else r=job-recorded:$pk; fi; fi;; esac; else case "$pj" in *pid?:[0-9]*) pk=${{pj#*pid?:}}; pk=$(printf %s "$pk" | tr ",}}" "  " | cut -d" " -f1); if num "$pk" && kill -0 "$pk" 2>/dev/null; then if pa=$(ps -p "$pk" -o args= 2>/dev/null) && [ -n "$pa" ]; then case "$pa" in *core*--state-dir*) r=process:$pk;; esac; else r=process-recorded:$pk; fi; fi;; esac; fi; done; fi"#),
        r#"if [ "$s" = need ]; then printf %s "ENDEAVOR $(uname -s) $(uname -m) need $r $u $d"; else printf %s "ENDEAVOR $(uname -s) $(uname -m) have"; fi; echo"#,
        r#"if [ "$s" = need ]; then read -r n || exit 1; t="$d.part.$$"; rm -rf "$t"; mkdir -p "$t" && head -c "$n" | (cd "$t" && tar xf -) || { rm -rf "$t"; printf %s "Endeavor: installing into $d failed" >&2; echo >&2; exit 1; }; rm -rf "$d"; mv "$t" "$d"; fi"#,
        &format!(r#"exec "$d/endeavor" connect "$@" {}--launcher "$ln" "$jf" "$jv" --runtime "$d/runtime" --depot "$dp" --build "$v""#, if exit_idle { "--exit-idle " } else { "" }),
    ]
    .join("; ")
}

/// Shell for the bootstrap script: a launcher (`$ln`) of `auto` becomes `slurm` when `sinfo` is
/// a file in a folder of `$PATH` or of `wire::slurm::FOLDERS` (where `wire::slurm::has` looks), else
/// `process`, as the helper itself would settle it. The helper is then started with the settled one.
pub(crate) const PICK_LAUNCHER_SH: &str = r#"if [ "$ln" = auto ]; then ln=process; pp="$PATH:/usr/bin:/usr/local/bin:/opt/slurm/bin"; while [ -n "$pp" ]; do pb=${pp%%:*}; if [ -n "$pb" ] && [ -f "$pb/sinfo" ]; then ln=slurm; fi; case "$pp" in *:*) pp=${pp#*:};; *) pp=;; esac; done; fi"#;

/// The six lines the script reads first.
fn preamble(server: &Server, options: &Options) -> Result<Vec<u8>, String> {
    let [flag, value] = server.julia_args();
    let launcher = options.launcher.unwrap_or_else(|| server.launcher()).word().to_owned();
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

/// Lets another thread give up on a connect and what follows on its channel
/// (`start`, `test`), killing its ssh.
#[derive(Default)]
pub struct Cancel {
    /// The ssh the connect started, from then until it has exited and before it
    /// is reaped (the channel's reader does that), so the pid is never another
    /// process's.
    pid: Arc<Mutex<Option<u32>>>,
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

    /// ssh `pid` has exited, and it is no longer this connect's to kill.
    fn finished(pid_slot: &Mutex<Option<u32>>, pid: u32) {
        let mut slot = pid_slot.lock().unwrap();
        if *slot == Some(pid) {
            *slot = None;
        }
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
    let mut taskkill = Command::new("taskkill");
    taskkill.args(["/PID", &pid.to_string(), "/T", "/F"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut taskkill);
    let _ = taskkill.status();
}

/// `uname`'s words as the helper's folders name platforms: `linux` and `aarch64`.
fn platform(os: &str, arch: &str) -> (String, String) {
    (os.to_lowercase(), if arch == "arm64" { "aarch64" } else { arch }.to_owned())
}

/// This computer's platform in the same words (`darwin` for macOS), as
/// `Options::helper` is asked for a server's.
pub fn this_platform() -> (String, String) {
    (if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS }.to_owned(), std::env::consts::ARCH.to_owned())
}

/// A runtime recorded on a server, found without the helper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Running {
    /// A process, by its pid on the server. `checked`: its command is a
    /// runtime's; false when `ps` couldn't say, so that it is only known to be alive.
    Process { pid: u32, checked: bool },
    /// A Slurm job recorded for the cluster, by its id. `listed`: Slurm lists it
    /// as pending or running; false when `squeue` wasn't there or didn't answer,
    /// so that it is only known to be recorded.
    Job { id: String, listed: bool },
}

/// What a connect found on a server that lacks this build's helper, when it
/// wasn't allowed to install it (`Options::allow_install`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedsInstall {
    /// `uname`'s words for the server, as `Linux` and `x86_64`.
    pub os: String,
    pub arch: String,
    /// Where this build's helper would be installed on the server.
    pub folder: String,
    /// About how much would be sent there, in bytes. Not known for a server of
    /// another platform than this computer's, whose helper isn't fetched before
    /// the install is allowed.
    pub bytes: Option<u64>,
    /// A helper of another build is installed there already; this one goes beside it.
    pub update: bool,
    /// A runtime recorded in the folder the helper would use.
    pub running: Option<Running>,
}

/// `seen` of the script's `need` line.
fn parse_seen(seen: &str) -> Option<Option<Running>> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    match seen.split_once(':') {
        None if seen == "none" => Some(None),
        Some(("process", pid)) if digits(pid) => Some(Some(Running::Process { pid: pid.parse().ok()?, checked: true })),
        Some(("process-recorded", pid)) if digits(pid) => Some(Some(Running::Process { pid: pid.parse().ok()?, checked: false })),
        Some(("job", id)) if digits(id) => Some(Some(Running::Job { id: id.to_owned(), listed: true })),
        Some(("job-recorded", id)) if digits(id) => Some(Some(Running::Job { id: id.to_owned(), listed: false })),
        _ => None,
    }
}

/// About what an install sends from this computer: the helper as it runs
/// here and the runtime's files.
fn this_copy_bytes() -> Option<u64> {
    let helper = std::fs::metadata(std::env::current_exe().ok()?).ok()?.len();
    Some(helper + crate::embedded::RUNTIME_FILES.iter().map(|(_, contents)| contents.len() as u64).sum::<u64>())
}

/// Why a connect failed, and whether trying again by itself could help: it
/// can't when the user has to act first (a key to add, a host to accept) or
/// the request itself is wrong, and can when the network or the server was
/// the trouble, a name that doesn't resolve (a laptop that just woke up) included.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectError {
    pub message: String,
    pub retry: bool,
    /// Set when nothing failed but the server needs the helper installed first
    /// and that wasn't allowed: nothing was written there, and `retry` is false.
    pub needs: Option<NeedsInstall>,
}

/// Run the bootstrap on `server` and wait for the helper's hello. The runtime
/// starts later, when the channel is asked to (`start`). A failure says whether
/// retrying could help.
pub fn connect(server: &Server, transport: &Transport, options: &Options, cancel: &Cancel, on: &dyn Fn(Event)) -> Result<(Channel, Hello), ConnectError> {
    let wrong = |message: String| ConnectError { message, retry: false, needs: None };
    let preamble = preamble(server, options).map_err(wrong)?;
    let version = crate::embedded::BUILD_VERSION;
    let mut command = transport.command(&bootstrap_script(version, options.exit_idle), &options.auth).map_err(wrong)?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().map_err(|e| wrong(format!("Couldn't run ssh: {e}")))?;
    if !cancel.started(child.id()) {
        kill_group(child.id());
    }
    let stderr = Stderr::collect(child.stderr.take().unwrap());
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let host = transport.host();
    let give_up = |mut child: Child, why: Option<String>, signed_in: bool| {
        kill_group(child.id());
        Cancel::finished(&cancel.pid, child.id());
        let status = child.wait().ok();
        match why {
            Some(why) => wrong(why),
            None => {
                let (message, retry) = explain_retry(transport, &options.auth, &stderr.finish(), status, cancel.cancelled.load(Ordering::SeqCst), signed_in);
                ConnectError { message, retry, needs: None }
            }
        }
    };

    // The script reads these before it says anything; a failed write shows as the script's end.
    let _ = stdin.write_all(&preamble).and_then(|_| stdin.flush());

    // Login scripts may print things before the script's own line.
    let (os, arch, found) = loop {
        let mut bytes = Vec::new();
        match stdout.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => return Err(give_up(child, None, false)),
            Ok(_) => {
                let line = String::from_utf8_lossy(&bytes);
                let line = line.trim_end_matches(['\n', '\r']);
                let Some(said) = line.strip_prefix("ENDEAVOR ") else {
                    eprintln!("{host}: {line}");
                    continue;
                };
                let parsed = match said.splitn(6, ' ').collect::<Vec<_>>()[..] {
                    [os, arch, "have"] => Some((os, arch, None)),
                    [os, arch, "need", seen, age @ ("older" | "first"), folder] if !folder.is_empty() => parse_seen(seen).map(|running| (os, arch, Some((running, age == "older", folder.to_owned())))),
                    _ => None,
                };
                match parsed {
                    Some((os, arch, found)) => break (os.to_owned(), arch.to_owned(), found),
                    None => return Err(give_up(child, Some(format!("{host}'s setup script answered with something Endeavor doesn't understand: {line}")), true)),
                }
            }
        }
    };
    on(Event::Connected { os: os.clone(), arch: arch.clone() });

    let install = match &found {
        None => None,
        Some((running, update, folder)) => {
            let (platform_os, platform_arch) = platform(&os, &arch);
            if options.allow_install {
                match (options.helper)(&platform_os, &platform_arch).and_then(|helper| install_tar(&helper)) {
                    Ok(tar) => Some(tar),
                    Err(e) => return Err(give_up(child, Some(e), true)),
                }
            } else {
                let here = (platform_os, platform_arch) == this_platform();
                let needs = NeedsInstall { os: os.clone(), arch: arch.clone(), folder: folder.clone(), bytes: if here { this_copy_bytes() } else { None }, update: *update, running: running.clone() };
                let mut error = give_up(child, Some(format!("Endeavor's helper isn't installed on {host}, and installing it wasn't allowed.")), true);
                error.needs = Some(needs);
                return Err(error);
            }
        }
    };
    if let Some(tar) = &install {
        let mut send = format!("{}\n", tar.len()).into_bytes();
        send.extend(tar);
        if stdin.write_all(&send).and_then(|_| stdin.flush()).is_err() {
            return Err(give_up(child, None, true));
        }
    }
    on(Event::Helper { installed: install.is_some() });

    let (pid, slot) = (child.id(), cancel.pid.clone());
    let channel = Channel::open_watched(child, stdin, stdout, move || Cancel::finished(&slot, pid));
    let retry = std::cell::Cell::new(true);
    let hello = channel.wait_hello(|| {
        let (message, again) = explain_retry(transport, &options.auth, &stderr.finish(), None, cancel.cancelled.load(Ordering::SeqCst), true);
        retry.set(again);
        message
    });
    match hello {
        Ok(hello) => match hello.other_version() {
            Some(message) => Err(ConnectError { message, retry: false, needs: None }),
            None => Ok((channel, hello)),
        },
        Err(message) => Err(ConnectError { message, retry: retry.get(), needs: None }),
    }
}

/// Start the runtime on a connected server's channel; `on` hears what was
/// found, the job queueing, and the runtime's log. `notice` hears if the runtime
/// goes away later. `options` says what to submit on a cluster, and whether the
/// helper may install what the start needs: if not, `StartError::NeedsInstall`
/// lists it.
pub fn start(channel: &Channel, listener: &Arc<Listener>, options: &StartOptions, on: &dyn Fn(Event), notice: impl FnOnce(Notice) + Send + 'static) -> Result<Runtime, StartError> {
    let runtime = channel.start_runtime(
        listener,
        options,
        &mut |message| match message {
            ToApp::Progress { line } => on(Event::Progress(line)),
            ToApp::Found { name, version, path } => on(Event::Found { name, version, path }),
            ToApp::Submitted { job, summary } => on(Event::Submitted { job, summary }),
            ToApp::Queued { job, state, reason } => on(Event::Queued { job, state, reason }),
            _ => {}
        },
        notice,
    )?;
    on(Event::Started { node: runtime.node.clone(), reattached: runtime.reattached });
    Ok(runtime)
}

/// Test connection: connect, check the runtime answers through a listener,
/// then stop it (or leave it running if it already was). On a cluster, only
/// ask Slurm about itself: starting Julia there means a job. What the start needs
/// installed is installed only if `options.allow_install`; else the test fails
/// naming it.
pub fn test(server: &Server, transport: &Transport, options: &Options, cancel: &Cancel, on: &dyn Fn(Event)) -> Result<(), String> {
    let (channel, hello) = connect(server, transport, options, cancel, on).map_err(|e| e.message)?;
    if hello.launcher.map_or(server.cluster.is_some(), |l| l == Launcher::Slurm) {
        let reply = channel.files(wire::files::Request::Slurm);
        channel.detach();
        return match reply.map_err(|e| or_cancelled(cancel, e))? {
            wire::files::Reply::Slurm { scheduler } => {
                on(Event::Slurm(scheduler));
                Ok(())
            }
            other => Err(format!("The helper answered {other:?}.")),
        };
    }
    let listener = test_listener()?;
    let runtime = start(&channel, &listener, &StartOptions { install: options.allow_install, ..StartOptions::default() }, on, |_| {}).map_err(|e| or_cancelled(cancel, e.message()))?;
    let answered = bridge_ping(listener.port(), &runtime.token);
    let stopped = if runtime.reattached { Ok(()) } else { channel.stop() };
    channel.detach();
    answered.map_err(|e| format!("Julia started on {}, but it didn't answer through Endeavor's connection ({e}).", runtime.node))?;
    stopped?;
    on(Event::Finished { stopped: !runtime.reattached });
    Ok(())
}

/// What a failure of `test` after its connect reads as: a cancel kills ssh, which shows as the connection closing.
fn or_cancelled(cancel: &Cancel, error: String) -> String {
    if cancel.cancelled.load(Ordering::SeqCst) { "Cancelled.".into() } else { error }
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
                // Bytes, decoded as they come: a line that isn't UTF-8 (a banner in Latin-1) doesn't end the reading.
                let mut stderr = BufReader::new(stderr);
                let mut bytes = Vec::new();
                while let Ok(n) = stderr.read_until(b'\n', &mut bytes) {
                    if n == 0 {
                        break;
                    }
                    let line = String::from_utf8_lossy(&bytes).trim_end_matches(['\n', '\r']).to_owned();
                    bytes.clear();
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
/// ask, so the sign-in failures say what to do instead. ssh exits with 255 for
/// its own failures (sign-in, network, host key) and otherwise with the remote
/// command's status, so those explanations apply only to a 255 or an unknown
/// status (ssh was killed, or the status isn't known), and never once
/// `signed_in`: what the server's script said then is shown as it is.
#[cfg(test)]
fn explain(transport: &Transport, auth: &Auth, stderr: &[String], status: Option<ExitStatus>, cancelled: bool, signed_in: bool) -> String {
    explain_retry(transport, auth, stderr, status, cancelled, signed_in).0
}

/// `explain`, and whether trying again by itself could help (`ConnectError`).
fn explain_retry(transport: &Transport, auth: &Auth, stderr: &[String], status: Option<ExitStatus>, cancelled: bool, signed_in: bool) -> (String, bool) {
    if cancelled {
        return ("Cancelled.".into(), false);
    }
    let host = transport.host();
    let batch = *auth == Auth::Batch;
    let said = |needle: &str| stderr.iter().any(|l| l.contains(needle));
    let last = stderr.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty());
    let ssh_failed = !signed_in && status.is_none_or(|s| s.code().is_none_or(|code| code == 255));
    let ssh_said = if !ssh_failed {
        None
    } else if said("Could not resolve hostname") {
        Some((format!("Couldn't find a server called {host}. Check the SSH host."), true))
    } else if said("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        Some((format!("{host}'s identity (host key) changed since the last connection. If the server was reinstalled, remove its old key with `ssh-keygen -R {host}` in a terminal; otherwise ask its administrator."), false))
    } else if said("Host key verification failed") && batch {
        Some((format!("{host} isn't one of the servers this computer has connected to before, so Endeavor can't check its identity (host key). Run `{}` once in a terminal and accept its host key, then try again.", transport.login_command()), false))
    } else if said("Host key verification failed") {
        Some((format!("{host}'s identity (host key) wasn't confirmed, so Endeavor didn't connect."), false))
    } else if said("Permission denied") && batch {
        Some((format!("{host} refused the sign-in. Either your key isn't accepted there, or it has a passphrase and isn't in your ssh agent (run `ssh-add` in a terminal to add it). Or the server asks for a password or a code, which Endeavor can't ask for yet."), false))
    } else if said("Permission denied") {
        Some((format!("{host} refused the sign-in. Check the user name, and your key or password."), false))
    } else if said("Connection refused") {
        Some((format!("{host} refused the connection. Check the host name and port, and that it accepts SSH."), true))
    } else if said("timed out") || said("Operation timed out") {
        Some((format!("{host} didn't answer (the connection timed out). Check the host name, and that you're on a network that can reach it (a VPN, perhaps)."), true))
    } else if said("No route to host") || said("Network is unreachable") {
        Some((format!("Couldn't reach {host} from this network."), true))
    } else {
        None
    };
    ssh_said.unwrap_or_else(|| {
        let message = match last {
            Some(line) => format!("The connection to {host} ended: {line}"),
            None => format!("The connection to {host} ended before Endeavor could start{}.", status.map(|s| format!(" ({s})")).unwrap_or_default()),
        };
        (message, true)
    })
}

#[cfg(test)]
mod tests;

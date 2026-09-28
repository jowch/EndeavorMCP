//! `endeavor-remote core`: the runtime the helper starts (docs/runtime-core.md).
//! It starts `julia boot.jl` as its child, serves the bridge port, and writes
//! `runtime.json` once Julia is ready. For now every request on the bridge
//! port is passed to Julia's own bridge unchanged but for its Host; the plan
//! moves handlers here one at a time.
//!
//! Julia shares the core's process group, which the helper created, so the
//! helper's signals to the group reach both. The core exits when Julia does,
//! the same way, and passes a stop signal sent to it alone on to Julia.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::http::{self, Head};
use crate::{USAGE, bridge_answers, parse_state, remove_state};

/// Where boot.jl writes its state for the core, in the state folder.
const JULIA_STATE: &str = "julia.json";

const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

struct Args {
    state_dir: PathBuf,
    julia: String,
    runtime: PathBuf,
    depot: String,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = argv.iter();
    let (mut state_dir, mut julia, mut runtime, mut depot) = (None, None, None, None);
    while let Some(arg) = args.next() {
        let value = args.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value?)),
            "--julia" => julia = Some(value?),
            "--runtime" => runtime = Some(PathBuf::from(value?)),
            "--depot" => depot = Some(value?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        julia: julia.ok_or("--julia is required")?,
        runtime: runtime.ok_or("--runtime is required")?,
        depot: depot.ok_or("--depot is required")?,
    })
}

/// `endeavor-remote core …`, with ENDEAVOR_TOKEN and ENDEAVOR_LAUNCHER in the
/// environment. Its stdout and stderr are the runtime's log, which Julia shares.
pub fn main(argv: &[String]) -> ! {
    let args = parse_args(argv).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    });
    let fail = |message: String| -> ! {
        eprintln!("endeavor-remote core: {message}");
        std::process::exit(1);
    };
    let token = std::env::var("ENDEAVOR_TOKEN").unwrap_or_else(|_| fail("ENDEAVOR_TOKEN is not set".into()));
    let launcher = std::env::var("ENDEAVOR_LAUNCHER").unwrap_or_else(|_| "process".into());
    let (stop_signals, inherited_mask) = block_stop_signals();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| fail(format!("Couldn't open the bridge port: {e}")));
    let julia_state = args.state_dir.join(JULIA_STATE);
    let _ = std::fs::remove_file(&julia_state);
    let mut command = julia_command(&args, &token, &launcher, &julia_state).unwrap_or_else(|e| fail(e));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            libc::pthread_sigmask(libc::SIG_SETMASK, &inherited_mask, std::ptr::null_mut());
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut julia = command.spawn().unwrap_or_else(|e| fail(format!("Couldn't start {}: {e}", args.julia)));
    let julia_pid = julia.id() as i32;
    pass_on_stop_signals(stop_signals, julia_pid);

    let upstream = Arc::new(OnceLock::new());
    let bridge_port = listener.local_addr().unwrap().port();
    accept(listener, upstream.clone());

    let status = loop {
        if let Some(status) = julia.try_wait().unwrap_or(None) {
            break status;
        }
        if upstream.get().is_none()
            && let Some(port) = julia_ready(&julia_state, &args.state_dir, bridge_port)
        {
            let _ = upstream.set(port);
        }
        if upstream.get().is_some() {
            break julia.wait().unwrap_or_else(|e| fail(format!("waiting for Julia: {e}")));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::fs::remove_file(&julia_state);
    remove_state(&args.state_dir, std::process::id() as i32);
    exit_like(status)
}

/// `julia boot.jl` with the environment it reads (see runtime/boot.jl), on
/// ports free here, writing its state to `julia_state` for the core.
fn julia_command(args: &Args, token: &str, launcher: &str, julia_state: &Path) -> Result<Command, String> {
    let ports = free_ports()?;
    let runtime = args.runtime.display();
    let mut command = Command::new(&args.julia);
    command
        .arg("--color=no")
        .arg(format!("--project={runtime}"))
        .arg(format!("{runtime}/boot.jl"))
        .args(ports.map(|p| p.to_string()))
        .env("JULIA_DEPOT_PATH", &args.depot)
        // Not argv, which `ps` shows to every user.
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_STATE", julia_state)
        .env("ENDEAVOR_LAUNCHER", launcher)
        .stdin(Stdio::null());
    Ok(command)
}

fn free_ports() -> Result<[u16; 2], String> {
    // Both held at once so the OS can't hand out the same port twice.
    let pluto = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let mcp = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok([&pluto, &mcp].map(|l| l.local_addr().unwrap().port()))
}

/// Once Julia has written its state and its bridge answers, write
/// `runtime.json` for the helper: Julia's, but naming the core's pid and
/// bridge port. Julia's bridge port.
fn julia_ready(julia_state: &Path, state_dir: &Path, bridge_port: u16) -> Option<u16> {
    let mut state: Value = serde_json::from_str(&std::fs::read_to_string(julia_state).ok()?).ok()?;
    let julia = parse_state(&state)?;
    if !bridge_answers(&julia) {
        return None;
    }
    state["pid"] = std::process::id().into();
    state["mcp_port"] = bridge_port.into();
    if let Err(e) = write_private(&state_dir.join("runtime.json"), state.to_string().as_bytes()) {
        eprintln!("endeavor-remote core: {e}");
        return None;
    }
    Some(julia.mcp_port)
}

/// Write `path` whole and readable only by us, so a reader never sees half of
/// it and nobody else sees the token.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Exit the way Julia did, so the helper reports the same status.
fn exit_like(status: ExitStatus) -> ! {
    if let Some(signal) = status.signal() {
        // SAFETY: plain syscalls; the default action of `signal` ends this process.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(signal);
        }
    }
    std::process::exit(status.code().unwrap_or(1))
}

/// Block the stop signals (for `pass_on_stop_signals` to take) before any
/// thread starts; the mask we had, for Julia.
fn block_stop_signals() -> (libc::sigset_t, libc::sigset_t) {
    // SAFETY: initializing and applying signal sets on this (still only) thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in STOP_SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        (set, old)
    }
}

/// A stop signal sent to the core goes to Julia; the core exits when Julia does.
fn pass_on_stop_signals(set: libc::sigset_t, julia_pid: i32) {
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only signals blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 {
                // SAFETY: plain syscall.
                unsafe { libc::kill(julia_pid, signal) };
            }
        }
    });
}

/// Serve the bridge port: a thread per client, of which there are only a few
/// (the helper's relayed streams).
fn accept(listener: TcpListener, upstream: Arc<OnceLock<u16>>) {
    std::thread::spawn(move || {
        for client in listener.incoming().map_while(Result::ok) {
            let upstream = upstream.clone();
            std::thread::spawn(move || {
                let _ = serve_client(client, &upstream);
            });
        }
    });
}

/// One client's requests in turn, until either side closes.
fn serve_client(client: TcpStream, upstream: &OnceLock<u16>) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    while let Some(request) = Head::read(&mut reader)? {
        let Some(&port) = upstream.get() else {
            return refuse(&mut client, "503 Service Unavailable", "Julia isn't ready yet");
        };
        if !forward(request, &mut reader, &mut client, port)? {
            break;
        }
    }
    Ok(())
}

/// Pass one request to Julia's bridge on a connection of its own, and its
/// response back as it comes. Whether the client connection can carry another request.
fn forward(mut request: Head, reader: &mut BufReader<TcpStream>, client: &mut TcpStream, port: u16) -> io::Result<bool> {
    let upstream = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(upstream) => upstream,
        Err(_) => {
            refuse(client, "502 Bad Gateway", "Julia's bridge isn't answering")?;
            return Ok(false);
        }
    };
    let _ = upstream.set_nodelay(true);
    request.replace("Host", &format!("127.0.0.1:{port}"));
    let mut to_julia = upstream.try_clone()?;
    request.write_to(&mut to_julia)?;
    http::copy_body(reader, &mut to_julia, &mut request.request_body()?)?;

    let mut from_julia = BufReader::new(upstream);
    let response = loop {
        let response = Head::read(&mut from_julia)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        response.write_to(client)?;
        // `100 Continue` comes before the real response.
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    let mut framing = response.response_body(request.method())?;
    http::relay_body(&mut from_julia, client, &mut framing)?;
    Ok(request.keeps_alive() && response.keeps_alive() && framing != http::Framing::UntilClose)
}

fn refuse(client: &mut TcpStream, status: &str, why: &str) -> io::Result<()> {
    let body = serde_json::json!({ "error": why }).to_string();
    write!(client, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

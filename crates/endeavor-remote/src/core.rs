//! `endeavor-remote core`: the runtime the helper starts (docs/runtime-core.md).
//! It starts `julia boot.jl` as its child, serves the runtime's one port, and
//! writes `runtime.json` once Julia is ready. On that port it serves the
//! agent's MCP connection at `/mcp` and the app's `/endeavor/call`s itself (see
//! `mcp`), and the app's `/endeavor/events` stream (see `notebooks`), driving
//! Pluto through Julia's adapter; the few calls Julia answers
//! (`endeavor/set_folder`, `endeavor/shutdown`) go on to Julia's own bridge.
//! Every other path is Pluto's page, passed through to Pluto's private port
//! with Pluto's secret added, WebSockets included (docs/one-port.md).
//!
//! Julia shares the core's process group, which the helper created, so the
//! helper's signals to the group reach both. The core exits when Julia does,
//! the same way, and passes a stop signal sent to it alone on to Julia. On
//! Windows the core puts itself in a Job Object before starting Julia, so
//! Julia and its workers end when the core does.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{self, Head};
use crate::mcp::Bridge;
use crate::{USAGE, bridge_call, owner_only, remove_state};

/// Where boot.jl writes its state for the core, in the state folder.
const JULIA_STATE: &str = "julia.json";

#[cfg(unix)]
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
/// environment, and ENDEAVOR_BUILD, the app build it came from, which it
/// reports to the app. Its stdout and stderr are the runtime's log, which Julia shares.
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
    #[cfg(unix)]
    let (stop_signals, inherited_mask) = block_stop_signals();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| fail(format!("Couldn't open the runtime's port: {e}")));
    let julia_state = args.state_dir.join(JULIA_STATE);
    let _ = std::fs::remove_file(&julia_state);
    let mut command = julia_command(&args, &token, &launcher, &julia_state).unwrap_or_else(|e| fail(e));
    // Kept, unused, until the process exits: closing it ends the job.
    #[cfg(windows)]
    let _job = crate::winproc::job_ending_with_this_process().unwrap_or_else(|e| fail(format!("Couldn't keep Julia's processes together with this one (Job Object): {e}")));
    // SAFETY: only async-signal-safe calls between fork and exec.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(move || {
            libc::pthread_sigmask(libc::SIG_SETMASK, &inherited_mask, std::ptr::null_mut());
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut julia = command.spawn().unwrap_or_else(|e| fail(format!("Couldn't start {}: {e}", args.julia)));
    #[cfg(unix)]
    pass_on_stop_signals(stop_signals, julia.id() as i32);

    let cookie = cookie_name(&token);
    let bridge = Bridge::new(token, &args.depot);
    if let Ok(build) = std::env::var("ENDEAVOR_BUILD") {
        let _ = bridge.notebooks.build.set(build);
    }
    let served = Arc::new(Served { bridge, pluto: OnceLock::new(), cookie });
    let port = listener.local_addr().unwrap().port();
    accept(listener, served.clone());

    let status = loop {
        if let Some(status) = julia.try_wait().unwrap_or(None) {
            break status;
        }
        if let Some(ready) = julia_ready(&julia_state, &args.state_dir, port, &served.bridge.token) {
            let _ = served.pluto.set(ready.pluto);
            let _ = served.bridge.julia.port.set(ready.bridge_port);
            served.bridge.notebooks.start();
            break julia.wait().unwrap_or_else(|e| fail(format!("waiting for Julia: {e}")));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::fs::remove_file(&julia_state);
    remove_state(&args.state_dir, std::process::id() as i32);
    exit_like(status)
}

/// `julia boot.jl` with the environment it reads (see runtime/boot.jl), on
/// private ports free here (Pluto's, and Julia's bridge for the core), writing
/// its state to `julia_state` for the core.
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

/// Pluto's private port and the secret it requires.
struct Pluto {
    port: u16,
    secret: String,
}

/// What `julia boot.jl` reports once it's up.
struct JuliaReady {
    pluto: Pluto,
    /// Julia's own bridge, for the adapter's calls.
    bridge_port: u16,
}

/// Once Julia has written its state and its bridge answers, write
/// `runtime.json` for the helper: the core's pid and its one `port`, and
/// Julia's launcher, node and job. Pluto's port and secret stay out of it.
fn julia_ready(julia_state: &Path, state_dir: &Path, port: u16, token: &str) -> Option<JuliaReady> {
    let julia: Value = serde_json::from_str(&std::fs::read_to_string(julia_state).ok()?).ok()?;
    let port_of = |key: &str| julia[key].as_u64().and_then(|p| u16::try_from(p).ok());
    let ready = JuliaReady {
        pluto: Pluto { port: port_of("pluto_port")?, secret: julia["pluto_secret"].as_str()?.to_owned() },
        bridge_port: port_of("mcp_port")?,
    };
    if !bridge_call(ready.bridge_port, "/call", token, "ping").is_ok_and(|status| status == 200) {
        return None;
    }
    // With the pid, what tells the core from a later process given its pid.
    #[cfg(windows)]
    let started = crate::winproc::own_start_time();
    #[cfg(unix)]
    let started: Option<u64> = None;
    let state = json!({
        "launcher": julia["launcher"], "node": julia["node"], "job": julia["job"],
        "pid": std::process::id(), "started": started, "port": port, "token": token,
    });
    if let Err(e) = write_private(&state_dir.join("runtime.json"), state.to_string().as_bytes()) {
        eprintln!("endeavor-remote core: {e}");
        return None;
    }
    Some(ready)
}

/// Write `path` whole and readable only by us, so a reader never sees half of
/// it and nobody else sees the token.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    owner_only(OpenOptions::new().write(true).create_new(true))
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Exit the way Julia did, so the helper reports the same status.
fn exit_like(status: ExitStatus) -> ! {
    #[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

/// What the runtime's port serves: the bridge, and once Julia is ready, Pluto.
struct Served {
    bridge: Bridge,
    pluto: OnceLock<Pluto>,
    /// The cookie that lets a browser into Pluto's page (`cookie_name`).
    cookie: String,
}

/// Serve the runtime's port: a thread per client, of which there are only a
/// few (the helper's relayed streams).
fn accept(listener: TcpListener, served: Arc<Served>) {
    std::thread::spawn(move || {
        for client in listener.incoming().map_while(Result::ok) {
            let served = served.clone();
            std::thread::spawn(move || {
                let _ = serve_client(client, &served);
            });
        }
    });
}

/// Where a request on the runtime's port goes, by its path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// The agent's MCP connection.
    Mcp,
    /// The app's notebook state stream.
    Events,
    /// The app's JSON-RPC calls (`endeavor/*`, and tools without a session).
    Call,
    /// Any other path under `/endeavor/`.
    NotFound,
    /// Everything else: Pluto's page, its files and its WebSocket.
    Pluto,
}

impl Route {
    fn of(path: &str) -> Route {
        match path {
            "/mcp" => Route::Mcp,
            "/endeavor/events" => Route::Events,
            "/endeavor/call" => Route::Call,
            _ if path.starts_with("/endeavor/") => Route::NotFound,
            _ => Route::Pluto,
        }
    }
}

/// One client's requests in turn, until either side closes.
fn serve_client(client: TcpStream, served: &Served) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let bridge = &served.bridge;
    while let Some(mut request) = Head::read(&mut reader)? {
        let route = Route::of(request.path());
        let keep_alive = match access(&request, route, &bridge.token, &served.cookie) {
            Access::Granted => None,
            Access::SetCookie { location } => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", served.cookie, bridge.token);
                let close = if request.keeps_alive() { "" } else { "Connection: close\r\n" };
                write!(client, "HTTP/1.1 303 See Other\r\nLocation: {location}\r\nSet-Cookie: {cookie}\r\nContent-Length: 0\r\n{close}\r\n")?;
                Some(request.keeps_alive())
            }
            Access::Refused(status, error) => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                let body = json!({ "error": error }).to_string();
                http::respond(&mut client, status, Some("application/json"), body.as_bytes(), request.keeps_alive())?;
                Some(request.keeps_alive())
            }
        };
        if let Some(keep_alive) = keep_alive {
            if !keep_alive {
                break;
            }
            continue;
        }
        let (Some(pluto), Some(&julia)) = (served.pluto.get(), bridge.julia.port.get()) else {
            return refuse(&mut client, "503 Service Unavailable", "Julia isn't ready yet");
        };
        let keep_alive = match (route, request.method()) {
            (Route::Events, "GET") => return bridge.notebooks.stream_events(&request, client),
            (Route::Mcp, "POST") => bridge.mcp(&request, &mut reader, &mut client)?,
            (Route::Call, "POST") => {
                let body = http::read_body(&mut reader, request.request_body()?)?;
                match bridge.app_call(&body) {
                    Some(reply) => {
                        http::respond(&mut client, "200 OK", Some("application/json"), reply.as_bytes(), request.keeps_alive())?;
                        request.keeps_alive()
                    }
                    None => {
                        request.set_target("/call");
                        request.replace("Host", &format!("127.0.0.1:{julia}"));
                        forward(request, Some(&body), &mut reader, julia, &|_| {})?
                    }
                }
            }
            (Route::Pluto, _) => {
                request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                request.headers.push(("Cookie".into(), format!("secret={}", pluto.secret)));
                forward(request, None, &mut reader, pluto.port, &|response| {
                    response.headers.retain(|(name, value)| !(name.eq_ignore_ascii_case("Set-Cookie") && value.trim_start().starts_with("secret=")));
                })?
            }
            (Route::NotFound, _) => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                http::respond(&mut client, "404 Not Found", None, b"", request.keeps_alive())?;
                request.keeps_alive()
            }
            // No server-initiated stream on /mcp, and no session to end.
            _ => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                http::respond(&mut client, "405 Method Not Allowed", None, b"", request.keeps_alive())?;
                request.keeps_alive()
            }
        };
        if !keep_alive {
            break;
        }
    }
    Ok(())
}

/// Answer one request on the app's listener while the app can't reach the
/// runtime (`token` its bearer token), then close. The agent's MCP client
/// gets an answer instead of a reset, which Claude Code counts toward giving
/// up on the server for good: a tool call fails with `why`, and the rest is
/// answered as the core would. Any other request is closed unanswered.
pub fn serve_unreachable(client: TcpStream, token: &str, why: &str) -> io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let Some(request) = Head::read(&mut reader)? else { return Ok(()) };
    if Route::of(request.path()) != Route::Mcp {
        return Ok(());
    }
    if let Access::Refused(status, error) = access(&request, Route::Mcp, token, "") {
        let body = json!({ "error": error }).to_string();
        return http::respond(&mut client, status, Some("application/json"), body.as_bytes(), false);
    }
    if request.method() == "POST" {
        crate::mcp::post(&request, &mut reader, &mut client, false, |message, _| crate::mcp::answer_unreachable(message, &request, why))?;
    }
    Ok(())
}

/// Whether a request may go where its route sends it.
#[derive(Debug, PartialEq)]
enum Access {
    Granted,
    /// A browser's first visit, with `token=` in its URL: it gets the cookie
    /// and goes to `location`, the same URL without the token.
    SetCookie { location: String },
    /// A status and an error code.
    Refused(&'static str, &'static str),
}

/// Whether a request may go where `route` sends it, given the runtime's
/// `token` and the name of its browser `cookie`. Loopback isn't private on a
/// shared machine, so the token is what keeps other local users out, and a
/// DNS-rebinding page sends a foreign Host.
///
/// Endeavor's own routes are a control API, never a web API: they take the
/// token only as a bearer header, and any request with an Origin came from a
/// browser page (MCP clients never send one). Pluto's page also takes the
/// cookie, as long as the request comes from that page itself: notebook
/// output runs its own JavaScript there, and must not reach the app's calls
/// (approving its own runs) or, through the cookie another runtime set on
/// this loopback address (cookies ignore ports), that runtime's Pluto.
fn access(request: &Head, route: Route, token: &str, cookie: &str) -> Access {
    let host = request.header("Host").unwrap_or_default();
    if !loopback_host(host) {
        return Access::Refused("403 Forbidden", "host_not_loopback");
    }
    let origin = request.header("Origin");
    let bearer = same(request.header("Authorization").unwrap_or_default(), &format!("Bearer {token}"));
    if route != Route::Pluto {
        return match (origin, bearer) {
            (Some(_), _) => Access::Refused("403 Forbidden", "browser_origin_refused"),
            (None, true) => Access::Granted,
            (None, false) => Access::Refused("401 Unauthorized", "unauthorized"),
        };
    }
    let from_this_page = origin.is_none_or(|origin| origin == format!("http://{host}"))
        && request.header("Sec-Fetch-Site").is_none_or(|site| matches!(site, "same-origin" | "none"));
    if !from_this_page {
        return Access::Refused("403 Forbidden", "browser_origin_refused");
    }
    if bearer || has_cookie(request, cookie, token) {
        return Access::Granted;
    }
    match without_token(request.target()) {
        (Some(given), location) if same(given, token) => Access::SetCookie { location },
        _ => Access::Refused("401 Unauthorized", "unauthorized"),
    }
}

/// The name of the cookie that holds `token` for browsers: one per runtime,
/// so runtimes on the same loopback address don't overwrite each other's.
fn cookie_name(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let id: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("endeavor-{id}")
}

/// Whether the request carries cookie `name` with the value `token`.
fn has_cookie(request: &Head, name: &str, token: &str) -> bool {
    request
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("Cookie"))
        .flat_map(|(_, value)| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(n, value)| n == name && same(value, token))
}

/// A target's `token` query parameter, and the target without it.
fn without_token(target: &str) -> (Option<&str>, String) {
    let Some((path, query)) = target.split_once('?') else { return (None, target.to_owned()) };
    let mut token = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| match pair.strip_prefix("token=") {
            Some(value) => {
                token = Some(value);
                false
            }
            None => !pair.is_empty(),
        })
        .collect();
    let location = if rest.is_empty() { path.to_owned() } else { format!("{path}?{}", rest.join("&")) };
    (token, location)
}

/// `Host` as clients send it: `127.0.0.1:2346`, `localhost`, `[::1]:2346`.
fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(_) => host.find(']').map_or("", |end| &host[..=end]),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// Whether `given` is `expected`, compared in constant time.
fn same(given: &str, expected: &str) -> bool {
    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Pass one request to the server at `port` (Pluto, or Julia's bridge) on a
/// connection of its own, its body from `client` or already read, and the
/// response back as it comes, through `edit`. Whether the client connection
/// can carry another request.
fn forward(request: Head, body: Option<&[u8]>, client: &mut BufReader<TcpStream>, port: u16, edit: &dyn Fn(&mut Head)) -> io::Result<bool> {
    let upstream = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(upstream) => upstream,
        Err(_) => {
            refuse(client.get_mut(), "502 Bad Gateway", "Julia isn't answering")?;
            return Ok(false);
        }
    };
    let _ = upstream.set_nodelay(true);
    let mut to_upstream = upstream.try_clone()?;
    let mut request = request;
    match body {
        None => {
            request.write_to(&mut to_upstream)?;
            http::copy_body(client, &mut to_upstream, &mut request.request_body()?)?;
        }
        Some(bytes) => {
            request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
            request.headers.push(("Content-Length".into(), bytes.len().to_string()));
            request.write_to(&mut to_upstream)?;
            to_upstream.write_all(bytes)?;
        }
    }
    http::relay_response(&request, client, &mut BufReader::new(upstream), edit)
}

fn refuse(client: &mut TcpStream, status: &str, why: &str) -> io::Result<()> {
    let body = json!({ "error": why }).to_string();
    write!(client, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> Head {
        Head::read(&mut text.as_bytes()).unwrap().unwrap()
    }

    const TOKEN: &str = "t0k3n";

    fn access_to(request: &str) -> Access {
        let request = head(request);
        access(&request, Route::of(request.path()), TOKEN, "endeavor-abc")
    }

    #[test]
    fn routes_endeavors_paths_to_the_core_and_the_rest_to_pluto() {
        assert_eq!(Route::of("/mcp"), Route::Mcp);
        assert_eq!(Route::of("/endeavor/events"), Route::Events);
        assert_eq!(Route::of("/endeavor/call"), Route::Call);
        assert_eq!(Route::of("/endeavor/nope"), Route::NotFound);
        for path in ["/", "/edit", "/open", "/static/x.js", "/channels", "/endeavor", "/mcp/x", "/events", "/call"] {
            assert_eq!(Route::of(path), Route::Pluto, "{path}");
        }
    }

    #[test]
    fn the_header_opens_every_path_and_the_cookie_only_plutos() {
        let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
        let cookie = format!("Cookie: other=1; endeavor-abc={TOKEN}\r\n");
        for path in ["/mcp", "/endeavor/call", "/endeavor/events", "/edit?id=1"] {
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{bearer}\r\n")), Access::Granted, "{path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")), Access::Refused("401 Unauthorized", "unauthorized"), "{path}");
        }
        assert_eq!(access_to(&format!("GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Granted);
        for path in ["/mcp", "/endeavor/call", "/endeavor/answer_run"] {
            assert_eq!(access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Refused("401 Unauthorized", "unauthorized"), "{path}");
            assert_eq!(
                access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\nOrigin: http://127.0.0.1:9\r\n{cookie}{bearer}\r\n")),
                Access::Refused("403 Forbidden", "browser_origin_refused"),
                "a page's request, even with the token"
            );
        }
        let wrong = |c: &str| access_to(&format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:9\r\nCookie: {c}\r\n\r\n"));
        assert_eq!(wrong(&format!("endeavor-def={TOKEN}")), Access::Refused("401 Unauthorized", "unauthorized"), "another runtime's cookie");
        assert_eq!(wrong("endeavor-abc=t0k3m"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(wrong(&format!("secret={TOKEN}")), Access::Refused("401 Unauthorized", "unauthorized"));
    }

    #[test]
    fn the_cookie_works_only_from_this_page() {
        let cookie = format!("Cookie: endeavor-abc={TOKEN}\r\n");
        let from = |headers: &str| access_to(&format!("GET /channels HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{headers}{cookie}\r\n"));
        assert_eq!(from("Origin: http://127.0.0.1:9\r\nSec-Fetch-Site: same-origin\r\n"), Access::Granted);
        assert_eq!(from("Sec-Fetch-Site: none\r\n"), Access::Granted, "the app loading the page");
        let refused = Access::Refused("403 Forbidden", "browser_origin_refused");
        assert_eq!(from("Origin: http://127.0.0.1:10\r\n"), refused, "another runtime's page");
        assert_eq!(from("Sec-Fetch-Site: same-site\r\n"), refused, "an image on another runtime's page");
        assert_eq!(from("Origin: https://example.com\r\n"), refused);
        assert_eq!(access_to(&format!("GET / HTTP/1.1\r\nHost: evil.example\r\n{cookie}\r\n")), Access::Refused("403 Forbidden", "host_not_loopback"));
    }

    #[test]
    fn the_token_in_a_url_sets_the_cookie_and_leaves_the_url() {
        let visit = |target: &str| access_to(&format!("GET {target} HTTP/1.1\r\nHost: localhost:9\r\n\r\n"));
        assert_eq!(visit(&format!("/?token={TOKEN}")), Access::SetCookie { location: "/".into() });
        assert_eq!(visit(&format!("/edit?id=1&token={TOKEN}&x=2")), Access::SetCookie { location: "/edit?id=1&x=2".into() });
        assert_eq!(visit("/?token=nope"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(
            access_to(&format!("GET /mcp?token={TOKEN} HTTP/1.1\r\nHost: localhost:9\r\n\r\n")),
            Access::Refused("401 Unauthorized", "unauthorized"),
            "only Pluto's paths take it"
        );
    }

    #[test]
    fn each_runtime_has_its_own_cookie() {
        assert_eq!(cookie_name("a"), "endeavor-ca978112ca1b", "from the token's SHA-256, not the token");
        assert_ne!(cookie_name("b"), cookie_name("a"));
    }
}

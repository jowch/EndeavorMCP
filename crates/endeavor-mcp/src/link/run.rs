//! The link process (`endeavor link --machine ID`, see `link`): one `client::Session`
//! for a machine, and the control interface the fronts call. What the session
//! does (connect, start, get the connection back) is in `client`; this file is
//! what makes it a process: the record, the control port, the idle limit, the
//! stop signals and leaving.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wire::slurm::JobRequest;

use super::{Record, Status, valid_id};
use crate::client::{Config, Messages, Server, Session, Transport, Want, this_platform};
use crate::http::{self, Framing, Head};
use crate::standalone::Env;

/// The link ends this long after the last control request.
const IDLE: Duration = Duration::from_secs(8 * 3600);

/// The largest body a control request may have.
const MAX_BODY: u64 = 64 * 1024;

struct Shared {
    dir: PathBuf,
    token: String,
    session: Session,
    activity: Mutex<Instant>,
    /// Control requests in flight.
    busy: AtomicUsize,
    leaving: AtomicBool,
    idle: Duration,
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// `endeavor link --machine ID`.
pub(crate) fn main(argv: &[String]) -> ! {
    let fail = |message: String| -> ! {
        eprintln!("endeavor link: {message}");
        exit(1)
    };
    let (id, install) = match argv {
        [flag, id] if flag == "--machine" => (id.clone(), false),
        [flag, id, install] if flag == "--machine" && install == "--install" => (id.clone(), true),
        _ => fail("usage: endeavor link --machine ID [--install]".into()),
    };
    valid_id(&id).unwrap_or_else(|e| fail(e));
    let env = Env::from_vars(&|name| std::env::var(name).ok());
    let dir = env.links_dir().join(&id);
    crate::make_state_dir(&dir).unwrap_or_else(|e| fail(e));
    let server_path = dir.join(super::SERVER_FILE);
    let server: Server = match std::fs::read_to_string(&server_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| fail(format!("{} isn't readable as the machine's settings ({e}). Call the tool again to start the connection afresh.", server_path.display()))),
        Err(e) => fail(format!("Couldn't read {} ({e}). It holds the machine's settings and is written when the connection is started, so call the tool again to start it.", server_path.display())),
    };
    if server.id != id {
        fail(format!("{} holds the settings of {}, not of {id}. Call the tool again to start the connection afresh.", server_path.display(), server.id));
    }
    if let Some(running) = super::running(&dir, &id)
        && running.pid != std::process::id()
    {
        fail(format!("The link to {id} already runs (pid {}).", running.pid));
    }
    catch_stop_signals();
    let idle = std::env::var("ENDEAVOR_LINK_IDLE_SECS").ok().and_then(|s| s.parse::<f64>().ok()).filter(|secs| *secs > 0.0).and_then(|secs| Duration::try_from_secs_f64(secs).ok()).unwrap_or(IDLE);
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let helper = |os: &str, arch: &str| {
        if (os.to_owned(), arch.to_owned()) == this_platform() {
            std::env::current_exe().map_err(|e| format!("Couldn't find the endeavor program itself: {e}"))
        } else {
            crate::release::helper_for(os, arch, &Env::from_vars(&|name| std::env::var(name).ok()).helpers_dir())
        }
    };
    let mut config = Config::new(server, helper);
    if std::env::var_os("ENDEAVOR_LINK_SHELL").is_some() {
        config.transport = Transport::Shell { env: Vec::new(), ask: std::env::var("ENDEAVOR_LINK_ASK").ok() };
    }
    (config.root, config.state, config.depot, config.allow_install) = (var("ENDEAVOR_LINK_ROOT"), var("ENDEAVOR_LINK_STATE"), var("ENDEAVOR_LINK_DEPOT"), install);
    config.messages = Messages {
        restart_failed: |name| format!("Julia on {name} couldn't start. Call use_machine to try again."),
        not_connected: |name| format!("Endeavor isn't connected to {name}. Call use_machine to use it again."),
    };
    let control = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| fail(format!("Couldn't open the control port: {e}")));
    let port = control.local_addr().map(|a| a.port()).unwrap_or_else(|e| fail(e.to_string()));
    let token = crate::random_hex::<32>().unwrap_or_else(|e| fail(e));
    let session = Session::new(config).unwrap_or_else(|e| fail(e));
    #[cfg(windows)]
    let started = crate::winproc::own_start_time();
    #[cfg(not(windows))]
    let started: Option<u64> = None;
    let record = Record { machine: id.clone(), pid: std::process::id(), started, port, token: token.clone(), build: crate::embedded::BUILD_VERSION.to_owned(), protocol: super::PROTOCOL };
    let shared = Arc::new(Shared { dir: dir.clone(), token, session, activity: Mutex::new(Instant::now()), busy: AtomicUsize::new(0), leaving: AtomicBool::new(false), idle });
    let text = serde_json::to_string(&record).unwrap_or_default();
    if let Err(e) = crate::core::write_private(&dir.join("link.json"), text.as_bytes()) {
        shared.session.close();
        fail(e);
    }
    eprintln!("The link to {id} listens for control on 127.0.0.1:{port}, and relays on 127.0.0.1:{}.", shared.session.port());

    let serving = shared.clone();
    std::thread::spawn(move || serve_control(serving, control));
    watch(shared)
}

impl Shared {
    fn touch(&self) {
        *self.activity.lock().unwrap() = Instant::now();
    }

    fn status(&self) -> Status {
        let s = self.session.status();
        Status {
            machine: s.machine,
            name: s.name,
            state: s.state,
            step: s.step,
            error: s.error,
            hello: s.hello,
            runtime: s.runtime,
            job: s.job,
            queue: s.queue,
            nothing_running: s.nothing_running,
            needs_install: s.needs_install,
            pid: std::process::id(),
            build: crate::embedded::BUILD_VERSION.to_owned(),
        }
    }
}

/// Ends the link when it has been idle, or a stop signal came.
fn watch(shared: Arc<Shared>) -> ! {
    let tick = (shared.idle / 4).clamp(Duration::from_millis(20), Duration::from_millis(100));
    loop {
        std::thread::sleep(tick);
        if SIGNALLED.load(Ordering::SeqCst) {
            eprintln!("Stopped by a signal");
            leave(&shared);
        }
        if shared.busy.load(Ordering::SeqCst) == 0 && shared.activity.lock().unwrap().elapsed() >= shared.idle {
            eprintln!("No request for {} s; ending (the runtime keeps running)", shared.idle.as_secs());
            leave(&shared);
        }
    }
}

/// Detach from the runtime, remove the record and exit. Never stops the runtime.
fn leave(shared: &Arc<Shared>) -> ! {
    if !start_leaving(shared) {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    finish_leaving(shared)
}

/// The first step of leaving, which comes before anything slow: the record goes,
/// so that a front that asks for the link now gets a new one, and the control
/// port stops answering as a link. False when another thread is leaving already.
fn start_leaving(shared: &Shared) -> bool {
    if shared.leaving.swap(true, Ordering::SeqCst) {
        return false;
    }
    let path = shared.dir.join("link.json");
    let ours = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Record>(&text).ok()).is_some_and(|r| r.pid == std::process::id());
    if ours {
        let _ = std::fs::remove_file(&path);
    }
    true
}

fn finish_leaving(shared: &Shared) -> ! {
    shared.session.close();
    exit(0)
}

fn serve_control(shared: Arc<Shared>, socket: TcpListener) {
    for connection in socket.incoming() {
        let Ok(connection) = connection else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let shared = shared.clone();
        std::thread::spawn(move || {
            let _ = control(&shared, connection);
        });
    }
}

/// Counts a control request as in flight, and as activity when it starts and ends.
struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    fn begin(shared: &'a Shared) -> InFlight<'a> {
        shared.busy.fetch_add(1, Ordering::SeqCst);
        shared.touch();
        InFlight(shared)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

fn control(shared: &Arc<Shared>, mut connection: TcpStream) -> std::io::Result<()> {
    connection.set_read_timeout(Some(Duration::from_secs(5)))?;
    connection.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(connection.try_clone()?);
    let Some(head) = Head::read(&mut reader)? else { return Ok(()) };
    let reply = |connection: &mut TcpStream, status: &str, body: &Value| http::respond(connection, status, Some("application/json"), body.to_string().as_bytes(), false);
    if !crate::core::loopback_host(head.header("Host").unwrap_or_default()) {
        return reply(&mut connection, "403 Forbidden", &json!({ "error": "host_not_loopback" }));
    }
    if head.header("Origin").is_some() {
        return reply(&mut connection, "403 Forbidden", &json!({ "error": "browser_origin_refused" }));
    }
    if !crate::core::same(head.header("Authorization").unwrap_or_default(), &format!("Bearer {}", shared.token)) {
        return reply(&mut connection, "401 Unauthorized", &json!({ "error": "unauthorized" }));
    }
    if shared.leaving.load(Ordering::SeqCst) && head.path() != "/link/quit" {
        return reply(&mut connection, "503 Service Unavailable", &json!({ "error": "The link is ending." }));
    }
    let _request = InFlight::begin(shared);
    let body = match head.request_body()? {
        Framing::Length(n) if n <= MAX_BODY => http::read_body(&mut reader, Framing::Length(n))?,
        Framing::Length(_) => return reply(&mut connection, "413 Payload Too Large", &json!({ "error": "too_large" })),
        _ => return reply(&mut connection, "400 Bad Request", &json!({ "error": "a body needs a Content-Length" })),
    };
    match (head.method(), head.path()) {
        ("GET", "/link/status") => reply(&mut connection, "200 OK", &json!(shared.status())),
        ("POST", "/link/start") => {
            let (job, only_running, install) = if body.is_empty() {
                (None, false, false)
            } else {
                let Ok(asked) = serde_json::from_slice::<Value>(&body) else { return reply(&mut connection, "400 Bad Request", &json!({ "error": "the body isn't JSON" })) };
                let Ok(job) = serde_json::from_value::<Option<JobRequest>>(asked.get("job").cloned().unwrap_or(Value::Null)) else {
                    return reply(&mut connection, "400 Bad Request", &json!({ "error": "job isn't a job request" }));
                };
                (job, asked["only_running"] == true, asked["install"] == true)
            };
            shared.session.request(&if only_running { Want::Attach { install } } else { Want::Start { job, install } });
            reply(&mut connection, "200 OK", &json!(shared.status()))
        }
        ("POST", "/link/install") => {
            shared.session.allow_install();
            reply(&mut connection, "200 OK", &json!(shared.status()))
        }
        ("POST", "/link/stop") => match shared.session.stop() {
            Ok(()) => reply(&mut connection, "200 OK", &json!({ "ok": true })),
            Err(message) => reply(&mut connection, "409 Conflict", &json!({ "error": message })),
        },
        ("POST", "/link/quit") => {
            if start_leaving(shared) {
                let _ = reply(&mut connection, "200 OK", &json!({ "ok": true }));
                finish_leaving(shared)
            }
            reply(&mut connection, "200 OK", &json!({ "ok": true }))
        }
        (_, "/link/status" | "/link/start" | "/link/install" | "/link/stop" | "/link/quit") => reply(&mut connection, "405 Method Not Allowed", &json!({ "error": "method_not_allowed" })),
        _ => reply(&mut connection, "404 Not Found", &json!({ "error": "not_found" })),
    }
}

/// A stop signal asks the link to leave (it is read on a thread of its own: `watch`).
/// Handlers, and not a blocked mask, since a mask would pass on to `ssh` and the helper.
#[cfg(unix)]
fn catch_stop_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::SeqCst);
    }
    // SAFETY: a handler that only sets an atomic; the mask is cleared of the signals first.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Ctrl-C, Ctrl-Break and the console closing ask the link to leave.
#[cfg(windows)]
fn catch_stop_signals() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn on_console_event(_: u32) -> windows_sys::core::BOOL {
        SIGNALLED.store(true, Ordering::SeqCst);
        // Windows ends the process once this returns for a closing console; `watch` exits first.
        std::thread::sleep(Duration::from_secs(10));
        1
    }
    // SAFETY: a handler that only touches an atomic and sleeps, for the life of the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
        eprintln!("endeavor link: couldn't take Ctrl-C ({})", std::io::Error::last_os_error());
    }
}

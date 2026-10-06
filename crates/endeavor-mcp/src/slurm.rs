//! The Slurm launcher. On the login node, `connect --launcher slurm` submits
//! a batch job whose script is `endeavor node-start`, which starts the
//! runtime on the compute node and writes `runtime.json` (with the job id) to
//! the state folder, shared with the login node. While the job waits in the
//! queue the app hears its state; once the runtime is up, the login helper
//! starts `endeavor relay` on the node, through `srun --overlap` inside
//! the job or else `ssh` to the node, and passes the app's streams through its
//! stdin and stdout. The runtime stays on the node's loopback.
//!
//! `job.json` records a submitted job until its runtime is up, so a reconnect
//! (from any login node) goes on waiting for it instead of submitting another.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::ChildStdin;
use std::sync::atomic::AtomicU64;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use wire::slurm::{self as s, Job, Queued};

use super::*;

/// How often to ask `squeue` about a job that waits.
fn poll() -> Duration {
    let ms = std::env::var("ENDEAVOR_SLURM_POLL_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    Duration::from_millis(ms)
}

/// How long the relay on the node has to say it's attached.
const RELAY_TIMEOUT: Duration = Duration::from_secs(60);

/// The runtime's job while this helper relays to it.
pub struct Running {
    job: String,
    /// The runtime's pid on the node, as `runtime.json` has it.
    pid: i32,
    node: String,
    ends_at: Option<u64>,
    route: &'static str,
    link: Arc<Link>,
    state_dir: PathBuf,
}

impl Running {
    pub fn generation(&self) -> u64 {
        self.link.generation
    }

    pub fn link(&self) -> Arc<Link> {
        self.link.clone()
    }

    pub fn info(&self) -> Job {
        Job { id: self.job.clone(), node: self.node.clone(), ends_at: self.ends_at, route: self.route.into() }
    }

    /// The relay ended while the job may still run: start another.
    pub fn reconnect(&mut self, mux: &Arc<Mux>, events: &Sender<Event>) -> Result<(), String> {
        self.link.kill();
        match squeue(&self.job) {
            Ok(Some(q)) if q.running() => {}
            _ => return Err("the job isn't running".into()),
        }
        let (link, route) = connect_node(&self.job, &self.node, &self.state_dir, mux, events)?;
        self.link = link;
        self.route = route;
        Ok(())
    }

    /// The runtime or its job ended: why, in plain words, and what Julia last said.
    pub fn ended(&self, dir: &Path, said: Option<(String, Vec<String>)>) -> ToApp {
        self.link.kill();
        let reason = match stopped::why(dir, stopped::Of::Runtime(self.pid)) {
            Some(how) => Some(stopped_text(how)),
            None => end_reason(&self.job, dir),
        };
        // The job ends with Julia (its script execs it); make sure.
        scancel(&self.job);
        forget(dir, &self.job);
        let (status, log_tail) = said.unwrap_or_else(|| ("exited".into(), log_tail(&dir.join("runtime.log"))));
        ToApp::Died { status: reason.map(str::to_owned).unwrap_or(status), log_tail }
    }

    /// Ask the runtime to shut down through the relay, then cancel the job.
    /// The caller holds the start lock.
    pub fn stop(self, rx: &mpsc::Receiver<Event>, said: &mut Said) {
        let generation = self.link.generation;
        if self.link.send(&ToHelper::Stop).is_ok() {
            let deadline = Instant::now() + Duration::from_secs(20);
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(Event::Node(g, ToApp::Stopped | ToApp::Died { .. })) | Ok(Event::NodeGone(g)) if g == generation => break,
                    Ok(event) => said.note(event),
                    Err(_) => break,
                }
            }
        }
        self.link.kill();
        scancel(&self.job);
        forget_locked(&self.state_dir, &self.job);
    }
}

/// The relay on the job's node: its stdin takes the app's streams, and the
/// stream ids it has open, to close them for the app if it goes.
pub struct Link {
    stdin: Mutex<ChildStdin>,
    forwarded: Mutex<HashSet<u32>>,
    child: Mutex<Child>,
    generation: u64,
}

impl Link {
    pub fn open(&self, id: u32) -> std::io::Result<()> {
        self.forwarded.lock().unwrap().insert(id);
        Frame::Open { id }.write_to(&mut *self.stdin.lock().unwrap())
    }

    pub fn forward(&self, frame: Frame) {
        if let Frame::Close { id } = &frame {
            self.forwarded.lock().unwrap().remove(id);
        }
        let _ = frame.write_to(&mut *self.stdin.lock().unwrap());
    }

    fn send(&self, message: &ToHelper) -> std::io::Result<()> {
        message.frame().write_to(&mut *self.stdin.lock().unwrap())
    }

    fn kill(&self) {
        let mut child = self.child.lock().unwrap();
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        // SIGTERM first: srun then ends its step on the node.
        // SAFETY: plain syscall on our own child.
        #[cfg(unix)]
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Take the cluster runtime over: the one running in a job, a job already
/// waiting, or a new job. The start lock is held while it reads `job.json`,
/// decides and submits, so two helpers never submit two jobs, and let go
/// before the wait in the queue: a helper that comes in meanwhile finds
/// `job.json` and waits for the same job.
pub fn attach(args: &Args, mux: &Arc<Mux>, rx: &mpsc::Receiver<Event>, events: &Sender<Event>, parts: &Parts, request: JobRequest) -> Result<Attached, ToApp> {
    let failed = |message: String| ToApp::StartFailed { message };
    let starting = lock_start(args, mux, rx, parts)?;
    let dir = &args.state_dir;
    if let Some(attached) = running(args, mux, events)? {
        return Ok(attached);
    }
    let job = match waiting_job(dir) {
        Some((job, summary)) => {
            let _ = mux.send(&ToApp::Submitted { job: job.clone(), summary }.frame());
            job
        }
        // A helper waiting for the job removes `job.json` once the runtime is up, which this didn't see before.
        None => match running(args, mux, events)? {
            Some(attached) => return Ok(attached),
            None => {
                stopped::clear(dir);
                submit(args, mux, &request).map_err(failed)?
            }
        },
    };
    drop(starting);
    let (state, running) = wait(args, mux, rx, events, &job)?;
    Ok(Attached { how: How::Slurm(running), state, reattached: false })
}

/// The runtime recorded as running in a job, attached to through a relay on its node.
fn running(args: &Args, mux: &Arc<Mux>, events: &Sender<Event>) -> Result<Option<Attached>, ToApp> {
    let failed = |message: String| ToApp::StartFailed { message };
    let dir = &args.state_dir;
    if let Some(state) = read_state(dir).filter(|s| s.launcher == "slurm")
        && let Some(job) = state.job.clone()
    {
        match squeue(&job).map_err(failed)? {
            Some(_) if state.port.is_none() => return Err(failed(OLDER_RUNTIME.into())),
            Some(q) if q.running() => {
                let ends_at = ends_at(&q);
                let (link, route) = connect_node(&job, &state.node, dir, mux, events).map_err(failed)?;
                let running = Running { job, pid: state.pid, node: state.node.clone(), ends_at, route, link, state_dir: dir.clone() };
                return Ok(Some(Attached { how: How::Slurm(running), state, reattached: true }));
            }
            _ => {
                let _ = std::fs::remove_file(dir.join("runtime.json"));
            }
        }
    }
    Ok(None)
}

/// A job an earlier connect submitted that's still queued or starting: (id, summary).
fn waiting_job(dir: &Path) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("job.json")).ok()?).ok()?;
    let job = v["job"].as_str()?.to_owned();
    match squeue(&job) {
        Ok(Some(q)) if q.pending() || q.running() => Some((job, v["summary"].as_str().unwrap_or_default().to_owned())),
        _ => {
            let _ = std::fs::remove_file(dir.join("job.json"));
            None
        }
    }
}

/// What runs from `dir`, found without taking it over: the job the runtime
/// runs in, or a job still waiting for a node or for Julia to start.
pub fn check(dir: &Path) -> RuntimeState {
    if let Some(state) = read_state(dir).filter(|s| s.launcher == "slurm")
        && let Some(job) = state.job
        && let Ok(Some(q)) = squeue(&job)
        && q.running()
    {
        let info = Job { id: job, node: q.node.clone(), ends_at: ends_at(&q), route: String::new() };
        return RuntimeState::Running { node: state.node, notebooks: None, job: Some(info) };
    }
    let recorded = std::fs::read_to_string(dir.join("job.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if let Some(job) = recorded.as_ref().and_then(|v| v["job"].as_str())
        && let Ok(Some(q)) = squeue(job)
    {
        if q.pending() {
            return RuntimeState::Queued { job: job.to_owned(), state: q.state, reason: q.reason };
        }
        if q.running() {
            return RuntimeState::Queued { job: job.to_owned(), state: q.state, reason: q.node };
        }
    }
    RuntimeState::NotRunning
}

/// Cancel the job recorded in `dir`: the one running the runtime, or one
/// still waiting for a node.
/// The caller holds the start lock.
pub fn cancel_recorded(dir: &Path) {
    if let Some(state) = read_state(dir).filter(|s| s.launcher == "slurm")
        && let Some(job) = state.job
    {
        stopped::mark(dir, stopped::Of::Runtime(state.pid), stopped::How::Connection);
        scancel(&job);
        forget_locked(dir, &job);
    }
    if let Some((job, _)) = waiting_job(dir) {
        stopped::mark(dir, stopped::Of::Job(&job), stopped::How::Connection);
        scancel(&job);
        forget_locked(dir, &job);
    }
}

/// Find Julia (here, on the shared filesystem), write the job script and
/// submit it. The job's id.
fn submit(args: &Args, mux: &Arc<Mux>, request: &JobRequest) -> Result<String, String> {
    let dir = &args.state_dir;
    let (julia, version) = julia::find(&args.julia, &|line| drop(mux.send(&ToApp::Progress { line }.frame())))?;
    let _ = mux.send(&ToApp::FoundJulia { path: julia.clone(), version }.frame());
    token(dir)?;
    let depot = match request.depot.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{}:", wire::files::expand(d).display()),
        None => match s::scratch() {
            Some(scratch) => format!("{scratch}/endeavor/depot:"),
            None => args.depot.clone(),
        },
    };
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the helper itself: {e}"))?;
    let build = args.build.as_deref().map_or(String::new(), |build| format!(" --build {}", quote(build)));
    let exit_idle = if args.exit_idle { " --exit-idle" } else { "" };
    let script = format!(
        "#!/bin/sh\n# Endeavor's Julia for this cluster, submitted by endeavor.\nexec {} node-start --state-dir {} --julia {} --runtime {} --depot {}{build}{exit_idle}\n",
        quote(&exe.display().to_string()),
        quote(&dir.display().to_string()),
        quote(&julia),
        quote(&args.runtime.display().to_string()),
        quote(&depot),
    );
    let script_path = dir.join("job.sh");
    std::fs::write(&script_path, script).map_err(|e| format!("Couldn't write {}: {e}", script_path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&script_path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).map_err(|e| e.to_string())?;
    let log = dir.join("runtime.log");
    // The log shows Pluto's secret URL.
    owner_only(OpenOptions::new().write(true).create(true).truncate(true)).open(&log).map_err(|e| format!("Couldn't open {}: {e}", log.display()))?;
    let _ = std::fs::remove_file(dir.join("runtime.json"));

    let output = Command::new("sbatch")
        .args(["--parsable", "--job-name=endeavor", "--nodes=1", "--ntasks=1", "--open-mode=append"])
        .arg(format!("--output={}", log.display()))
        .args(request.sbatch_args())
        .arg(&script_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run sbatch: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let job = stdout.trim().split(';').next().unwrap_or_default().to_owned();
    if !output.status.success() || job.is_empty() || !job.chars().all(|c| c.is_ascii_digit() || c == '_') {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.lines().map(|l| l.trim_start_matches("sbatch: error: ").trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        return Err(format!("The cluster didn't take the job: {said}"));
    }
    let summary = request.resources.summary();
    let record = json!({ "job": job, "summary": summary });
    std::fs::write(dir.join("job.json"), record.to_string()).map_err(|e| e.to_string())?;
    let _ = mux.send(&ToApp::Submitted { job: job.clone(), summary }.frame());
    Ok(job)
}

/// Wait for the job to start and its runtime to come up, telling the app how
/// it's queued, then connect to it. Stop cancels the job; the app leaving
/// leaves it queued, for the next connect.
fn wait(args: &Args, mux: &Arc<Mux>, rx: &mpsc::Receiver<Event>, events: &Sender<Event>, job: &str) -> Result<(State, Running), ToApp> {
    let dir = &args.state_dir;
    let ready = Arc::new(AtomicBool::new(false));
    let log_done = Arc::new(Exit::default());
    let mut log = None;
    let mut last: Option<(String, String)> = None;
    let result = loop {
        match rx.recv_timeout(poll()) {
            Ok(Event::App(ToHelper::Stop)) => {
                stopped::mark(dir, stopped::Of::Job(job), stopped::How::Connection);
                scancel(job);
                forget(dir, job);
                break Err(ToApp::Stopped);
            }
            Ok(Event::App(ToHelper::Detach) | Event::Eof) => std::process::exit(0),
            Ok(_) => continue,
            Err(RecvTimeoutError::Disconnected) => unreachable!("the watchers hold senders"),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let q = match squeue(job) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("endeavor: squeue: {e}");
                continue;
            }
        };
        match q {
            Some(q) if q.pending() => {
                let now = (q.state.clone(), q.reason.clone());
                if last.as_ref() != Some(&now) {
                    let _ = mux.send(&ToApp::Queued { job: job.into(), state: q.state, reason: q.reason }.frame());
                    last = Some(now);
                }
            }
            Some(q) if q.running() => {
                if last.as_ref().is_none_or(|(state, _)| state != "RUNNING") {
                    // The reason slot carries the node it got.
                    let _ = mux.send(&ToApp::Queued { job: job.into(), state: q.state.clone(), reason: q.node.clone() }.frame());
                    last = Some((q.state.clone(), q.node.clone()));
                }
                if log.is_none() {
                    log = Some(follow_log(dir.join("runtime.log"), mux.clone(), ready.clone(), log_done.clone()));
                }
                if let Some(state) = read_state(dir).filter(|s| s.job.as_deref() == Some(job)) {
                    // Its progress lines go out before the relay's.
                    ready.store(true, Ordering::SeqCst);
                    if let Some(log) = log.take() {
                        let _ = log.join();
                    }
                    under_start_lock(dir, || forget_job_record(dir, job));
                    // Before connecting, which can take a while: `q` says how long was left when it was asked.
                    let ends_at = ends_at(&q);
                    match connect_node(job, &state.node, dir, mux, events) {
                        Ok((link, route)) => {
                            let running = Running { job: job.into(), pid: state.pid, node: state.node.clone(), ends_at, route, link, state_dir: dir.clone() };
                            break Ok((state, running));
                        }
                        Err(e) => {
                            scancel(job);
                            forget(dir, job);
                            break Err(ToApp::StartFailed {
                                message: format!("Julia started on {} (job {job}), but Endeavor couldn't reach it there, so the job was cancelled. {e}", state.node),
                            });
                        }
                    }
                }
            }
            // Completing, or gone from the queue.
            _ => {
                if let Some(how) = stopped::why(dir, stopped::Of::Job(job)) {
                    break Err(ToApp::StartFailed { message: format!("The start was stopped. {}", stopped_text(how)) });
                }
                let reason = end_reason(job, dir).unwrap_or("Its Slurm job ended.");
                let tail = log_tail(&dir.join("runtime.log"));
                let said = tail.iter().rev().find(|l| !l.trim().is_empty()).map(|l| format!(" Its last output: {}", l.trim())).unwrap_or_default();
                forget(dir, job);
                break Err(ToApp::StartFailed { message: format!("{reason} Julia wasn't ready yet.{said}") });
            }
        }
    };
    ready.store(true, Ordering::SeqCst);
    log_done.set(0, String::new(), &mpsc::channel().0);
    if let Some(log) = log {
        let _ = log.join();
    }
    result
}

fn ends_at(q: &Queued) -> Option<u64> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    q.left.map(|left| now + left)
}

/// Drop what's recorded about `job` (it ended or was cancelled), unless the
/// records have moved on to another job: helpers that shared `job` call this
/// late, after one of them may have submitted the next. Whoever submits does it
/// under the start lock, so the check and the removal are made under it too. A
/// start that holds it for long isn't waited for: the records stay, and
/// `waiting_job` drops a `job.json` whose job is gone.
fn forget(dir: &Path, job: &str) {
    under_start_lock(dir, || forget_locked(dir, job));
}

/// Run `f` with the start lock, if it's free within a few seconds.
fn under_start_lock(dir: &Path, f: impl FnOnce()) {
    let pause = || {
        std::thread::sleep(Duration::from_millis(100));
        Ok::<(), std::convert::Infallible>(())
    };
    if let Ok(_lock) = standalone::wait_for_start_lock(dir, Duration::from_secs(5), pause) {
        f();
    }
}

/// `forget`, for a caller that holds the start lock.
fn forget_locked(dir: &Path, job: &str) {
    forget_job_record(dir, job);
    if read_state(dir).is_some_and(|s| s.job.as_deref() == Some(job)) {
        let _ = std::fs::remove_file(dir.join("runtime.json"));
    }
}

/// Remove `job.json` if it records `job`.
fn forget_job_record(dir: &Path, job: &str) {
    let recorded = std::fs::read_to_string(dir.join("job.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if recorded.is_some_and(|v| v["job"].as_str() == Some(job)) {
        let _ = std::fs::remove_file(dir.join("job.json"));
    }
}

/// Start the relay on the job's node: through `srun --overlap` inside the job
/// (no SSH between cluster nodes needed), else `ssh` to the node (for clusters
/// whose Slurm is too old for `--overlap`). Which one, and why.
fn connect_node(job: &str, node: &str, dir: &Path, mux: &Arc<Mux>, events: &Sender<Event>) -> Result<(Arc<Link>, &'static str), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.display().to_string();
    let dir = dir.display().to_string();
    let mut failures = Vec::new();
    if s::has("srun") {
        let mut srun = Command::new("srun");
        // Without --unbuffered srun holds the relay's output until a newline, and frames are binary.
        srun.arg(format!("--jobid={job}")).args(["--overlap", "--unbuffered", "--nodes=1", "--ntasks=1", "--quiet"]).arg(&exe).args(["relay", "--state-dir", &dir]);
        match spawn_relay(srun, mux, events) {
            Ok(link) => {
                let _ = mux.send(&ToApp::Progress { line: format!("Reached {node} through srun (inside job {job})") }.frame());
                return Ok((link, "srun"));
            }
            Err(e) => failures.push(format!("srun: {e}")),
        }
    } else {
        failures.push("srun: not found".into());
    }
    let mut ssh = Command::new("ssh");
    ssh.args(["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=20", "--", node])
        .arg(format!("{} relay --state-dir {}", quote(&exe), quote(&dir)));
    match spawn_relay(ssh, mux, events) {
        Ok(link) => {
            let why = failures.join("; ");
            let _ = mux.send(&ToApp::Progress { line: format!("Reached {node} through ssh ({why})") }.frame());
            Ok((link, "ssh"))
        }
        Err(e) => {
            failures.push(format!("ssh: {e}"));
            Err(format!("Neither srun nor ssh reached {node}. {}", failures.join("; ")))
        }
    }
}

/// Run `command` (a relay on the node) and wait for it to attach. Its frames
/// go on to the app; its control messages become `Event::Node`.
fn spawn_relay(mut command: Command, mux: &Arc<Mux>, events: &Sender<Event>) -> Result<Arc<Link>, String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let generation = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let stderr = child.stderr.take().unwrap();
        let lines = stderr_lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("relay: {line}");
                lines.lock().unwrap().push(line);
            }
        });
    }
    let link = Arc::new(Link { stdin: Mutex::new(stdin), forwarded: Mutex::default(), child: Mutex::new(child), generation });
    let (first_tx, first) = mpsc::channel::<ToApp>();
    let early: Arc<Mutex<Option<Sender<ToApp>>>> = Arc::new(Mutex::new(Some(first_tx)));
    {
        let (link, mux, events, early) = (link.clone(), mux.clone(), events.clone(), early.clone());
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            while let Ok(Some(frame)) = Frame::read_from(&mut stdout) {
                match frame {
                    Frame::Control(json) => {
                        let Ok(message) = serde_json::from_slice::<ToApp>(&json) else { continue };
                        match &*early.lock().unwrap() {
                            Some(first) => drop(first.send(message)),
                            None => drop(events.send(Event::Node(generation, message))),
                        }
                    }
                    Frame::Close { id } => {
                        link.forwarded.lock().unwrap().remove(&id);
                        let _ = mux.send(&frame);
                    }
                    Frame::Data { .. } => drop(mux.send(&frame)),
                    Frame::Open { .. } => {}
                }
            }
            early.lock().unwrap().take();
            let open: Vec<u32> = link.forwarded.lock().unwrap().drain().collect();
            for id in open {
                let _ = mux.send(&Frame::Close { id });
            }
            let _ = events.send(Event::NodeGone(generation));
        });
    }
    let deadline = Instant::now() + RELAY_TIMEOUT;
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match first.recv_timeout(left) {
            Ok(ToApp::Hello { .. } | ToApp::Progress { .. }) => {}
            Ok(ToApp::Ready { .. }) => break Ok(()),
            Ok(ToApp::StartFailed { message } | ToApp::Error { message }) => break Err(message),
            Ok(other) => break Err(format!("it said {other:?}")),
            Err(RecvTimeoutError::Timeout) => break Err(format!("no answer in {} s", RELAY_TIMEOUT.as_secs())),
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(200));
                let lines = stderr_lines.lock().unwrap();
                let said = lines.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("it exited");
                break Err(said.to_owned());
            }
        }
    };
    early.lock().unwrap().take();
    match result {
        Ok(()) => Ok(link),
        Err(e) => {
            link.kill();
            Err(e)
        }
    }
}

/// `squeue` on one job: None once it's no longer listed; an error when
/// `squeue` itself failed (the controller not answering, say).
fn squeue(job: &str) -> Result<Option<Queued>, String> {
    let output = Command::new("squeue")
        .args(["-h", "-j", job, "-o", s::SQUEUE_FORMAT])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run squeue: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if stderr.contains("Invalid job id") {
            return Ok(None);
        }
        return Err(format!("squeue failed: {}", stderr.trim()));
    }
    Ok(s::parse_squeue(&String::from_utf8_lossy(&output.stdout)))
}

fn scancel(job: &str) {
    let _ = Command::new("scancel").arg(job).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Why `job` ended, if Slurm (or its last words in the log) says: `sacct`,
/// else `squeue` while it still lists finished jobs, else the log.
fn end_reason(job: &str, dir: &Path) -> Option<&'static str> {
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let output = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_owned())
    };
    // sacct can lag the job's end by a moment.
    for _ in 0..3 {
        if let Some(state) = run("sacct", &["-j", job, "-X", "-n", "-P", "-o", "State"]) {
            if let Some(text) = s::ended_text(&state) {
                return Some(text);
            }
            if state.starts_with("COMPLETED") {
                return None;
            }
        }
        if let Some(text) = run("squeue", &["-h", "-j", job, "-t", "all", "-o", "%T"]).as_deref().and_then(s::ended_text) {
            return Some(text);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let log = log_tail(&dir.join("runtime.log")).join("\n");
    if log.contains("DUE TO TIME LIMIT") {
        Some(s::ended_text("TIMEOUT").unwrap())
    } else if log.contains("DUE TO PREEMPTION") {
        Some(s::ended_text("PREEMPTED").unwrap())
    } else if log.contains("oom-kill") || log.contains("Out Of Memory") {
        Some(s::ended_text("OUT_OF_MEMORY").unwrap())
    } else if log.contains("CANCELLED AT") {
        Some(s::ended_text("CANCELLED").unwrap())
    } else {
        None
    }
}

/// `text` as one word for `sh`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter().position(|a| a == name).and_then(|i| argv.get(i + 1)).cloned()
}

/// `endeavor relay --state-dir DIR`, on a job's node: attach to the
/// runtime recorded there and relay stdin/stdout to its ports, until the
/// runtime goes, the login helper stops it, or its input ends.
pub fn relay_main(argv: &[String]) -> ! {
    let Some(dir) = flag(argv, "--state-dir").map(PathBuf::from) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    block_sigusr1();
    let mux = stdout_mux();
    let (events, rx) = mpsc::channel();
    let home = wire::files::home().display().to_string();
    let _ = mux.send(&ToApp::Hello { version: env!("CARGO_PKG_VERSION").into(), node: hostname(), home, slurm: false, uploads: false }.frame());
    let here = hostname();
    let state = read_state(&dir).filter(|s| s.node == here && alive(s)).and_then(|s| Some((s.port?, s)));
    let Some((port, state)) = state else {
        let _ = mux.send(&ToApp::StartFailed { message: format!("Julia isn't running on {here}.") }.frame());
        std::process::exit(1);
    };
    let runtime = Runtime::recorded(&state, &dir, &events);
    relay_stdin(mux.clone(), Arc::new(RwLock::new(Route::Local(port))), events, Arc::new(wire::files::answer), Parts::default());
    let ready = ToApp::Ready {
        launcher: state.launcher.clone(),
        node: state.node.clone(),
        pid: state.pid as u32,
        token: state.token.clone(),
        reattached: true,
        job: None,
    };
    let _ = mux.send(&ready.frame());
    loop {
        match rx.recv().expect("senders live as long as their threads") {
            Event::App(ToHelper::Stop) => {
                runtime.stop(Some(&state));
                let _ = mux.send(&ToApp::Stopped.frame());
                std::process::exit(0);
            }
            Event::App(ToHelper::Detach) | Event::Eof => std::process::exit(0),
            Event::Exited(pid, status) if pid == state.pid => {
                let _ = mux.send(&runtime.died(status).frame());
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

/// `endeavor node-start …`, the job's script: become the core on this
/// node, which starts Julia, logging to the job's output.
pub fn node_start_main(argv: &[String]) -> ! {
    let need = |name: &str| {
        flag(argv, name).unwrap_or_else(|| {
            eprintln!("{name} is required\n{USAGE}");
            std::process::exit(2);
        })
    };
    let (dir, julia, runtime, depot) = (PathBuf::from(need("--state-dir")), need("--julia"), PathBuf::from(need("--runtime")), need("--depot"));
    let fail = |message: String| -> ! {
        eprintln!("endeavor: {message}");
        std::process::exit(1);
    };
    let token = std::fs::read_to_string(dir.join("token")).unwrap_or_else(|e| fail(format!("Couldn't read the token in {}: {e}", dir.display())));
    let mut command = runtime_command(&julia, &runtime, &depot, token.trim(), &dir, "slurm", flag(argv, "--build").as_deref()).unwrap_or_else(|e| fail(e));
    if argv.iter().any(|a| a == "--exit-idle") {
        command.env("ENDEAVOR_EXIT_IDLE", "1");
    }
    let error = exec(command);
    fail(format!("Couldn't start the runtime: {error}"))
}

#[cfg(unix)]
fn exec(mut command: Command) -> std::io::Error {
    command.exec()
}

/// A cluster's nodes run Linux, so a job never runs this on Windows.
#[cfg(windows)]
fn exec(_command: Command) -> std::io::Error {
    std::io::ErrorKind::Unsupported.into()
}

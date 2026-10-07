//! The front on a machine (docs/plugins-and-remote.md): where this session's
//! notebooks run, the four machine tools the front answers itself, and what a
//! call to a runtime on a machine goes through: the machine's link (`link`),
//! which gives the runtime's port and token on this computer.
//!
//! A session is on this computer or on one machine. Its calls go to the
//! runtime there, a machine's with `X-Endeavor-Host` and `X-Endeavor-Browser-Port`.
//! When the session moves to another runtime its key on the old one is ended and
//! it gets a new key, so nothing of its one-notebook binding carries over.

use std::sync::{Arc, Mutex, MutexGuard, TryLockError, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wire::slurm::{JobRequest, Partition, Resources, check_extra_flag};

use super::projects::Remembered;
use super::{Relay, Route, Status as Local, start_wait, tool_failure};
use crate::client::{Cluster, Running, Server, ssh_config_hosts};
use crate::link::{self, Link, State};
use crate::mcp::{browser_link, to_json, tool_error};

/// A tool call's result that is `text`.
pub(super) fn text_result(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

/// What `use_machine` and `stop_machine` call this computer.
pub(super) const LOCAL: &str = "local";

/// How often a call that waits for a machine asks its link how it is.
const POLL: Duration = Duration::from_millis(250);

/// How long a link may say `connected` after it was asked to start something before that is taken as nothing running.
const GRACE: Duration = Duration::from_secs(3);

/// Another session counts as active in a notebook if it called a tool this lately.
const RECENT_SECONDS: u64 = 15 * 60;

/// A stop waits for the helper as long as is left of the call's time, but at least this long: the
/// helper may take longer, and the link's own wait is longer still.
const STOP_FLOOR: Duration = Duration::from_secs(2);

/// How often a front asks its link how it is, so that the link's idle exit counts from the end of the session.
const PING_EVERY: Duration = Duration::from_secs(240);

/// How long a runtime may say nothing to the question of who else is active before `stop_machine` gives up on it.
const CHECK_WAIT: Duration = Duration::from_secs(5);

/// A link call isn't started with less than this left of the call's time.
const MIN_CALL: Duration = Duration::from_millis(300);

/// The time one machine tool call has, counted from the moment it arrived: waiting for the
/// other machine tool call, finding or starting the link, and every call to it come out of it.
#[derive(Clone, Copy)]
pub(super) struct Deadline(Instant);

impl Deadline {
    pub(super) fn after(wait: Duration) -> Deadline {
        Deadline(Instant::now() + wait)
    }

    fn left(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// Too little time is left to ask the link anything.
    fn spent(self) -> bool {
        self.left() < MIN_CALL
    }

    /// How long a call to the link may take, or the plain error when too little time is left.
    fn call_wait(self) -> Result<Duration, String> {
        if self.spent() {
            return Err("This call ran out of the time a tool call gets before it could ask the link. Call it again to continue.".into());
        }
        Ok(self.left().min(link::CALL_WAIT))
    }

    fn status(self, link: &Link) -> Result<link::Status, String> {
        link.status(self.call_wait()?)
    }

    /// The status once more as the time runs out, so that what a request started is reported: it may take a second past the deadline.
    fn last_status(self, link: &Link) -> Result<link::Status, String> {
        link.status(self.left().clamp(Duration::from_secs(1), link::CALL_WAIT))
    }

    /// A start or attach request to `link`, sent whatever its protocol: `Relay::ask` is the one that holds back.
    fn send(self, link: &Link, ask: Ask, install: bool) -> Result<link::Status, String> {
        match ask {
            Ask::Attach => link.attach(install, self.call_wait()?),
            Ask::Start(job) => link.start(job, install, self.call_wait()?),
        }
    }

    /// `f`, given up on (it goes on in the background) when the time is out.
    fn run<T: Send + 'static>(self, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || drop(done.send(f())));
        waited.recv_timeout(self.left()).map_err(|_| "This call ran out of the time a tool call gets while it waited for the link. Call it again to continue.".to_owned())
    }

    /// Tell the link the user agreed to install the helper.
    fn install(self, link: &Link) -> Result<link::Status, String> {
        link.install(self.call_wait()?)
    }

    fn ensure(self, server: &Server) -> Result<Link, String> {
        let server = server.clone();
        self.run(move || link::ensure(&server))?
    }

    /// `ensure`, and a link that has to be started may install the helper at once.
    fn ensure_install(self, server: &Server, install: bool) -> Result<Link, String> {
        let server = server.clone();
        self.run(move || link::ensure_install(&server, install))?
    }

    fn find(self, id: &str) -> Result<Option<Link>, String> {
        let id = id.to_owned();
        self.run(move || link::find(&id))?
    }
}

/// What a start request to a link asks for.
pub(super) enum Ask {
    /// Attach to a runtime that is there, or a job that waits; start nothing.
    Attach,
    /// Start the runtime (on a cluster, with the job).
    Start(Option<JobRequest>),
}

/// Where this session's notebooks run.
#[derive(Clone)]
pub(super) enum Target {
    /// On this computer. `stopped`: `stop_machine` ended its runtime, and calls say so until `use_machine`.
    Local { stopped: bool },
    Machine(Machine),
}

#[derive(Clone)]
pub(super) struct Machine {
    /// The id in the machines file, which the link goes by.
    pub id: String,
    /// What the agent calls it.
    pub name: String,
    pub cluster: bool,
    /// The session's folder there, if `use_machine` was given one; else the server's home.
    pub folder: Option<String>,
    pub link: Option<Link>,
    /// The session should have a runtime there: false after `stop_machine`.
    pub active: bool,
    /// The link process (its pid) that was asked to attach to or start the runtime.
    pub asked: Option<u32>,
    /// The runtime (its pid) that was told this session's folder.
    told: Option<u32>,
}

/// Why a call can't go to a runtime yet. `status` is the link's when it was reached.
pub(super) struct NotReady {
    name: String,
    message: String,
    status: Option<link::Status>,
}

impl NotReady {
    fn plain(message: impl Into<String>) -> NotReady {
        NotReady { name: String::new(), message: message.into(), status: None }
    }

    fn of(name: &str, status: &link::Status, message: impl Into<String>) -> NotReady {
        NotReady { name: name.to_owned(), message: message.into(), status: Some(status.clone()) }
    }
}

fn invalid(message: impl std::fmt::Display) -> String {
    format!("ArgumentError: invalid_argument::{message}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn state_word(state: State) -> &'static str {
    match state {
        State::Connecting => "connecting",
        State::Connected => "connected",
        State::Starting => "starting",
        State::Queued => "queued",
        State::Ready => "ready",
        State::Failed => "failed",
        State::NeedsInstall => "needs_install",
        State::Unknown => "unknown",
    }
}

/// Slurm's reason for a waiting job, in words for the user.
fn queue_reason_text(reason: &str) -> String {
    match reason {
        "" | "None" => "no reason given yet".into(),
        "Priority" => "other jobs are ahead of it".into(),
        "Resources" => "it waits for the resources it asked for to be free".into(),
        "Dependency" => "it waits for another job to end".into(),
        "BeginTime" => "its start time is in the future".into(),
        "ReqNodeNotAvail" => "the nodes it needs are not available (reserved or down)".into(),
        "JobHeldUser" | "JobHeldAdmin" => "the job is held".into(),
        other if other.starts_with("QOS") || other.starts_with("Assoc") || other.contains("Limit") => format!("a limit on the account or user applies ({other})"),
        other => other.to_owned(),
    }
}

/// What a link in a state that isn't ready says, for the agent to relay and act on.
fn not_ready_message(name: &str, status: &link::Status) -> String {
    let step = status.step.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" Last step: {s}")).unwrap_or_default();
    match status.state {
        State::Connecting => format!("Endeavor is connecting to {name}.{step} Wait a little, then call `pluto_session_status` to see how far it got. If it stays like this, call `use_machine` again."),
        State::Connected => format!("Julia isn't running on {name} right now. Call `use_machine` with machine \"{name}\" to start it."),
        State::Starting => format!("Julia is starting on {name}. The first start installs packages and takes a few minutes.{step} Wait, then call `pluto_session_status` to see how far it got."),
        State::Queued => {
            let job = status.job.as_ref().map(|j| format!(" {}", j.id)).unwrap_or_default();
            let (state, reason) = status.queue.as_ref().map_or(("PENDING", ""), |q| (q.state.as_str(), q.reason.as_str()));
            let what = if state == "RUNNING" { format!("running on node {reason}, and Julia is starting there") } else { format!("waiting in the queue: {}", queue_reason_text(reason)) };
            format!("The Slurm job{job} on {name} is {what}. Tell the user, wait, and call `pluto_session_status` to follow it.")
        }
        State::Failed => {
            let error = status.error.as_deref().unwrap_or("it didn't say why");
            format!("Julia on {name} isn't available: {error}\nCall `use_machine` with machine \"{name}\" to try again, or tell the user.")
        }
        State::NeedsInstall => format!("{} Nothing was installed.", install_text(name, status, "use_machine")),
        State::Unknown => format!("The link to {name} is in a state this version of Endeavor doesn't know (it is from a newer build). Call `use_machine` with machine \"{name}\" to try again; if it stays like this, tell the user."),
        State::Ready => format!("Julia on {name} is ready."),
    }
}

/// What installing would do on the machine and what to ask the user, for the agent to relay.
/// `tool` is the machine tool to call again, with `install: true`, once the user has agreed.
/// Every item is named with its size and place; a kind this build knows adds a note.
fn install_text(name: &str, status: &link::Status, tool: &str) -> String {
    let Some(info) = &status.needs_install else { return format!("Endeavor needs to install something on {name}.") };
    let again = match tool {
        "add_machine" => "`add_machine` again with the same arguments".to_owned(),
        "stop_machine" => format!("`stop_machine` again with machine \"{name}\" (and the same other arguments)"),
        _ => format!("`use_machine` again with machine \"{name}\" (and the same other arguments)"),
    };
    let mut text = wire::needs_text(&info.items, name);
    if let Some(helper) = &info.helper {
        let update = if helper.update { " A helper of an older version is installed there already (this is an update); it stays beside the new one." } else { "" };
        let attach = "installing the helper doesn't touch it, and the helper is what lets Endeavor attach to it.";
        let running = match &helper.running {
            Some(Running::Process { pid, checked: true }) => format!(" Julia is already running there (process {pid}); {attach}"),
            Some(Running::Process { pid, checked: false }) => format!(" A process ({pid}) that was recorded as Julia's is alive there, but Endeavor couldn't check what it is; {attach}"),
            Some(Running::Job { id, listed: true }) => format!(" A Slurm job ({id}) for Julia is pending or running there; {attach}"),
            Some(Running::Job { id, listed: false }) => format!(" A Slurm job ({id}) is recorded there for Julia, but Endeavor couldn't ask Slurm whether it still exists; {attach}"),
            None => " No running Julia was found there.".to_owned(),
        };
        let needed = if tool == "stop_machine" { " Stopping the runtime there needs it." } else { "" };
        let later = match tool {
            "add_machine" => " That doesn't install what a start needs: if Julia isn't found on the machine, `use_machine` asks about that.",
            "stop_machine" => "",
            _ => " The same yes covers what this start needs after it, such as Julia if none is found there.",
        };
        text.push_str(&format!(" It is Endeavor's helper program and its runtime files ({} {}).{needed}{later}{update}{running}", helper.os, helper.arch));
    }
    if info.items.iter().any(|item| item.kind == wire::KIND_RUNTIME) {
        text.push_str(&format!(" Or, if Julia is on {name}, call `add_machine` with its host and `julia` set to the path of the julia program, or to a shell line such as `module load julia`, and it is used instead. The download is part of `install: true` for this call only."));
    }
    format!("{text} Ask the user whether Endeavor may do that. Only if they agree, call {again} and `install: true`.")
}

fn install_json(status: &link::Status) -> Option<Value> {
    let info = status.needs_install.as_ref()?;
    let mut out = json!({ "items": info.items });
    if let Some(helper) = &info.helper {
        out["os"] = helper.os.clone().into();
        out["arch"] = helper.arch.clone().into();
        out["update"] = helper.update.into();
        out["running"] = json!(helper.running.as_ref().map(|r| match r {
            Running::Process { pid, checked: true } => json!({ "process": pid }),
            Running::Process { pid, checked: false } => json!({ "process_recorded": pid }),
            Running::Job { id, listed: true } => json!({ "slurm_job": id }),
            Running::Job { id, listed: false } => json!({ "slurm_job_recorded": id }),
        }));
    }
    Some(out)
}

/// What a tool says when the machine needs something installed that the user hasn't agreed to.
fn needs_install_result(name: &str, status: &link::Status, tool: &str) -> Value {
    let mut result = json!({
        "machine": name,
        "state": "needs_install",
        "ready": false,
        "needs_install": true,
        "install": install_json(status),
        "message": format!("{} Nothing was installed on {name}.", install_text(name, status, tool)),
    });
    if tool == "stop_machine" {
        result["stopped"] = false.into();
    }
    result
}

/// End the link of `id` that `add_machine` started and could not use, unless a runtime is on it.
fn quit_unless_in_use(id: &str) {
    if let Ok(Some(link)) = link::find(id)
        && link.status(link::CALL_WAIT).is_ok_and(|status| replaceable(&status))
    {
        let _ = link.quit();
    }
}

/// A link whose protocol isn't this front's is replaced (quit, then started again) only when no
/// runtime hangs on it, since a new link has another port and the user's browser page would break.
fn replaceable(status: &link::Status) -> bool {
    status.runtime.is_none() && matches!(status.state, State::Connecting | State::Connected | State::Failed | State::NeedsInstall)
}

fn job_json(status: &link::Status) -> Option<Value> {
    let job = status.job.as_ref()?;
    let mut out = json!({ "id": job.id });
    if let Some(summary) = &job.summary {
        out["summary"] = summary.clone().into();
    }
    if let Some(node) = &job.node {
        out["node"] = node.clone().into();
    }
    if let Some(ends_at) = job.ends_at {
        out["ends_at"] = ends_at.into();
        out["ends_in_minutes"] = (ends_at.saturating_sub(unix_now()) / 60).into();
    }
    Some(out)
}

fn queue_json(status: &link::Status) -> Option<Value> {
    let queue = status.queue.as_ref()?;
    Some(json!({ "state": queue.state, "reason": queue.reason, "reason_text": queue_reason_text(&queue.reason) }))
}

/// What `pluto_session_status` says when the machine's runtime isn't up.
fn status_result(name: &str, status: &link::Status, message: &str) -> Value {
    let mut out = json!({ "machine": name, "state": state_word(status.state), "ready": false, "message": message });
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            out[key] = value;
        }
    };
    put("step", status.step.clone().map(Into::into));
    put("error", status.error.clone().map(Into::into));
    put("queue", queue_json(status));
    put("job", job_json(status));
    put("install", install_json(status));
    out
}

fn resources_json(resources: &Resources, account: Option<&str>) -> Value {
    json!({
        "partition": resources.partition,
        "cpus": resources.cpus,
        "memory_gb": resources.mem_gb,
        "hours": f64::from(resources.minutes) / 60.0,
        "gpus": resources.gres,
        "account": account,
        "extra_sbatch_flags": resources.extra,
        "summary": resources.summary(),
    })
}

fn partition_json(p: &Partition) -> Value {
    json!({ "name": p.name, "default": p.default, "max_hours": p.max_minutes.map(|m| f64::from(m) / 60.0), "cpus": p.cpus, "memory_gb": p.mem_gb() })
}

fn partition_text(p: &Partition) -> String {
    let limit = p.max_minutes.map_or("no time limit".to_owned(), |m| format!("up to {}", wire::slurm::duration_text(m)));
    format!("{}{} ({limit}, {} CPUs and {} GB a node)", p.name, if p.default { " (default)" } else { "" }, p.cpus, p.mem_gb())
}

/// A machine's name as the tools take it: letters, digits, `-`, `_` and `.`, starting and ending with a letter or digit.
fn valid_name(name: &str) -> Result<(), String> {
    let plain = !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && name.starts_with(|c: char| c.is_ascii_alphanumeric()) && name.ends_with(|c: char| c.is_ascii_alphanumeric());
    if !plain {
        return Err(invalid(format!("\"{name}\" isn't a machine name: use up to 64 letters, digits, - _ and ., starting and ending with a letter or digit.")));
    }
    if name.eq_ignore_ascii_case(LOCAL) {
        return Err(invalid(format!("\"{LOCAL}\" is this computer, so no machine can have that name.")));
    }
    Ok(())
}

/// A name for a host given as `user@host`, where the user isn't part of it.
fn default_name(host: &str) -> String {
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let name: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    name.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_owned()
}

/// A new machine's id: its name in lower case, with a number after it if that id is taken.
fn new_id(name: &str, servers: &[Server]) -> Result<String, String> {
    let base = name.to_ascii_lowercase();
    let mut id = base.clone();
    for n in 2.. {
        if !servers.iter().any(|s| s.id == id) {
            break;
        }
        id = format!("{base}-{n}");
    }
    link::valid_id(&id).map_err(invalid)?;
    Ok(id)
}

fn text_arg(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.chars().any(char::is_control) => Err(invalid(format!("{key} can't hold control characters"))),
        Some(Value::String(text)) => Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty())),
        Some(_) => Err(invalid(format!("{key} must be a string"))),
    }
}

fn flag_arg(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(invalid(format!("{key} must be true or false"))),
    }
}

fn count_arg(args: &Value, key: &str, max: u32) -> Result<Option<u32>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| (1..=max).contains(n)) {
            Some(n) => Ok(Some(n)),
            None => Err(invalid(format!("{key} must be a whole number from 1 to {max}"))),
        },
    }
}

/// The resources and account a `use_machine` call gives.
#[derive(Default, Debug, PartialEq)]
struct Given {
    partition: Option<String>,
    cpus: Option<u32>,
    memory_gb: Option<u32>,
    minutes: Option<u32>,
    /// Not given, none (`gpus` 0, which clears the saved default), or a gres string.
    gres: Option<Option<String>>,
    account: Option<String>,
    extra: Option<Vec<String>>,
}

impl Given {
    fn parse(args: &Value) -> Result<Given, String> {
        let minutes = match args.get("hours") {
            None | Some(Value::Null) => None,
            Some(hours) => match hours.as_f64().filter(|h| h.is_finite() && *h > 0.0 && *h <= 24.0 * 365.0) {
                Some(hours) => Some((hours * 60.0).ceil() as u32),
                None => return Err(invalid("hours must be a number above 0, such as 8 or 0.5")),
            },
        };
        let gres = match args.get("gpus") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => match n.as_u64() {
                Some(0) => Some(None),
                Some(n) if n <= 64 => Some(Some(format!("gpu:{n}"))),
                _ => return Err(invalid("gpus must be a count from 0 to 64, or a Slurm gres string such as \"gpu:a100:2\"")),
            },
            Some(Value::String(text)) if !text.trim().is_empty() && !text.chars().any(|c| c.is_control() || c.is_whitespace()) => Some(Some(text.trim().to_owned())),
            Some(_) => return Err(invalid("gpus must be a count, or a Slurm gres string such as \"gpu:a100:2\" with no spaces")),
        };
        let extra = match args.get("extra_sbatch_flags") {
            None | Some(Value::Null) => None,
            Some(Value::Array(flags)) => {
                let flags: Option<Vec<String>> = flags.iter().map(|f| f.as_str().map(str::to_owned)).collect();
                let flags = flags.ok_or_else(|| invalid("extra_sbatch_flags must be a list of strings, one flag each"))?;
                for flag in &flags {
                    check_extra_flag(flag).map_err(|why| invalid(format!("extra_sbatch_flags: {why}")))?;
                    if flag.chars().any(char::is_control) {
                        return Err(invalid("extra_sbatch_flags can't hold control characters"));
                    }
                }
                Some(flags)
            }
            Some(_) => return Err(invalid("extra_sbatch_flags must be a list of strings, one flag each")),
        };
        Ok(Given { partition: text_arg(args, "partition")?, cpus: count_arg(args, "cpus", 4096)?, memory_gb: count_arg(args, "memory_gb", 1_000_000)?, minutes, gres, account: text_arg(args, "account")?, extra })
    }

    fn any(&self) -> bool {
        *self != Given::default()
    }

    /// The machine's saved defaults with what was given put over them, kept within the partition's limits.
    fn over(&self, cluster: &Cluster) -> Result<(Resources, Option<String>), String> {
        let mut resources = cluster.resources.clone();
        if let Some(name) = &self.partition {
            if !cluster.partitions.is_empty() && cluster.partition(Some(name)).is_none() {
                let known: Vec<&str> = cluster.partitions.iter().map(|p| p.name.as_str()).collect();
                return Err(invalid(format!("There is no partition \"{name}\" on this cluster. Its partitions: {}.", known.join(", "))));
            }
            resources.partition = Some(name.clone());
        }
        resources.cpus = self.cpus.unwrap_or(resources.cpus);
        resources.mem_gb = self.memory_gb.unwrap_or(resources.mem_gb);
        resources.minutes = self.minutes.unwrap_or(resources.minutes);
        if let Some(gres) = &self.gres {
            resources.gres = gres.clone();
        }
        if let Some(extra) = &self.extra {
            resources.extra = extra.clone();
        }
        resources.clip(cluster.partition(resources.partition.as_deref()));
        Ok((resources, self.account.clone().or_else(|| cluster.account.clone())))
    }
}

impl Relay {
    pub(super) fn machine(&self) -> Option<Machine> {
        self.machine_placed().map(|(machine, _)| machine)
    }

    /// The machine the session is on and the session's key, taken together.
    fn machine_placed(&self) -> Option<(Machine, String)> {
        match self.placed() {
            (Target::Machine(machine), session) => Some((machine, session)),
            (Target::Local { .. }, _) => None,
        }
    }

    fn update_machine(&self, id: &str, change: impl FnOnce(&mut Machine)) {
        if let Target::Machine(machine) = &mut *self.target.lock().unwrap()
            && machine.id == id
        {
            change(machine);
        }
    }

    /// The runtime's port and token on a machine, if the link says it is up. Starts nothing.
    pub(super) fn machine_runtime(&self, machine: &Machine) -> Option<(u16, String)> {
        let status = machine.link.as_ref()?.status(link::CALL_WAIT).ok()?;
        status.runtime.filter(|_| status.state == State::Ready).map(|r| (r.port, r.token))
    }

    /// The session moves to `next`. The target and the session's key are replaced together, by a
    /// key the runtime hasn't ended unless the session stays on the runtime it is on. What it left:
    /// the target, the key it had there, and whether the key is to be ended there (`leave`).
    pub(super) fn switch(&self, next: Target) -> (Target, String, bool) {
        let mut target = self.target.lock().unwrap();
        let stays = match (&*target, &next) {
            (Target::Machine(from), Target::Machine(to)) => from.id == to.id,
            (Target::Local { stopped: false }, Target::Local { .. }) => true,
            _ => false,
        };
        let previous = std::mem::replace(&mut *target, next);
        let mut session = self.session.lock().unwrap();
        let before = std::mem::replace(&mut *session, String::new());
        *session = if stays {
            before.clone()
        } else {
            let n = self.sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            format!("{}-{n}", self.session_base)
        };
        (previous, before, !stays)
    }

    /// The project's remembered machine becomes the target, without starting anything.
    pub(super) fn target_from_project(&self) {
        let remembered = match self.projects.get(&self.options.folder) {
            Ok(remembered) => remembered,
            Err(e) => {
                eprintln!("endeavor: {e}");
                None
            }
        };
        let Some(remembered) = remembered else { return };
        match self.machines.find_by_id(&remembered.machine) {
            Ok(Some(server)) => {
                eprintln!("endeavor: this project uses the machine {}", display_name(&server));
                *self.target.lock().unwrap() = Target::Machine(Machine::new(&server, remembered.folder));
            }
            Ok(None) => {
                *self.notice.lock().unwrap() = Some(format!(
                    "This project last used the machine \"{}\", which isn't in the list of machines any more, so this session uses this computer. `list_machines` shows the machines there are.",
                    remembered.machine
                ));
            }
            Err(e) => eprintln!("endeavor: {e}"),
        }
    }

    /// While the target is a machine, ask its link how it is every few minutes: the link ends 8 hours after its last request.
    pub(super) fn keep_link_alive(self: &Arc<Self>) {
        let every = std::env::var("ENDEAVOR_FRONT_PING_SECS").ok().and_then(|s| s.parse::<f64>().ok()).filter(|s| *s > 0.0).and_then(|s| Duration::try_from_secs_f64(s).ok()).unwrap_or(PING_EVERY);
        let relay = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                if let Some(link) = relay.machine().and_then(|m| m.link) {
                    let _ = link.status(link::CALL_WAIT);
                }
            }
        });
    }

    /// Where a call goes, waiting up to `start_wait()` for a runtime that is on its way. `wait` false
    /// (for the status tool, which says how far it is) doesn't wait, and a job in the queue isn't waited for either.
    pub(super) fn route(self: &Arc<Self>, wait: bool) -> Result<Route, NotReady> {
        let (target, session) = self.placed();
        match target {
            Target::Local { stopped: true } => Err(NotReady::plain("Julia on this computer was stopped from this session with stop_machine. Call `use_machine` with machine \"local\" to start it again.")),
            Target::Local { stopped: false } => self.runtime().map(|(port, token)| Route { port, token, session, host: None }).map_err(NotReady::plain),
            Target::Machine(_) => self.machine_route(wait),
        }
    }

    fn machine_route(&self, wait: bool) -> Result<Route, NotReady> {
        let deadline = Deadline::after(start_wait());
        let mut connected_since: Option<Instant> = None;
        loop {
            let Some((machine, session)) = self.machine_placed() else { return Err(NotReady::plain("This session moved to another machine while the call waited. Try the call again.")) };
            let name = machine.name.clone();
            let (link, status, _) = self.machine_link(&machine, deadline).map_err(NotReady::plain)?;
            if !machine.active {
                return Err(NotReady::of(&name, &status, format!("Julia on {name} was stopped from this session with stop_machine. Call `use_machine` with machine \"{name}\" to start it again.")));
            }
            if status.state == State::Ready {
                return self.machine_ready(&machine, &status, &session).ok_or_else(|| NotReady::of(&name, &status, "The link to the machine says it is ready but gave no runtime. Try again."));
            }
            if machine.asked != Some(link.pid) {
                self.ask(&link, Ask::Attach, false, deadline).map_err(NotReady::plain)?;
                self.update_machine(&machine.id, |m| m.asked = Some(link.pid));
                continue;
            }
            match status.state {
                State::Connected if status.nothing_running && machine.cluster => return Err(NotReady::of(&name, &status, self.needs_job_message(&machine))),
                State::Connected if status.nothing_running => {
                    if self.ask(&link, Ask::Start(None), false, deadline).map_err(NotReady::plain)? {
                        continue;
                    }
                    return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
                }
                State::Connected => {
                    if !wait || connected_since.get_or_insert_with(Instant::now).elapsed() > GRACE {
                        return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
                    }
                }
                State::Failed | State::Queued | State::NeedsInstall | State::Unknown => return Err(NotReady::of(&name, &status, not_ready_message(&name, &status))),
                _ => connected_since = None,
            }
            if !wait || deadline.spent() {
                return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
            }
            std::thread::sleep(POLL);
            if deadline.spent() {
                return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
            }
        }
    }

    /// The machine's link and what it says, started if there is none or it has gone, after `link_rule`.
    fn machine_link(&self, machine: &Machine, deadline: Deadline) -> Result<(Link, link::Status, Option<String>), String> {
        let server = self.machines.find_by_id(&machine.id)?.ok_or_else(|| format!("{} isn't in the list of machines ({}) any more.", machine.name, self.machines.path().display()))?;
        let (link, status) = match machine.link.clone().and_then(|link| deadline.status(&link).ok().map(|status| (link, status))) {
            Some(found) => found,
            None => {
                let link = deadline.ensure(&server)?;
                let status = deadline.status(&link)?;
                (link, status)
            }
        };
        let (link, status, note) = self.link_rule(&server, link, status, deadline)?;
        if machine.link.as_ref() != Some(&link) {
            self.update_machine(&machine.id, |m| m.link = Some(link.clone()));
        }
        Ok((link, status, note))
    }

    /// The rule for a link, which every path to a link goes through before it asks for a runtime.
    /// A link of the same control protocol is used fully, whatever its build. One of another
    /// protocol is replaced (quit, then started again) only when no runtime hangs on it
    /// (`replaceable`), since a new link has another port and the user's browser page would
    /// break. Else it is used as it is, and the third is why; `ask` sends it no start, since it
    /// may not know `only_running`. `status` is the link's.
    fn link_rule(&self, server: &Server, link: Link, status: link::Status, deadline: Deadline) -> Result<(Link, link::Status, Option<String>), String> {
        if link.protocol == link::PROTOCOL {
            return Ok((link, status, None));
        }
        let (link, status) = if replaceable(&status) {
            let _ = link.quit();
            let fresh = deadline.ensure(server)?;
            let status = deadline.status(&fresh)?;
            (fresh, status)
        } else {
            (link, status)
        };
        if link.protocol == link::PROTOCOL {
            return Ok((link, status, None));
        }
        let name = display_name(server);
        let note = format!(
            "The link to {name} was started by another build of endeavor ({}) that works differently from this one, and goes on, because a runtime is in use through it and a new link would change the address of the user's browser page. It is replaced when it ends.",
            link.build
        );
        Ok((link, status, Some(note)))
    }

    /// Ask `link` to start the runtime or attach to it. A link of another protocol gets nothing: it
    /// may not know `only_running`, and would then start what was only to be attached to (on a
    /// cluster, a job nobody agreed to). False when nothing was sent. Every start goes through here.
    pub(super) fn ask(&self, link: &Link, ask: Ask, install: bool, deadline: Deadline) -> Result<bool, String> {
        if link.protocol != link::PROTOCOL {
            return Ok(false);
        }
        deadline.send(link, ask, install)?;
        Ok(true)
    }

    /// Where calls to the machine's runtime go, once it is ready; the session's folder is told to it once.
    fn machine_ready(&self, machine: &Machine, status: &link::Status, session: &str) -> Option<Route> {
        let runtime = status.runtime.as_ref()?;
        if machine.told != Some(runtime.pid) {
            let folder = machine.folder.clone().or_else(|| status.hello.as_ref().map(|h| h.home.clone()).filter(|h| !h.is_empty()));
            if let Some(folder) = folder {
                self.tell_session_folder(runtime.port, &runtime.token, session, &folder);
            }
            self.update_machine(&machine.id, |m| m.told = Some(runtime.pid));
        }
        let host = crate::mcp::clean_label(&machine.name).unwrap_or_else(|| machine.id.clone());
        Some(Route { port: runtime.port, token: runtime.token.clone(), session: session.to_owned(), host: Some(host) })
    }

    /// For a project's remembered cluster with no job: what to ask the user before submitting one.
    fn needs_job_message(&self, machine: &Machine) -> String {
        let defaults = self.machines.find_by_id(&machine.id).ok().flatten().and_then(|s| s.cluster).map(|c| {
            let partition = c.resources.partition.as_deref().map_or("the cluster's default partition".to_owned(), |p| format!("partition {p}"));
            format!("{} on {partition}", c.resources.summary())
        });
        let name = &machine.name;
        let defaults = defaults.map(|d| format!(" The defaults would be {d}.")).unwrap_or_default();
        format!(
            "This project uses {name}, a Slurm cluster, and no job is running there. Starting Julia means submitting a job that waits in the queue and uses the user's allocation, so nothing was submitted.{defaults} Ask the user to confirm those resources or choose others, then call `use_machine` with machine \"{name}\" and the resources to submit it."
        )
    }

    /// The reply to a call that couldn't go to a runtime.
    pub(super) fn unready(&self, message: &Value, tool: Option<&str>, unready: NotReady) {
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else { return };
        let reply = match (&unready.status, tool) {
            (Some(status), Some("pluto_session_status")) => {
                let result = status_result(&unready.name, status, &unready.message);
                to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": to_json(&result) }], "isError": false } }))
            }
            _ => tool_failure(id, message, &unready.message),
        };
        self.write(&self.decorate(message, tool, reply));
    }

    /// A runtime's reply to the agent's call, with what only the front knows: that the session is on a
    /// machine (and its job) in `pluto_session_status`, and a notice that is owed once.
    pub(super) fn decorate(&self, message: &Value, tool: Option<&str>, reply: String) -> String {
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        if parsed["id"] != message["id"] || (parsed.get("result").is_none() && parsed.get("error").is_none()) {
            return reply;
        }
        let mut changed = false;
        if tool == Some("pluto_session_status") && parsed["result"]["isError"] == false {
            changed |= self.add_machine_fields(&mut parsed);
        }
        if let Some(notice) = self.notice.lock().unwrap().take() {
            match parsed["result"]["content"].as_array_mut() {
                Some(content) => content.push(json!({ "type": "text", "text": notice })),
                None => parsed["error"]["message"] = format!("{} {notice}", parsed["error"]["message"].as_str().unwrap_or_default()).into(),
            }
            changed = true;
        }
        if changed { to_json(&parsed) } else { reply }
    }

    fn add_machine_fields(&self, reply: &mut Value) -> bool {
        let Some(machine) = self.machine() else { return false };
        let Some(Value::Object(mut fields)) = reply["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()) else { return false };
        fields.insert("machine".into(), machine.name.clone().into());
        if machine.cluster
            && let Some(job) = machine.link.as_ref().and_then(|l| l.status(link::CALL_WAIT).ok()).and_then(|s| job_json(&s))
        {
            fields.insert("job".into(), job);
        }
        reply["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        true
    }

    /// Answer a call to one of the machine tools.
    pub(super) fn machine_tool(self: &Arc<Self>, message: &Value, tool: &str) {
        let deadline = Deadline::after(start_wait());
        if message.get("id").is_none_or(Value::is_null) {
            return;
        }
        let arguments = message["params"].get("arguments").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        let result = match tool {
            "list_machines" => self.list_machines(),
            "add_machine" => self.add_machine(&arguments, deadline),
            "use_machine" => self.use_machine(&arguments, deadline),
            _ => self.stop_machine(&arguments, deadline),
        };
        let result = match result {
            Ok(result) => text_result(&to_json(&result)),
            Err(e) => tool_error(&e, false),
        };
        self.answer_call(message, Some(tool), result);
    }

    /// Answer request `message` with `result`, a tool call's result.
    pub(super) fn answer_call(&self, message: &Value, tool: Option<&str>, result: Value) {
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else { return };
        self.write(&self.decorate(message, tool, to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))));
    }

    /// What `tool` (`pluto_session_status` or `list_notebooks`) answers when the session is on this
    /// computer and its runtime isn't up yet: it uses one that is running, as a start would, and
    /// starts none. None when the call is to go to the runtime (or the machine) as usual; else
    /// the answer, or why there is none.
    pub(super) fn without_start(self: &Arc<Self>, tool: &str) -> Option<Result<String, String>> {
        let idle = || matches!(*self.target.lock().unwrap(), Target::Local { stopped: false }) && matches!(*self.status.lock().unwrap(), Local::Idle);
        if !idle() {
            return None;
        }
        // The same lock as `use_machine` and `stop_machine`: the session doesn't move, and the
        // runtime isn't stopped, between the check and the answer.
        let _one = match self.lock_ops(Deadline::after(start_wait())) {
            Ok(one) => one,
            Err(why) => return Some(Err(why)),
        };
        if !idle() {
            return None;
        }
        match super::attach(&self.options) {
            Ok(Some(up)) => {
                self.told(&up);
                let mut status = self.status.lock().unwrap();
                if matches!(*status, Local::Idle) {
                    *status = Local::Ready { port: up.port, token: up.state.token };
                }
                drop(status);
                self.changed.notify_all();
                None
            }
            Ok(None) if tool == "list_notebooks" => Some(Ok("[]".to_owned())),
            Ok(None) => Some(Ok(to_json(&json!({
                "pluto": "not running",
                "notebooks": [],
                "message": "Julia on this computer isn't running. It starts at the first notebook tool call, which then takes a few minutes the first time.",
            })))),
            Err(e) => Some(Err(format!("Endeavor's Julia couldn't start: {e}"))),
        }
    }

    /// The one machine tool call that runs at a time (they switch, start and stop things), waited
    /// for until `deadline`. A call that doesn't get it has changed nothing.
    fn lock_ops(&self, deadline: Deadline) -> Result<MutexGuard<'_, ()>, String> {
        loop {
            match self.ops.try_lock() {
                Ok(held) => return Ok(held),
                Err(TryLockError::Poisoned(held)) => return Ok(held.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if deadline.spent() {
                return Err("Another machine tool call is still running, so this one didn't start and changed nothing. Try again in a moment.".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn list_machines(&self) -> Result<Value, String> {
        let servers = self.machines.load()?;
        let mine = self.machine().map(|m| m.id);
        let machines: Vec<Value> = servers
            .iter()
            .map(|server| {
                let status = link::find(&server.id).ok().flatten().and_then(|l| l.status(link::CALL_WAIT).ok());
                let state = status.as_ref().map_or("no link running", |s| state_word(s.state));
                let mut entry = json!({ "name": display_name(server), "host": server.ssh_target(), "cluster": server.cluster.is_some(), "state": state, "this_session": mine.as_deref() == Some(server.id.as_str()) });
                if let Some(error) = status.and_then(|s| s.error) {
                    entry["error"] = error.into();
                }
                entry
            })
            .collect();
        let (stopped, on_local) = match &*self.target.lock().unwrap() {
            Target::Local { stopped } => (*stopped, true),
            Target::Machine(_) => (false, false),
        };
        let running = super::running_here(&self.options.state_dir).is_some();
        let local_state = match (stopped, running) {
            (true, _) => "stopped from this session",
            (false, true) => "running",
            (false, false) => "not running",
        };
        let ssh_hosts: Vec<String> = ssh_config_hosts().into_iter().filter(|h| !servers.iter().any(|s| s.ssh_host == *h || s.name.eq_ignore_ascii_case(h))).collect();
        let used = self.machine().map_or(LOCAL.to_owned(), |m| m.name);
        Ok(json!({
            "machines": machines,
            "local": { "name": LOCAL, "state": local_state, "this_session": on_local },
            "this_session": { "machine": used },
            "ssh_hosts_not_added": ssh_hosts,
            "message": format!("This session works on {used}. `add_machine` adds a server from the ssh hosts listed; `use_machine` moves the session to a machine, or back to \"{LOCAL}\"."),
        }))
    }

    fn add_machine(&self, args: &Value, deadline: Deadline) -> Result<Value, String> {
        let _one = self.lock_ops(deadline)?;
        let typed = text_arg(args, "host")?.ok_or_else(|| invalid("host is required: an ssh alias from ~/.ssh/config, or user@host"))?;
        let (host, port) = Server::parse_target(&typed).map_err(invalid)?;
        let given_name = text_arg(args, "name")?;
        let name = match &given_name {
            Some(name) => name.clone(),
            None => default_name(&host),
        };
        valid_name(&name)?;
        let julia = text_arg(args, "julia")?;
        let install = flag_arg(args, "install")?;
        let slurm = match args.get("slurm") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(slurm)) => Some(*slurm),
            Some(_) => return Err(invalid("slurm must be true (run Julia in Slurm jobs) or false (run it directly on the machine)")),
        };
        self.machines.check_writable()?;
        let servers = self.machines.load()?;
        let by_name = |n: &str| servers.iter().find(|s| s.name.eq_ignore_ascii_case(n)).cloned();
        let existing = match &given_name {
            Some(name) => by_name(name),
            None => by_name(&name).or_else(|| servers.iter().find(|s| s.ssh_host == host && s.port == port).cloned()),
        };
        let prior = existing.clone();
        let mut record = match &existing {
            Some(existing) => existing.clone(),
            None => Server { id: new_id(&name, &servers)?, name: name.clone(), ..Default::default() },
        };
        record.ssh_host = host;
        record.port = port;
        if julia.is_some() {
            record.julia = julia;
        }
        // The record is saved only once it has connected: until then the link holds it, and a machine that never connects leaves nothing behind.
        // A link that failed before, or that was started for other settings, connects afresh.
        if let Some(old) = deadline.find(&record.id)?
            && (link::handed(&record.id).is_some_and(|handed| !handed.same_connection(&record)) || old.status(link::CALL_WAIT).is_ok_and(|s| s.state == State::Failed))
        {
            let _ = old.quit();
        }
        let link = deadline.ensure_install(&record, install)?;
        let first = deadline.status(&link)?;
        let (link, _, kept) = self.link_rule(&record, link, first, deadline)?;
        let mut notes: Vec<String> = kept.into_iter().collect();
        if install && notes.is_empty() {
            deadline.install(&link)?;
        } else if install {
            notes.push("`install: true` wasn't passed on to that link, which has a runtime in use.".into());
        }
        let mut status = deadline.status(&link)?;
        let status = loop {
            match status.state {
                State::NeedsInstall if status.needs_install.as_ref().is_some_and(link::InstallInfo::needs_helper) => {
                    let mut result = needs_install_result(&record.name, &status, "add_machine");
                    result["host"] = record.ssh_target().into();
                    result["saved"] = false.into();
                    result["message"] = format!("{} The machine is saved when it has connected, and not before.", result["message"].as_str().unwrap_or_default()).into();
                    return Ok(result);
                }
                State::Failed => {
                    quit_unless_in_use(&record.id);
                    return Err(format!(
                        "Couldn't connect to {}: {}\nNothing was saved. This is for the user to fix in a terminal (never ask them for a password or passphrase here, and don't run ssh yourself), then call `add_machine` again.",
                        record.ssh_target(),
                        status.error.as_deref().unwrap_or("no reason was given")
                    ));
                }
                State::Connecting if deadline.spent() => {
                    return Ok(json!({
                        "machine": record.name, "host": record.ssh_target(), "state": "connecting", "saved": false,
                        "step": status.step,
                        "message": format!("Still connecting to {}. Call `add_machine` again with the same host to continue. The machine is saved when it has connected, and not before.", record.ssh_target()),
                    }));
                }
                State::Connecting => {
                    std::thread::sleep(POLL);
                    if !deadline.spent() {
                        status = deadline.status(&link)?;
                    }
                }
                _ => break status,
            }
        };
        let mut status = status;
        let connected_as_cluster = record.cluster.is_some();
        let found = status.hello.as_ref().is_some_and(|h| h.slurm);
        let cluster = match choose_mode(slurm, prior.as_ref(), found) {
            Ok(cluster) => cluster,
            Err(why) => {
                quit_unless_in_use(&record.id);
                return Err(format!("{why}\nNothing was saved."));
            }
        };
        if prior.as_ref().is_some_and(|p| p.cluster.is_some() != cluster) && (status.runtime.is_some() || status.job.is_some() || matches!(status.state, State::Starting | State::Queued | State::Ready)) {
            return Err(format!(
                "Julia is running, or starting, on {} through the way it is saved now. Changing between running Julia in Slurm jobs and running it directly would leave that one where `stop_machine` can't reach it. Nothing was changed. Call `stop_machine` first (with the user's agreement), then call `add_machine` again.",
                display_name(&record)
            ));
        }
        // The link asks Slurm for the partitions after it connects, so they may come a moment later. They are reported, so they are waited for, except when the user said it is no cluster.
        let listed = cluster || slurm != Some(false);
        while listed && status.hello.as_ref().is_some_and(|h| h.slurm && h.partitions.is_none()) && !deadline.spent() {
            std::thread::sleep(POLL);
            if !deadline.spent() {
                status = deadline.status(&link)?;
            }
        }
        let hello = status.hello.clone().unwrap_or_default();
        if listed && hello.slurm && hello.partitions.is_none() && record.cluster.as_ref().is_none_or(|c| c.partitions.is_empty()) {
            return Ok(json!({
                "machine": record.name, "host": record.ssh_target(), "state": "connecting", "saved": false,
                "step": status.step,
                "message": format!("Connected to {}, but Slurm hasn't listed its partitions yet. Call `add_machine` again with the same host to continue. The machine is saved when it has connected, and not before.", record.ssh_target()),
            }));
        }
        if cluster {
            let before = record.cluster.take();
            let mut cluster = before.clone().unwrap_or_default();
            if hello.slurm {
                cluster.partitions = hello.partitions.clone().unwrap_or(cluster.partitions);
                cluster.scratch = hello.scratch.clone().or(cluster.scratch);
            }
            if before.is_none() {
                let mut resources = Resources::default();
                resources.clip(cluster.partition(None));
                cluster.resources = resources;
            }
            record.cluster = Some(cluster);
        } else {
            record.cluster = None;
        }
        self.machines.save(record.clone())?;
        if record.cluster.is_some() != connected_as_cluster {
            // It connected the other way: the next connection starts the helper for how it is saved now.
            let _ = link.quit();
        }
        let partitions: Vec<Partition> = record.cluster.as_ref().map(|c| c.partitions.clone()).or_else(|| hello.partitions.clone()).unwrap_or_default();
        let mut message = format!("Connected to {} (node {}, home folder {}). ", record.ssh_target(), hello.node, hello.home);
        if let Some(saved) = &record.cluster {
            message.push_str("It has Slurm, and Julia runs in Slurm jobs there");
            if partitions.is_empty() {
                message.push_str(if hello.partitions_failed { ", but its partitions couldn't be read. " } else { ", but it didn't list its partitions. " });
            } else {
                message.push_str(&format!(". Partitions: {}. ", partitions.iter().map(partition_text).collect::<Vec<_>>().join("; ")));
            }
            message.push_str(&format!("Default job: {}. ", saved.resources.summary()));
            message.push_str("To run Julia directly on the machine instead, call `add_machine` again with slurm false (the saved job defaults are then dropped). ");
        } else if hello.slurm {
            let why = if slurm == Some(false) { "as asked" } else { "since it was saved before as a plain server" };
            message.push_str(&format!("It has Slurm, but Julia runs on it directly and not in a job, {why}. To run Julia in Slurm jobs instead, call `add_machine` again with slurm true. "));
        } else {
            message.push_str("It has no Slurm, so Julia runs there directly. ");
        }
        if hello.found.is_empty() {
            message.push_str("Endeavor looks for Julia when it first starts a runtime there. ");
        }
        for found in &hello.found {
            message.push_str(&format!("{} {} is at {}. ", found.name, found.version, found.path));
        }
        message.push_str(&format!("The machine is saved as \"{}\". Call `use_machine` to work on it.", record.name));
        for note in &notes {
            message.push_str(&format!(" {note}"));
        }
        Ok(json!({
            "machine": record.name,
            "host": record.ssh_target(),
            "state": "connected",
            "saved": true,
            "updated": prior.is_some(),
            "node": hello.node,
            "home": hello.home,
            "os": hello.os,
            "arch": hello.arch,
            "slurm": hello.slurm,
            "cluster": record.cluster.is_some(),
            "runs_in": if record.cluster.is_some() { "slurm_jobs" } else { "directly" },
            "partitions": partitions.iter().map(partition_json).collect::<Vec<_>>(),
            "scratch": record.cluster.as_ref().and_then(|c| c.scratch.clone()),
            "found": hello.found,
            "message": message,
        }))
    }

    /// A machine named in a call: a saved name or id.
    fn find_machine(&self, key: &str) -> Result<Server, String> {
        match self.machines.find(key)? {
            Some(server) => Ok(server),
            None => {
                let names: Vec<String> = self.machines.load()?.iter().map(display_name).collect();
                let known = if names.is_empty() { "No machine is added yet; `add_machine` adds one.".to_owned() } else { format!("Machines: {}.", names.join(", ")) };
                Err(format!("ArgumentError: machine_not_found::There is no machine \"{key}\". {known} \"{LOCAL}\" is this computer."))
            }
        }
    }

    /// The machine's link and its status, started if there is none, after `link_rule`; the third is a note for the result.
    fn link_for_use(&self, server: &Server, deadline: Deadline) -> Result<(Link, link::Status, Option<String>), String> {
        let link = deadline.ensure(server)?;
        let status = deadline.status(&link)?;
        self.link_rule(server, link, status, deadline)
    }

    /// Wait up to `deadline` for the link to settle: ready, queued, failed, found nothing running, or
    /// connected with nothing under way.
    fn settle(&self, link: &Link, deadline: Deadline) -> Result<link::Status, String> {
        let mut connected_since: Option<Instant> = None;
        loop {
            let status = deadline.last_status(link)?;
            match status.state {
                State::Ready | State::Queued | State::Failed | State::NeedsInstall | State::Unknown => return Ok(status),
                State::Connected if status.nothing_running => return Ok(status),
                State::Connected if connected_since.get_or_insert_with(Instant::now).elapsed() > GRACE => return Ok(status),
                State::Connected => {}
                _ => connected_since = None,
            }
            if deadline.spent() {
                return Ok(status);
            }
            std::thread::sleep(POLL);
        }
    }

    /// Put the session on a machine. Everything that can fail, and the request to start or attach, comes
    /// first; only when the request was taken does the session move, its key on the old runtime end,
    /// and the project remember the machine. Whatever fails before that leaves all of it as it was.
    fn use_machine(self: &Arc<Self>, args: &Value, deadline: Deadline) -> Result<Value, String> {
        let _one = self.lock_ops(deadline)?;
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        if key.eq_ignore_ascii_case(LOCAL) {
            return self.use_local();
        }
        let server = self.find_machine(&key)?;
        let install = flag_arg(args, "install")?;
        let given = Given::parse(args)?;
        let folder = match text_arg(args, "folder")? {
            Some(folder) => Some(folder),
            None => self.projects.get(&self.options.folder).ok().flatten().filter(|remembered| remembered.machine == server.id).and_then(|remembered| remembered.folder),
        };
        if given.any() && server.cluster.is_none() {
            return Err(invalid(format!("{} isn't a Slurm cluster, so partition, cpus, memory_gb, hours, gpus, account and extra_sbatch_flags don't apply to it. Leave them out.", display_name(&server))));
        }
        let planned = match &server.cluster {
            Some(cluster) if given.any() => Some(given.over(cluster)?),
            _ => None,
        };
        let name = display_name(&server);
        let (link, status, note) = self.link_for_use(&server, deadline)?;
        let was_ready = status.state == State::Ready;
        let mut notes: Vec<String> = note.into_iter().collect();
        let mut saved_resources = None;
        let status = match &server.cluster {
            None => {
                self.ask(&link, Ask::Start(None), install, deadline)?;
                self.settle(&link, deadline)?
            }
            Some(cluster) => {
                self.ask(&link, Ask::Attach, install, deadline)?;
                let mut status = self.settle(&link, deadline)?;
                if status.nothing_running {
                    let Some((resources, account)) = planned else { return Ok(self.needs_job(&name, cluster)) };
                    let mut job = cluster.job(&resources);
                    job.account = account.clone();
                    if self.ask(&link, Ask::Start(Some(job)), install, deadline)? {
                        saved_resources = Some((resources, account));
                    } else {
                        notes.push("No job was submitted: this link is from another build.".into());
                    }
                    status = self.settle(&link, deadline)?;
                } else if given.any() && status.state != State::NeedsInstall {
                    notes.push("A job is already queued or running there, so the resources you gave were not used.".into());
                }
                status
            }
        };
        if status.state == State::NeedsInstall {
            return Ok(needs_install_result(&name, &status, "use_machine"));
        }
        if status.state == State::Failed {
            return Err(format!("{}{}", not_ready_message(&name, &status), notes.iter().map(|n| format!(" {n}")).collect::<String>()));
        }
        if status.state == State::Ready && status.runtime.is_none() {
            return Err(format!("The link to {name} says it is ready but gave no runtime. Call `use_machine` again."));
        }
        let mut machine = Machine::new(&server, folder.clone());
        machine.link = Some(link.clone());
        machine.asked = Some(link.pid);
        let (previous, before, ended) = self.switch(Target::Machine(machine));
        if ended {
            self.leave(&previous, &before);
        }
        if let Some((resources, account)) = saved_resources {
            let mut saved = server.clone();
            if let Some(c) = saved.cluster.as_mut() {
                c.resources = resources;
                c.account = account;
            }
            if let Err(e) = self.save_defaults(&saved) {
                notes.push(format!("These resources weren't saved as the machine's defaults: {e}"));
            }
        }
        if let Err(e) = self.projects.set(&self.options.folder, Some(Remembered { machine: server.id.clone(), folder: folder.clone() })) {
            notes.push(format!("The project won't remember this machine: {e}"));
        }
        self.use_result(&server, &status, was_ready, notes)
    }

    /// Put the saved defaults back as they are now in the file, with the cluster's resources and account from `saved`.
    fn save_defaults(&self, saved: &Server) -> Result<(), String> {
        let Some(mut current) = self.machines.find_by_id(&saved.id)? else { return Ok(()) };
        if let (Some(now), Some(new)) = (current.cluster.as_mut(), &saved.cluster) {
            now.resources = new.resources.clone();
            now.account = new.account.clone();
        }
        self.machines.save(current)
    }

    /// End key `session` on the runtime `target` has, if it is up. Best effort.
    fn leave(&self, target: &Target, session: &str) {
        let runtime = match target {
            Target::Local { .. } => match &*self.status.lock().unwrap() {
                Local::Ready { port, token } => Some((*port, token.clone())),
                _ => None,
            },
            Target::Machine(machine) => self.machine_runtime(machine),
        };
        if let Some((port, token)) = runtime {
            self.end_session(port, &token, session);
        }
    }

    fn use_result(&self, server: &Server, status: &link::Status, was_ready: bool, notes: Vec<String>) -> Result<Value, String> {
        let name = display_name(server);
        let notes = if notes.is_empty() { String::new() } else { format!(" {}", notes.join(" ")) };
        match status.state {
            State::Ready => {
                let (machine, session) = self.machine_placed().ok_or("The session moved to another machine.")?;
                let Some(route) = self.machine_ready(&machine, status, &session) else { return Err(format!("The link to {name} says it is ready but gave no runtime. Call `use_machine` again.")) };
                let runtime = status.runtime.as_ref().ok_or("The link gave no runtime.")?;
                let folder = machine.folder.clone().or_else(|| status.hello.as_ref().map(|h| h.home.clone()));
                let mut result = json!({
                    "machine": name,
                    "state": "ready",
                    "ready": true,
                    "browser_url": browser_link(route.port, &route.token, "/"),
                    "node": runtime.node,
                    "folder": folder,
                    "already_running": runtime.reattached || was_ready,
                });
                if let Some(job) = job_json(status) {
                    result["job"] = job;
                }
                let on = if runtime.reattached || was_ready { "A runtime was already running there, and this session uses it" } else { "Julia started there" };
                let ends = job_json(status).and_then(|j| j["ends_in_minutes"].as_u64()).map(|m| format!(" The job ends in {}.", wire::slurm::duration_text(m as u32))).unwrap_or_default();
                result["message"] = format!(
                    "{on} (node {}). Give the user this link to watch the notebooks: {}.{ends} This session has no notebook on {name} yet: create one with `new_notebook` or open one with `open_notebook`; paths and files are {name}'s.{notes}",
                    runtime.node, result["browser_url"].as_str().unwrap_or_default()
                )
                .into();
                Ok(result)
            }
            _ => {
                let mut result = status_result(&name, status, &format!("{}{notes}", not_ready_message(&name, status)));
                result["this_session"] = true.into();
                Ok(result)
            }
        }
    }

    /// `use_machine` with this computer.
    fn use_local(self: &Arc<Self>) -> Result<Value, String> {
        let (previous, before, ended) = self.switch(Target::Local { stopped: false });
        if ended {
            self.leave(&previous, &before);
        }
        let mut notes = String::new();
        if let Err(e) = self.projects.set(&self.options.folder, None) {
            notes = format!(" The project's remembered machine wasn't cleared: {e}");
        }
        let folder = self.options.folder.display().to_string();
        match self.runtime() {
            Ok((port, token)) => {
                self.tell_folder(port, &token);
                Ok(json!({
                    "machine": LOCAL,
                    "state": "ready",
                    "ready": true,
                    "browser_url": browser_link(port, &token, "/"),
                    "folder": folder,
                    "message": format!("This session works on this computer again. The host tools (`list_folder`, `read_file`, `run_shell`) don't apply here: use your own file and shell tools. The session has no notebook yet; `new_notebook` or `open_notebook` makes one.{notes}"),
                }))
            }
            Err(why) => Ok(json!({ "machine": LOCAL, "state": "starting", "ready": false, "message": format!("{why}{notes}") })),
        }
    }

    fn stop_machine(&self, args: &Value, deadline: Deadline) -> Result<Value, String> {
        let _one = self.lock_ops(deadline)?;
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        let force = match args.get("force") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => return Err(invalid("force must be true or false")),
        };
        let install = flag_arg(args, "install")?;
        if key.eq_ignore_ascii_case(LOCAL) {
            return self.stop_local(force);
        }
        let server = self.find_machine(&key)?;
        let name = display_name(&server);
        let (link, mut status, _) = self.link_for_use(&server, deadline)?;
        if !matches!(status.state, State::Ready | State::Starting | State::Queued) {
            self.ask(&link, Ask::Attach, install, deadline)?;
            status = self.settle(&link, deadline)?;
        }
        if status.state == State::NeedsInstall {
            return Ok(needs_install_result(&name, &status, "stop_machine"));
        }
        if status.nothing_running {
            return Ok(json!({ "machine": name, "stopped": false, "message": format!("Julia isn't running on {name}, so there is nothing to stop.") }));
        }
        if !force && matches!(status.state, State::Starting | State::Queued) {
            return Ok(waiting_result(&name, &status));
        }
        if !force && let Some(runtime) = status.runtime.as_ref().filter(|_| status.state == State::Ready) {
            let session = self.session();
            let route = Route { port: runtime.port, token: runtime.token.clone(), session: session.clone(), host: Some(crate::mcp::clean_label(&name).unwrap_or_else(|| server.id.clone())) };
            let on_this = self.machine().is_some_and(|m| m.id == server.id);
            let others = self.recent_others(&route, CHECK_WAIT);
            if !on_this {
                self.end_session(route.port, &route.token, &session);
            }
            let others = others?;
            if !others.is_empty() {
                return Ok(others_result(&name, others));
            }
        }
        let stopping = link.clone();
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || drop(done.send(stopping.stop())));
        match waited.recv_timeout(deadline.left().max(STOP_FLOOR)) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Err(why),
            Err(_) => return Ok(json!({ "machine": name, "stopped": false, "message": format!("Stopping Julia on {name} is taking a while. It goes on in the background: call `pluto_session_status` or `list_machines` later to see whether it ended.") })),
        }
        self.update_machine(&server.id, |m| {
            m.active = false;
            m.told = None;
        });
        let cluster = if server.cluster.is_some() { " and its Slurm job was cancelled" } else { "" };
        Ok(json!({
            "machine": name,
            "stopped": true,
            "message": format!("Julia on {name} was stopped{cluster}. Every notebook running there ended. `use_machine` starts it again."),
        }))
    }

    fn stop_local(&self, force: bool) -> Result<Value, String> {
        let dir = &self.options.state_dir;
        let Some(state) = super::running_here(dir) else {
            return Ok(json!({ "machine": LOCAL, "stopped": false, "message": "Julia isn't running on this computer, so there is nothing to stop." }));
        };
        if !force && let Some(port) = state.port {
            let session = self.session();
            let route = Route { port, token: state.token.clone(), session: session.clone(), host: None };
            let on_this = matches!(&*self.target.lock().unwrap(), Target::Local { .. });
            let others = self.recent_others(&route, CHECK_WAIT);
            if !on_this {
                self.end_session(route.port, &route.token, &session);
            }
            let others = others?;
            if !others.is_empty() {
                return Ok(others_result(LOCAL, others));
            }
        }
        // Marked before the runtime is ended, so that a notebook call meanwhile doesn't start it again.
        let marked = match &mut *self.target.lock().unwrap() {
            Target::Local { stopped } if !*stopped => {
                *stopped = true;
                true
            }
            _ => false,
        };
        let unmark = || {
            if marked && let Target::Local { stopped } = &mut *self.target.lock().unwrap() {
                *stopped = false;
            }
        };
        match super::end_runtime(dir) {
            super::Ended::Stopped(_) | super::Ended::NotRunning => {}
            super::Ended::Alive(pid) => {
                unmark();
                return Err(format!("Julia (pid {pid}) is still running after the stop."));
            }
            super::Ended::Elsewhere(node) => {
                unmark();
                return Err(format!("The Julia recorded here runs on {node}, not on this computer."));
            }
        }
        // Only the runtime that was stopped: another thread may have started a new one meanwhile.
        let mut status = self.status.lock().unwrap();
        if matches!(&*status, Local::Ready { port, .. } if Some(*port) == state.port) {
            *status = Local::Idle;
        }
        drop(status);
        Ok(json!({ "machine": LOCAL, "stopped": true, "message": "Julia on this computer was stopped. Every notebook running there ended. `use_machine` with machine \"local\" starts it again." }))
    }

    /// The other sessions that were active in a notebook lately: who, how long ago, and which notebook.
    /// The runtime has `quiet` to answer; if it doesn't, or can't be asked, the error says so, and that `force` stops anyway.
    fn recent_others(&self, route: &Route, quiet: Duration) -> Result<Vec<Value>, String> {
        let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }).to_string();
        let answer: Mutex<Option<Value>> = Mutex::new(None);
        let sink = |reply: String| {
            if let Ok(reply) = serde_json::from_str::<Value>(&reply)
                && reply.get("result").is_some()
            {
                *answer.lock().unwrap() = Some(reply);
            }
        };
        let unknown = |why: String| format!("Nothing was stopped: couldn't check who else is active there ({why}). Stopping ends everyone's notebooks there, so call `stop_machine` again with `force: true` only if the user agrees to stop it anyway.");
        self.post_within(route, &call, &sink, Some(quiet)).map_err(|e| {
            unknown(match e {
                super::Sent::NotConnected(e) | super::Sent::Failed(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => "the runtime didn't answer in time".to_owned(),
                super::Sent::NotConnected(e) | super::Sent::Failed(e) => e.to_string(),
            })
        })?;
        let reply = answer.into_inner().unwrap().ok_or_else(|| unknown("no answer".into()))?;
        if reply["result"]["isError"] == true {
            return Err(unknown(reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned()));
        }
        let listed: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap_or("[]")).map_err(|e| unknown(e.to_string()))?;
        Ok(recent_sessions(&listed))
    }

    /// For a cluster with no job and no resources given: what to ask the user before submitting one. The session stays where it is.
    fn needs_job(&self, name: &str, cluster: &Cluster) -> Value {
        let defaults = resources_json(&cluster.resources, cluster.account.as_deref());
        let partitions: Vec<Value> = cluster.partitions.iter().map(partition_json).collect();
        let partition = cluster.resources.partition.as_deref().map_or("the cluster's default partition".to_owned(), |p| format!("partition {p}"));
        let stays = self.machine().map_or("this computer".to_owned(), |m| m.name);
        json!({
            "machine": name,
            "state": "needs_job",
            "ready": false,
            "needs_job": true,
            "defaults": defaults,
            "partitions": partitions,
            "message": format!(
                "No job is running on {name}, and starting Julia there means submitting a Slurm job that waits in the queue and uses the user's allocation, so nothing was submitted and this session has not moved: it stays on {stays} until `use_machine` is called with machine \"{name}\" and the resources. The saved defaults are {} on {partition}. Ask the user to confirm them or choose others, then call `use_machine` again with machine \"{name}\" and the resources to submit it.",
                cluster.resources.summary()
            ),
        })
    }
}

impl Machine {
    fn new(server: &Server, folder: Option<String>) -> Machine {
        Machine { id: server.id.clone(), name: display_name(server), cluster: server.cluster.is_some(), folder, link: None, active: true, asked: None, told: None }
    }
}

/// The name the agent knows a machine by.
fn display_name(server: &Server) -> String {
    [&server.name, &server.ssh_host, &server.id].into_iter().find(|n| !n.trim().is_empty()).cloned().unwrap_or_default()
}

/// What `stop_machine` says, without `force`, when Julia is starting or a job is queued: other
/// sessions that wait for it can't be seen, and stopping cancels it for them too.
fn waiting_result(name: &str, status: &link::Status) -> Value {
    let what = match (&status.job, &status.queue, status.state) {
        (job, Some(queue), State::Queued) => {
            let id = job.as_ref().map_or(String::new(), |j| format!(" {}", j.id));
            format!("the Slurm job{id} on {name} is {} ({})", queue.state.to_lowercase(), queue_reason_text(&queue.reason))
        }
        (Some(job), _, _) => format!("the Slurm job {} on {name} is starting Julia", job.id),
        _ => format!("Julia is starting on {name}"),
    };
    let mut result = json!({
        "machine": name,
        "stopped": false,
        "state": state_word(status.state),
        "message": format!("Nothing was stopped: {what}. Stopping cancels it. Endeavor can't see which other sessions are waiting for a runtime that isn't up yet, and they would lose it. Tell the user, and call `stop_machine` again with force true only if they agree."),
    });
    if let Some(job) = job_json(status) {
        result["job"] = job;
    }
    if let Some(queue) = queue_json(status) {
        result["queue"] = queue;
    }
    result
}

/// Whether Julia runs in Slurm jobs on the machine, from what `add_machine` was given (`slurm`), the
/// record it had when it was connected to before (`prior`), and whether the helper found Slurm.
fn choose_mode(slurm: Option<bool>, prior: Option<&Server>, found: bool) -> Result<bool, String> {
    match (slurm, prior) {
        (Some(true), _) if !found => Err("slurm true can't be used: Endeavor's helper found no Slurm on that machine (no sinfo). Leave slurm out, or give false, to run Julia there directly.".into()),
        (Some(wanted), _) => Ok(wanted),
        (None, Some(prior)) => Ok(prior.cluster.is_some()),
        (None, None) => Ok(found),
    }
}

/// Sessions in `listed` (the result of `list_notebooks`) that were active in a notebook in the last `RECENT_SECONDS`.
fn recent_sessions(listed: &Value) -> Vec<Value> {
    let mut others: Vec<(u64, Value)> = Vec::new();
    for notebook in listed.as_array().into_iter().flatten() {
        for session in notebook["other_sessions"].as_array().into_iter().flatten() {
            if let Some(ago) = session["active_seconds_ago"].as_u64().filter(|ago| *ago <= RECENT_SECONDS) {
                others.push((ago, json!({ "client": session["client"], "active_seconds_ago": ago, "notebook": notebook["path"] })));
            }
        }
    }
    others.sort_by_key(|(ago, _)| *ago);
    others.into_iter().map(|(_, other)| other).collect()
}

fn others_result(name: &str, others: Vec<Value>) -> Value {
    let who: Vec<String> = others
        .iter()
        .map(|o| {
            let minutes = o["active_seconds_ago"].as_u64().unwrap_or(0) / 60;
            let ago = if minutes == 0 { "less than a minute ago".to_owned() } else { format!("{minutes} min ago") };
            format!("{} ({ago}) in {}", o["client"].as_str().unwrap_or("an unnamed client"), o["notebook"].as_str().unwrap_or("a notebook"))
        })
        .collect();
    json!({
        "machine": name,
        "stopped": false,
        "other_sessions": others,
        "message": format!("Nothing was stopped: another session was active on {name} in the last 15 minutes: {}. Stopping ends their notebooks too. Tell the user, and call `stop_machine` again with force true only if they agree.", who.join("; ")),
    })
}

#[cfg(test)]
mod tests;

//! The front on a machine (docs/plugins-and-remote.md): where this session's
//! notebooks run, the four machine tools the front answers itself, and what a
//! call to a runtime on a machine goes through: the machine's link (`link`),
//! which gives the runtime's port and token on this computer.
//!
//! A session is on this computer or on one machine. Its calls go to the
//! runtime there, a machine's with `X-Endeavor-Host` and `X-Endeavor-Browser-Port`.
//! When the session moves to another runtime its key on the old one is ended and
//! it gets a new key, so nothing of its one-notebook binding carries over.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wire::slurm::{Partition, Resources};

use super::projects::Remembered;
use super::{Relay, Route, Status as Local, start_wait, tool_failure};
use crate::client::{Cluster, Server, ssh_config_hosts};
use crate::link::{self, Link, State};
use crate::mcp::{browser_link, to_json, tool_error};

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

/// Where this session's notebooks run.
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
        State::Ready => format!("Julia on {name} is ready."),
    }
}

/// A link whose build isn't this front's is replaced (quit, then started again) only when no
/// runtime hangs on it, since a new link has another port and the user's browser page would break.
fn replaceable(status: &link::Status) -> bool {
    status.runtime.is_none() && matches!(status.state, State::Connecting | State::Connected | State::Failed)
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
    gres: Option<String>,
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
                Some(0) => None,
                Some(n) if n <= 64 => Some(format!("gpu:{n}")),
                _ => return Err(invalid("gpus must be a count from 0 to 64, or a Slurm gres string such as \"gpu:a100:2\"")),
            },
            Some(Value::String(text)) if !text.trim().is_empty() && !text.chars().any(|c| c.is_control() || c.is_whitespace()) => Some(text.trim().to_owned()),
            Some(_) => return Err(invalid("gpus must be a count, or a Slurm gres string such as \"gpu:a100:2\" with no spaces")),
        };
        let extra = match args.get("extra_sbatch_flags") {
            None | Some(Value::Null) => None,
            Some(Value::Array(flags)) => {
                let flags: Option<Vec<String>> = flags.iter().map(|f| f.as_str().filter(|f| !f.is_empty() && !f.chars().any(char::is_control)).map(str::to_owned)).collect();
                Some(flags.ok_or_else(|| invalid("extra_sbatch_flags must be a list of strings, one flag each"))?)
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
        if self.gres.is_some() {
            resources.gres = self.gres.clone();
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
        match &*self.target.lock().unwrap() {
            Target::Machine(machine) => Some(machine.clone()),
            Target::Local { .. } => None,
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
        let status = machine.link.as_ref()?.status().ok()?;
        status.runtime.filter(|_| status.state == State::Ready).map(|r| (r.port, r.token))
    }

    /// A key the runtime hasn't ended, for a session that moves to another runtime.
    fn new_session(&self) {
        let n = self.sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        *self.session.lock().unwrap() = format!("{}-{n}", self.session_base);
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
                    let _ = link.status();
                }
            }
        });
    }

    /// Where a call goes, waiting up to `start_wait()` for a runtime that is on its way. `wait` false
    /// (for the status tool, which says how far it is) doesn't wait, and a job in the queue isn't waited for either.
    pub(super) fn route(self: &Arc<Self>, wait: bool) -> Result<Route, NotReady> {
        let local = match &*self.target.lock().unwrap() {
            Target::Local { stopped } => Some(*stopped),
            Target::Machine(_) => None,
        };
        match local {
            Some(true) => Err(NotReady::plain("Julia on this computer was stopped from this session with stop_machine. Call `use_machine` with machine \"local\" to start it again.")),
            Some(false) => self.runtime().map(|(port, token)| Route { port, token, host: None }).map_err(NotReady::plain),
            None => self.machine_route(wait),
        }
    }

    fn machine_route(&self, wait: bool) -> Result<Route, NotReady> {
        let deadline = Instant::now() + start_wait();
        let mut connected_since: Option<Instant> = None;
        loop {
            let Some(machine) = self.machine() else { return Err(NotReady::plain("This session moved to another machine while the call waited. Try the call again.")) };
            let name = machine.name.clone();
            let (link, status) = self.machine_link(&machine).map_err(NotReady::plain)?;
            if !machine.active {
                return Err(NotReady::of(&name, &status, format!("Julia on {name} was stopped from this session with stop_machine. Call `use_machine` with machine \"{name}\" to start it again.")));
            }
            if status.state == State::Ready {
                return self.machine_ready(&machine, &status).ok_or_else(|| NotReady::of(&name, &status, "The link to the machine says it is ready but gave no runtime. Try again."));
            }
            if machine.asked != Some(link.pid) {
                link.attach().map_err(NotReady::plain)?;
                self.update_machine(&machine.id, |m| m.asked = Some(link.pid));
                continue;
            }
            match status.state {
                State::Connected if status.nothing_running && machine.cluster => return Err(NotReady::of(&name, &status, self.needs_job_message(&machine))),
                State::Connected if status.nothing_running => {
                    link.start(None).map_err(NotReady::plain)?;
                    continue;
                }
                State::Connected => {
                    if !wait || connected_since.get_or_insert_with(Instant::now).elapsed() > GRACE {
                        return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
                    }
                }
                State::Failed | State::Queued => return Err(NotReady::of(&name, &status, not_ready_message(&name, &status))),
                _ => connected_since = None,
            }
            if !wait || Instant::now() >= deadline {
                return Err(NotReady::of(&name, &status, not_ready_message(&name, &status)));
            }
            std::thread::sleep(POLL);
        }
    }

    /// The machine's link and what it says, started if there is none or it has gone.
    fn machine_link(&self, machine: &Machine) -> Result<(Link, link::Status), String> {
        if let Some(link) = &machine.link
            && let Ok(status) = link.status()
        {
            return Ok((link.clone(), status));
        }
        let link = link::ensure(&machine.id)?;
        let status = link.status()?;
        self.update_machine(&machine.id, |m| m.link = Some(link.clone()));
        Ok((link, status))
    }

    /// Where calls to the machine's runtime go, once it is ready; the session's folder is told to it once.
    fn machine_ready(&self, machine: &Machine, status: &link::Status) -> Option<Route> {
        let runtime = status.runtime.as_ref()?;
        if machine.told != Some(runtime.pid) {
            let folder = machine.folder.clone().or_else(|| status.hello.as_ref().map(|h| h.home.clone()).filter(|h| !h.is_empty()));
            if let Some(folder) = folder {
                self.tell_session_folder(runtime.port, &runtime.token, &folder);
            }
            self.update_machine(&machine.id, |m| m.told = Some(runtime.pid));
        }
        let host = crate::mcp::clean_label(&machine.name).unwrap_or_else(|| machine.id.clone());
        Some(Route { port: runtime.port, token: runtime.token.clone(), host: Some(host) })
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
            && let Some(job) = machine.link.as_ref().and_then(|l| l.status().ok()).and_then(|s| job_json(&s))
        {
            fields.insert("job".into(), job);
        }
        reply["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        true
    }

    /// Answer a call to one of the machine tools.
    pub(super) fn machine_tool(self: &Arc<Self>, message: &Value, tool: &str) {
        let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else { return };
        let arguments = message["params"].get("arguments").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
        let result = match tool {
            "list_machines" => self.list_machines(),
            "add_machine" => self.add_machine(&arguments),
            "use_machine" => self.use_machine(&arguments),
            _ => self.stop_machine(&arguments),
        };
        let result = match result {
            Ok(result) => json!({ "content": [{ "type": "text", "text": to_json(&result) }], "isError": false }),
            Err(e) => tool_error(&e, false),
        };
        let reply = to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        self.write(&self.decorate(message, Some(tool), reply));
    }

    fn list_machines(&self) -> Result<Value, String> {
        let servers = self.machines.load()?;
        let mine = self.machine().map(|m| m.id);
        let machines: Vec<Value> = servers
            .iter()
            .map(|server| {
                let status = link::find(&server.id).ok().flatten().and_then(|l| l.status().ok());
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
        let running = crate::read_state(&self.options.state_dir).is_some_and(|s| crate::pid_alive(s.pid, s.started));
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

    fn add_machine(&self, args: &Value) -> Result<Value, String> {
        let _one = self.ops.lock().unwrap();
        let deadline = Instant::now() + start_wait();
        let typed = text_arg(args, "host")?.ok_or_else(|| invalid("host is required: an ssh alias from ~/.ssh/config, or user@host"))?;
        let (host, port) = Server::parse_target(&typed).map_err(invalid)?;
        let given_name = text_arg(args, "name")?;
        let name = match &given_name {
            Some(name) => name.clone(),
            None => default_name(&host),
        };
        valid_name(&name)?;
        let julia = text_arg(args, "julia")?;
        let servers = self.machines.load()?;
        let by_name = |n: &str| servers.iter().find(|s| s.name.eq_ignore_ascii_case(n)).cloned();
        let existing = match &given_name {
            Some(name) => by_name(name),
            None => by_name(&name).or_else(|| servers.iter().find(|s| s.ssh_host == host && s.port == port).cloned()),
        };
        let mut record = match &existing {
            Some(existing) => existing.clone(),
            None => Server { id: new_id(&name, &servers)?, name: name.clone(), ..Default::default() },
        };
        let moved = existing.as_ref().is_some_and(|e| e.ssh_host != host || e.port != port || julia.as_ref().is_some_and(|j| e.julia.as_ref() != Some(j)));
        record.ssh_host = host;
        record.port = port;
        if julia.is_some() {
            record.julia = julia;
        }
        self.machines.save(record.clone())?;
        let undo = |this: &Relay| {
            let _ = match &existing {
                Some(before) => this.machines.save(before.clone()),
                None => this.machines.remove(&record.id).map(|_| ()),
            };
            if let Ok(Some(link)) = link::find(&record.id) {
                let _ = link.quit();
            }
        };
        // A link that failed before, or that was made for another address, connects afresh.
        if let Ok(Some(old)) = link::find(&record.id)
            && (moved || old.status().is_ok_and(|s| s.state == State::Failed))
        {
            let _ = old.quit();
        }
        let link = link::ensure(&record.id).inspect_err(|_| undo(self))?;
        let status = loop {
            let status = link.status().inspect_err(|_| undo(self))?;
            match status.state {
                State::Failed => {
                    undo(self);
                    return Err(format!(
                        "Couldn't connect to {}: {}\nNothing was saved. This is for the user to fix in a terminal (never ask them for a password or passphrase here, and don't run ssh yourself), then call `add_machine` again.",
                        record.ssh_target(),
                        status.error.as_deref().unwrap_or("no reason was given")
                    ));
                }
                State::Connecting if Instant::now() >= deadline => {
                    return Ok(json!({
                        "machine": record.name, "host": record.ssh_target(), "state": "connecting", "saved": true,
                        "step": status.step,
                        "message": format!("Still connecting to {}. Call `add_machine` again with the same host to continue; the machine is saved already.", record.ssh_target()),
                    }));
                }
                State::Connecting => std::thread::sleep(POLL),
                _ => break status,
            }
        };
        let mut status = status;
        while status.hello.as_ref().is_some_and(|h| h.slurm && h.partitions.is_none()) && Instant::now() < deadline {
            std::thread::sleep(POLL);
            status = link.status().inspect_err(|_| undo(self))?;
        }
        let hello = status.hello.clone().unwrap_or_default();
        let was_cluster = record.cluster.is_some();
        let plain_by_choice = existing.as_ref().is_some_and(|e| e.cluster.is_none());
        if hello.slurm && !plain_by_choice {
            let before = record.cluster.take();
            let mut cluster = before.clone().unwrap_or_default();
            cluster.partitions = hello.partitions.clone().unwrap_or(cluster.partitions);
            cluster.scratch = hello.scratch.clone().or(cluster.scratch);
            if before.is_none() {
                let mut resources = Resources::default();
                resources.clip(cluster.partition(None));
                cluster.resources = resources;
            }
            record.cluster = Some(cluster);
        }
        self.machines.save(record.clone())?;
        if record.cluster.is_some() && !was_cluster {
            // It connected as a plain server: the next connection starts the helper for Slurm.
            let _ = link.quit();
        }
        let partitions: Vec<Partition> = record.cluster.as_ref().map(|c| c.partitions.clone()).or_else(|| hello.partitions.clone()).unwrap_or_default();
        let mut message = format!("Connected to {} (node {}, home folder {}). ", record.ssh_target(), hello.node, hello.home);
        if hello.slurm {
            message.push_str("It has Slurm");
            if plain_by_choice {
                message.push_str(", but it was saved before as a plain server, so Julia runs on it directly and not in a job. ");
            } else if partitions.is_empty() {
                message.push_str(", but it didn't list its partitions. ");
            } else {
                message.push_str(&format!(". Partitions: {}. ", partitions.iter().map(partition_text).collect::<Vec<_>>().join("; ")));
            }
            if let Some(cluster) = &record.cluster {
                message.push_str(&format!("Default job: {}. ", cluster.resources.summary()));
            }
        } else {
            message.push_str("It has no Slurm, so Julia runs there directly. ");
        }
        match &hello.julia {
            Some(julia) => message.push_str(&format!("Julia {} is at {}. ", julia.version, julia.path)),
            None => message.push_str("Endeavor looks for Julia when it first starts a runtime there. "),
        }
        message.push_str(&format!("The machine is saved as \"{}\". Call `use_machine` to work on it.", record.name));
        Ok(json!({
            "machine": record.name,
            "host": record.ssh_target(),
            "state": "connected",
            "saved": true,
            "updated": existing.is_some(),
            "node": hello.node,
            "home": hello.home,
            "os": hello.os,
            "arch": hello.arch,
            "slurm": hello.slurm,
            "partitions": partitions.iter().map(partition_json).collect::<Vec<_>>(),
            "scratch": record.cluster.as_ref().and_then(|c| c.scratch.clone()),
            "julia": hello.julia,
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

    /// The machine's link, started if there is none. A link from another build is replaced only when
    /// nothing hangs on it (`replaceable`); else it is used, and the second is why.
    fn link_for_use(&self, server: &Server) -> Result<(Link, Option<String>), String> {
        let link = link::ensure(&server.id)?;
        if link.build == crate::embedded::BUILD_VERSION {
            return Ok((link, None));
        }
        if link.status().is_ok_and(|status| replaceable(&status)) {
            let _ = link.quit();
            return Ok((link::ensure(&server.id)?, None));
        }
        let note = format!(
            "The link to {} was started by another build of endeavor ({}) and goes on, because a runtime is in use through it and a new link would change the address of the user's browser page. It is replaced when it ends.",
            display_name(server),
            link.build
        );
        Ok((link, Some(note)))
    }

    /// Wait up to `deadline` for the link to settle: ready, queued, failed, found nothing running, or
    /// connected with nothing under way.
    fn settle(&self, link: &Link, deadline: Instant) -> Result<link::Status, String> {
        let mut connected_since: Option<Instant> = None;
        loop {
            let status = link.status()?;
            match status.state {
                State::Ready | State::Queued | State::Failed => return Ok(status),
                State::Connected if status.nothing_running => return Ok(status),
                State::Connected if connected_since.get_or_insert_with(Instant::now).elapsed() > GRACE => return Ok(status),
                State::Connected => {}
                _ => connected_since = None,
            }
            if Instant::now() >= deadline {
                return Ok(status);
            }
            std::thread::sleep(POLL);
        }
    }

    fn use_machine(self: &Arc<Self>, args: &Value) -> Result<Value, String> {
        let _one = self.ops.lock().unwrap();
        let deadline = Instant::now() + start_wait();
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        if key.eq_ignore_ascii_case(LOCAL) {
            return self.use_local();
        }
        let server = self.find_machine(&key)?;
        let given = Given::parse(args)?;
        let folder = text_arg(args, "folder")?;
        if given.any() && server.cluster.is_none() {
            return Err(invalid(format!("{} isn't a Slurm cluster, so partition, cpus, memory_gb, hours, gpus, account and extra_sbatch_flags don't apply to it. Leave them out.", display_name(&server))));
        }
        let name = display_name(&server);
        let (link, note) = self.link_for_use(&server)?;
        let was_ready = link.status().is_ok_and(|status| status.state == State::Ready);
        self.move_to(&server, folder.clone(), &link);
        let mut notes: Vec<String> = note.into_iter().collect();
        if let Err(e) = self.projects.set(&self.options.folder, Some(Remembered { machine: server.id.clone(), folder: folder.clone() })) {
            notes.push(format!("The project won't remember this machine: {e}"));
        }
        let mut status;
        match &server.cluster {
            None => {
                link.start(None)?;
                status = self.settle(&link, deadline)?;
            }
            Some(cluster) => {
                link.attach()?;
                status = self.settle(&link, deadline)?;
                if status.nothing_running {
                    if !given.any() {
                        return Ok(needs_job(&name, cluster));
                    }
                    let (resources, account) = given.over(cluster)?;
                    let mut job = cluster.job(&resources);
                    job.account = account.clone();
                    link.start(Some(job))?;
                    let mut saved = server.clone();
                    if let Some(c) = saved.cluster.as_mut() {
                        c.resources = resources;
                        c.account = account;
                    }
                    if let Err(e) = self.save_defaults(&saved) {
                        notes.push(format!("These resources weren't saved as the machine's defaults: {e}"));
                    }
                    status = self.settle(&link, deadline)?;
                } else if given.any() {
                    notes.push("A job is already queued or running there, so the resources you gave were not used.".into());
                }
            }
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

    /// The session's target becomes `server`, ending its key on the runtime it leaves.
    fn move_to(&self, server: &Server, folder: Option<String>, link: &Link) {
        let previous = std::mem::replace(&mut *self.target.lock().unwrap(), Target::Machine(Machine::new(server, folder.clone())));
        let same = matches!(&previous, Target::Machine(m) if m.id == server.id);
        if !same {
            self.leave(&previous);
            self.new_session();
        }
        self.update_machine(&server.id, |m| {
            m.link = Some(link.clone());
            m.asked = Some(link.pid);
        });
    }

    /// End this session's key on the runtime `target` has, if it is up. Best effort.
    fn leave(&self, target: &Target) {
        let runtime = match target {
            Target::Local { .. } => match &*self.status.lock().unwrap() {
                Local::Ready { port, token } => Some((*port, token.clone())),
                _ => None,
            },
            Target::Machine(machine) => self.machine_runtime(machine),
        };
        if let Some((port, token)) = runtime {
            self.end_session(port, &token);
        }
    }

    fn use_result(&self, server: &Server, status: &link::Status, was_ready: bool, notes: Vec<String>) -> Result<Value, String> {
        let name = display_name(server);
        let notes = if notes.is_empty() { String::new() } else { format!(" {}", notes.join(" ")) };
        match status.state {
            State::Ready => {
                let machine = self.machine().ok_or("The session moved to another machine.")?;
                let Some(route) = self.machine_ready(&machine, status) else { return Err(format!("The link to {name} says it is ready but gave no runtime. Call `use_machine` again.")) };
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
            State::Failed => Err(format!("{}{notes}", not_ready_message(&name, status))),
            _ => {
                let mut result = status_result(&name, status, &format!("{}{notes}", not_ready_message(&name, status)));
                result["this_session"] = true.into();
                Ok(result)
            }
        }
    }

    /// `use_machine` with this computer.
    fn use_local(self: &Arc<Self>) -> Result<Value, String> {
        let previous = std::mem::replace(&mut *self.target.lock().unwrap(), Target::Local { stopped: false });
        match &previous {
            Target::Machine(_) => {
                self.leave(&previous);
                self.new_session();
            }
            Target::Local { stopped: true } => self.new_session(),
            Target::Local { .. } => {}
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

    fn stop_machine(&self, args: &Value) -> Result<Value, String> {
        let _one = self.ops.lock().unwrap();
        let deadline = Instant::now() + start_wait();
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        let force = match args.get("force") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => return Err(invalid("force must be true or false")),
        };
        if key.eq_ignore_ascii_case(LOCAL) {
            return self.stop_local(force);
        }
        let server = self.find_machine(&key)?;
        let name = display_name(&server);
        let (link, _) = self.link_for_use(&server)?;
        let mut status = link.status()?;
        if !matches!(status.state, State::Ready | State::Starting | State::Queued) {
            link.attach()?;
            status = self.settle(&link, deadline)?;
        }
        if status.nothing_running {
            return Ok(json!({ "machine": name, "stopped": false, "message": format!("Julia isn't running on {name}, so there is nothing to stop.") }));
        }
        if !force && let Some(runtime) = status.runtime.as_ref().filter(|_| status.state == State::Ready) {
            let route = Route { port: runtime.port, token: runtime.token.clone(), host: Some(crate::mcp::clean_label(&name).unwrap_or_else(|| server.id.clone())) };
            let on_this = self.machine().is_some_and(|m| m.id == server.id);
            let others = self.recent_others(&route);
            if !on_this {
                self.end_session(route.port, &route.token);
            }
            let others = others?;
            if !others.is_empty() {
                return Ok(others_result(&name, others));
            }
        }
        let stopping = link.clone();
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || drop(done.send(stopping.stop())));
        match waited.recv_timeout(deadline.saturating_duration_since(Instant::now()).max(STOP_FLOOR)) {
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
        let running = crate::read_state(dir).filter(|s| s.node == crate::hostname() && crate::pid_alive(s.pid, s.started));
        let Some(state) = running else {
            return Ok(json!({ "machine": LOCAL, "stopped": false, "message": "Julia isn't running on this computer, so there is nothing to stop." }));
        };
        if !force && let Some(port) = state.port {
            let route = Route { port, token: state.token.clone(), host: None };
            let on_this = matches!(&*self.target.lock().unwrap(), Target::Local { .. });
            let others = self.recent_others(&route);
            if !on_this {
                self.end_session(route.port, &route.token);
            }
            let others = others?;
            if !others.is_empty() {
                return Ok(others_result(LOCAL, others));
            }
        }
        match super::end_runtime(dir) {
            super::Ended::Stopped(_) | super::Ended::NotRunning => {}
            super::Ended::Alive(pid) => return Err(format!("Julia (pid {pid}) is still running after the stop.")),
            super::Ended::Elsewhere(node) => return Err(format!("The Julia recorded here runs on {node}, not on this computer.")),
        }
        *self.status.lock().unwrap() = Local::Idle;
        if let Target::Local { stopped } = &mut *self.target.lock().unwrap() {
            *stopped = true;
        }
        Ok(json!({ "machine": LOCAL, "stopped": true, "message": "Julia on this computer was stopped. Every notebook running there ended. `use_machine` with machine \"local\" starts it again." }))
    }

    /// The other sessions that were active in a notebook lately: who, how long ago, and which notebook.
    fn recent_others(&self, route: &Route) -> Result<Vec<Value>, String> {
        let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }).to_string();
        let answer: Mutex<Option<Value>> = Mutex::new(None);
        let sink = |reply: String| {
            if let Ok(reply) = serde_json::from_str::<Value>(&reply)
                && reply.get("result").is_some()
            {
                *answer.lock().unwrap() = Some(reply);
            }
        };
        let unknown = |why: String| format!("Couldn't find out whether other sessions are active there ({why}). If the user wants it stopped anyway, call again with force true.");
        self.post(route, &call, &sink).map_err(|e| unknown(match e {
            super::Sent::NotConnected(e) | super::Sent::Failed(e) => e.to_string(),
        }))?;
        let reply = answer.into_inner().unwrap().ok_or_else(|| unknown("no answer".into()))?;
        if reply["result"]["isError"] == true {
            return Err(unknown(reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned()));
        }
        let listed: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap_or("[]")).map_err(|e| unknown(e.to_string()))?;
        Ok(recent_sessions(&listed))
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

fn needs_job(name: &str, cluster: &Cluster) -> Value {
    let defaults = resources_json(&cluster.resources, cluster.account.as_deref());
    let partitions: Vec<Value> = cluster.partitions.iter().map(partition_json).collect();
    let partition = cluster.resources.partition.as_deref().map_or("the cluster's default partition".to_owned(), |p| format!("partition {p}"));
    json!({
        "machine": name,
        "state": "needs_job",
        "ready": false,
        "needs_job": true,
        "defaults": defaults,
        "partitions": partitions,
        "message": format!(
            "No job is running on {name}, and starting Julia there means submitting a Slurm job that waits in the queue and uses the user's allocation, so nothing was submitted. The saved defaults are {} on {partition}. Ask the user to confirm them or choose others, then call `use_machine` again with machine \"{name}\" and the resources to submit it.",
            cluster.resources.summary()
        ),
    })
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

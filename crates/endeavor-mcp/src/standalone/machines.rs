use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wire::slurm::{Partition, Resources, check_extra_flag};

use super::install_page::Clicked;
use super::projects::Remembered;
use super::target::{OtherBuild, Provider, Target, local_server};
use super::{Relay, Route, tool_failure};
use crate::client::{Asker, Auth, Reply, Cluster, Config, InstallInfo, Launcher, Messages, Outcome, Running, RuntimeInfo, Server, Session, State, Status, Transport, Want, ssh_config_hosts, this_platform};
use crate::mcp::{browser_link, to_json, tool_error};
use crate::notebooks::Folder;

pub(super) fn text_result(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

pub(super) const LOCAL: &str = "local";

const RECENT_SECONDS: u64 = 15 * 60;

const CHECK_WAIT: Duration = Duration::from_secs(5);

const CONNECT_WAIT: Duration = Duration::from_secs(10);

const MIN_CALL: Duration = Duration::from_millis(300);

pub(super) fn test_var(name: &str) -> Option<std::ffi::OsString> {
    if cfg!(debug_assertions) { std::env::var_os(name) } else { None }
}

fn open_session(server: Server, allow_install: bool, launcher: Option<Launcher>, wanted: &Arc<Mutex<Instant>>) -> Result<(Session, Option<Asker>), String> {
    let (id, name) = (server.id.clone(), server.display_name());
    let asker = sign_in(&server, wanted);
    let var = |name: &str| test_var(name).and_then(|v| v.into_string().ok()).unwrap_or_default().replace("{id}", &id);
    let helper = |os: &str, arch: &str| {
        if (os.to_owned(), arch.to_owned()) == this_platform() {
            crate::this_program()
        } else {
            crate::release::helper_for(os, arch, &crate::paths::Env::here().helpers_dir())
        }
    };
    let mut config = Config::new(server, helper);
    if test_var("ENDEAVOR_TEST_SHELL").is_some() {
        config.transport = Transport::Shell { env: Vec::new(), ask: test_var("ENDEAVOR_TEST_ASK").and_then(|v| v.into_string().ok()) };
    }
    (config.root, config.state, config.depot, config.allow_install) = (var("ENDEAVOR_TEST_ROOT"), var("ENDEAVOR_TEST_STATE"), var("ENDEAVOR_TEST_DEPOT"), allow_install);
    config.launcher = launcher;
    config.julia_when_needed = test_var("ENDEAVOR_TEST_JULIA_AT_START").is_none();
    config.messages = Messages {
        restart_failed: |name| format!("Endeavor on {name} couldn't start. Call use_machine to try again."),
        restart_needs_install: |name, items| format!("Endeavor on {name} couldn't start. {}", install_text(name, &InstallInfo { items: items.to_vec(), helper: None }, "use_machine")),
        not_connected: |name| format!("Endeavor isn't connected to {name}. Call use_machine to use it again."),
        no_run_gate: None,
        tell_other_build: false,
    };
    let asker = asker.and_then(|asker| askpass_program().map_err(|e| eprintln!("┌ Warning: {e}; {name} signs in with keys only")).ok().map(|program| (asker, program)));
    if let Some((asker, program)) = &asker {
        config.auth = Auth::Env(asker.env(program));
    }
    Ok((Session::new(config)?, asker.map(|(asker, _)| asker)))
}

fn askpass_program() -> Result<std::path::PathBuf, String> {
    if cfg!(target_os = "linux") {
        return Ok(format!("/proc/{}/exe", std::process::id()).into());
    }
    crate::this_program()
}

fn login_julia(provider: &dyn Provider, name: &str, runtime: &RuntimeInfo, deadline: &Deadline) -> Option<String> {
    let again = "Call `use_machine` again with `install: true` in a minute to finish; until then a Julia notebook there can't start.";
    match crate::bridge_rpc(runtime.port, &runtime.token, "endeavor/julia_status", json!({})) {
        Ok(reply) if reply["result"]["state"] == "failed" && reply["result"]["code"] == "julia_not_found" => {}
        Ok(_) => return None,
        Err(e) => return Some(format!("Couldn't ask Endeavor on {name} whether it has Julia ({e}). Calling `use_machine` again with `install: true` tries again.")),
    }
    loop {
        let path = match provider.files(wire::files::Request::Julia { download: true }, deadline.left()) {
            Some(Ok(wire::files::Reply::Julia { julia: wire::files::JuliaState::Ready { path, .. } })) => path,
            Some(Ok(wire::files::Reply::Julia { julia: wire::files::JuliaState::Downloading { step } })) => {
                if deadline.left() > Duration::from_secs(3) {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                return Some(format!("Endeavor is getting Julia on {name}'s login node ({step}). {again}"));
            }
            Some(Ok(wire::files::Reply::Error { message })) => return Some(format!("Couldn't get Julia on {name}'s login node: {message} Calling `use_machine` again with `install: true` tries again.")),
            Some(Ok(_)) => return Some(format!("{name}'s helper gave an answer about Julia that this Endeavor doesn't know, so Julia wasn't set up there. Tell the user; `stop_machine` and `use_machine` again start a new job.")),
            Some(Err(e)) => return Some(format!("Couldn't ask {name} about Julia ({e}). Calling `use_machine` again with `install: true` tries again.")),
            None => return Some(format!("{name} didn't say about Julia in time. {again}")),
        };
        return crate::bridge_rpc(runtime.port, &runtime.token, "endeavor/use_julia", json!({ "path": path }))
            .err()
            .map(|e| format!("Couldn't tell Endeavor on {name} where Julia is ({e}); calling `use_machine` again with `install: true` tries again."));
    }
}

const SIGN_IN_WINDOW: Duration = Duration::from_secs(2 * 60);

const SIGN_IN_WAIT: Duration = Duration::from_secs(100);

static SIGN_IN: std::sync::LazyLock<super::signin_page::SignIn> = std::sync::LazyLock::new(Default::default);

fn sign_in(server: &Server, wanted: &Arc<Mutex<Instant>>) -> Option<Asker> {
    if !super::browser::shows(|name| std::env::var(name).ok()) {
        return None;
    }
    let (name, wanted) = (server.display_name(), wanted.clone());
    Asker::replying(move |ask| {
        let wait = test_var("ENDEAVOR_TEST_SIGN_IN_WAIT_MS").and_then(|ms| ms.into_string().ok()?.parse().ok()).map_or(SIGN_IN_WAIT, Duration::from_millis);
        put_to_user(&name, &wanted, || SIGN_IN.ask(&name, &ask, wait, &open_page))
    })
    .map_err(|e| eprintln!("┌ Warning: {e}; {} signs in with keys only", server.display_name()))
    .ok()
}

fn put_to_user(name: &str, wanted: &Mutex<Instant>, ask: impl FnOnce() -> Reply) -> Reply {
    if wanted.lock().unwrap().elapsed() > SIGN_IN_WINDOW {
        eprintln!("[ Info: {name} asked to sign in while no machine tool waited for it; not asking the user");
        return Reply::GaveUp;
    }
    let reply = ask();
    if matches!(reply, Reply::Answered(_)) {
        *wanted.lock().unwrap() = Instant::now();
    }
    reply
}

struct Held {
    server: Server,
    session: Arc<Session>,
    saved: bool,
    launcher: Option<Launcher>,
    wanted: Arc<Mutex<Instant>>,
    _asker: Option<Asker>,
}

impl Held {
    fn new(server: &Server, allow_install: bool, saved: bool, launcher: Option<Launcher>) -> Result<Held, String> {
        let wanted = Arc::new(Mutex::new(Instant::now()));
        let (session, asker) = open_session(server.clone(), allow_install, launcher, &wanted)?;
        Ok(Held { server: server.clone(), session: Arc::new(session), saved, launcher, wanted, _asker: asker })
    }

    fn asked_for(&self) -> Arc<Session> {
        *self.wanted.lock().unwrap() = Instant::now();
        self.session.clone()
    }

    fn replaceable(&self) -> bool {
        replaceable(&self.session.status())
    }
}

fn replaceable(status: &Status) -> bool {
    matches!(status.state, State::Connecting | State::Connected | State::NothingRunning | State::Failed(_) | State::NeedsInstall(_))
}

impl Drop for Held {
    fn drop(&mut self) {
        let session = self.session.clone();
        std::thread::spawn(move || session.close());
    }
}

#[derive(Default)]
pub(super) struct Connections {
    held: Mutex<HashMap<String, Held>>,
}

struct Unsaved<'a> {
    connections: &'a Connections,
    id: String,
    leave: bool,
}

impl Drop for Unsaved<'_> {
    fn drop(&mut self) {
        if !self.leave {
            self.connections.drop_unsaved(Some(&self.id));
        }
    }
}

fn settings_changed(name: &str) -> String {
    format!("The settings of {name} changed while Endeavor is in use on it through this session with the old ones. Call `stop_machine` for {name} (with the user's agreement), or put the settings back.")
}

impl Connections {
    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.held.lock().unwrap().get(id).filter(|held| held.saved).map(Held::asked_for)
    }

    fn open(&self, server: &Server, allow_install: bool) -> Result<Arc<Session>, String> {
        let mut held = self.held.lock().unwrap();
        if let Some(old) = held.get(&server.id).filter(|old| old.saved) {
            if old.server.same_connection(server) {
                return Ok(old.asked_for());
            }
            if !old.replaceable() {
                return Err(settings_changed(&server.display_name()));
            }
        }
        let fresh = Held::new(server, allow_install, true, None)?;
        let session = fresh.session.clone();
        let old = held.insert(server.id.clone(), fresh);
        drop(held);
        drop(old);
        Ok(session)
    }

    fn trying(&self, record: &Server, allow_install: bool, launcher: Option<Launcher>) -> Result<(Arc<Session>, Unsaved<'_>), String> {
        let mut held = self.held.lock().unwrap();
        let abandoned: Vec<Held> = held.extract_if(|id, h| !h.saved && *id != record.id).map(|(_, h)| h).collect();
        let unsaved = || Unsaved { connections: self, id: record.id.clone(), leave: false };
        match held.get(&record.id) {
            Some(old) if old.server.same_connection(record) && (launcher.is_none() || old.launcher == launcher) => return Ok((old.asked_for(), unsaved())),
            Some(old) if old.saved && !old.replaceable() => return Err(settings_changed(&record.display_name())),
            _ => {}
        }
        let fresh = Held::new(record, allow_install, false, launcher)?;
        let session = fresh.session.clone();
        let old = held.insert(record.id.clone(), fresh);
        drop(held);
        drop((old, abandoned));
        Ok((session, unsaved()))
    }

    fn save(&self, record: &Server) {
        if let Some(held) = self.held.lock().unwrap().get_mut(&record.id) {
            (held.server, held.saved) = (record.clone(), true);
        }
    }

    fn drop_unsaved(&self, id: Option<&str>) {
        let ended: Vec<Held> = self.held.lock().unwrap().extract_if(|key, held| !held.saved && id.is_none_or(|id| id == key)).map(|(_, held)| held).collect();
        drop(ended);
    }

    fn forget(&self, id: &str) {
        let old = self.held.lock().unwrap().remove(id);
        drop(old);
    }

    fn status(&self, id: &str) -> Option<Status> {
        self.held.lock().unwrap().get(id).filter(|held| held.saved).map(|held| held.session.status())
    }

    pub(super) fn close_all(&self) {
        let all: Vec<Held> = self.held.lock().unwrap().drain().map(|(_, held)| held).collect();
        let ending: Vec<_> = all.into_iter().map(|held| std::thread::spawn(move || held.session.close())).collect();
        for ending in ending {
            let _ = ending.join();
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Deadline(Instant);

impl Deadline {
    pub(super) fn after(wait: Duration) -> Deadline {
        Deadline(Instant::now() + wait)
    }

    pub(super) fn left(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub(super) fn spent(self) -> bool {
        self.left() < MIN_CALL
    }
}

pub(super) struct Reached {
    outcome: Outcome,
    status: Status,
}

pub(super) struct NotReady {
    name: String,
    message: String,
    reached: Option<Box<Reached>>,
    idle: bool,
    stopped: bool,
}

impl NotReady {
    pub(super) fn nothing_runs(&self) -> bool {
        self.idle || self.stopped
    }

    fn plain(message: impl Into<String>) -> NotReady {
        NotReady { name: String::new(), message: message.into(), reached: None, idle: false, stopped: false }
    }

    fn of(name: &str, outcome: Outcome, status: Status, message: impl Into<String>) -> NotReady {
        NotReady { name: name.to_owned(), message: message.into(), reached: Some(Box::new(Reached { outcome, status })), idle: false, stopped: false }
    }
}

fn place(name: &str) -> &str {
    if name == LOCAL { "this computer" } else { name }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Need {
    Start,
    Look,
    Peek,
}

fn invalid(message: impl std::fmt::Display) -> String {
    format!("ArgumentError: invalid_argument::{message}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn state_word(state: &State) -> &'static str {
    match state {
        State::Connecting => "connecting",
        State::Connected | State::NothingRunning => "connected",
        State::Starting { .. } => "starting",
        State::Queued(_) => "queued",
        State::Ready(_) => "ready",
        State::Failed(_) => "failed",
        State::NeedsInstall(_) => "needs_install",
    }
}

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

fn not_ready_message(name: &str, reached: &Reached) -> String {
    let at = place(name);
    match &reached.outcome {
        Outcome::StillWorking(step) => {
            let step = if step.is_empty() { String::new() } else { format!(" Last step: {}.", step.trim_end_matches('.')) };
            if matches!(reached.status.state, State::Starting { .. }) {
                format!("Endeavor is starting on {at}. The first start can take a few minutes.{step} To wait, call the notebook tool you want again: each call waits up to 45 seconds. `session_status` answers at once and only shows the step, so don't call it repeatedly.")
            } else {
                format!("Endeavor is connecting to {name}.{step} To wait, call the notebook tool you want again: each call waits up to 45 seconds. `session_status` answers at once and only shows the step, so don't call it repeatedly. If it stays like this, call `use_machine` again.")
            }
        }
        Outcome::NothingRunning => format!("Endeavor isn't running on {at} right now. Call `use_machine` with machine \"{name}\" to start it."),
        Outcome::Queued { job, queue } => {
            let job = job.as_ref().map(|j| format!(" {}", j.id)).unwrap_or_default();
            let (what, wait) = if queue.state == "RUNNING" {
                (format!("running on node {}, and Endeavor is starting there", queue.reason), "")
            } else {
                (format!("waiting in the queue: {}", queue_reason_text(&queue.reason)), " A queued job can wait minutes or hours: after a few tries, stop and let the user say when to check again.")
            };
            format!("The Slurm job{job} on {name} is {what}. Tell the user. To wait, call the notebook tool you want again: each call waits up to 45 seconds. `session_status` answers at once and only shows the job's state, so don't call it repeatedly.{wait}")
        }
        Outcome::Failed(error) => {
            let error = if error.is_empty() { "it didn't say why" } else { error };
            format!("Endeavor on {at} isn't available: {error}\nCall `use_machine` with machine \"{name}\" to try again, or tell the user.")
        }
        Outcome::NeedsInstall(info) => format!("{} Nothing was installed.", install_text(name, info, "use_machine")),
        Outcome::Ready(_) => format!("Endeavor on {at} is ready."),
    }
}

const ASK_IN_CHAT: &str = "Ask the user whether Endeavor may do that. Only if they agree, call ";
const ASKED_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that the app they talk to you in shows, which names what it installs, as soon as a call needs it: tell the user it needs their OK, then call ";
const CALL_END_IN_CHAT: &str = " and `install: true`.";
const CALL_END_IN_DIALOG: &str = " and `install: true` now, after one line telling the user that Endeavor is asking for their OK, and for what, in the same reply; don't ask in the chat.";

fn asked_in_dialog(text: &str) -> String {
    let Some(at) = text.find(ASK_IN_CHAT) else { return text.to_owned() };
    let rest = &text[at + ASK_IN_CHAT.len()..];
    let Some(end) = rest.find(CALL_END_IN_CHAT) else { return text.to_owned() };
    format!("{}{ASKED_IN_DIALOG}{}{CALL_END_IN_DIALOG}{}", &text[..at], &rest[..end], &rest[end + CALL_END_IN_CHAT.len()..])
}

const ASKED_ON_PAGE: &str = "Endeavor asks the user itself, on a page that it opens in their browser and that names what it installs, once a call has `install: true`: tell the user it needs their OK, then call ";

fn asked_on_page(text: &str) -> String {
    let Some(at) = text.find(ASK_IN_CHAT) else { return text.to_owned() };
    let rest = &text[at + ASK_IN_CHAT.len()..];
    let Some(end) = rest.find(CALL_END_IN_CHAT) else { return text.to_owned() };
    format!("{}{ASKED_ON_PAGE}{}{CALL_END_IN_DIALOG}{}", &text[..at], &rest[..end], &rest[end + CALL_END_IN_CHAT.len()..])
}

pub(super) fn open_page(url: &str) -> bool {
    if let Some(file) = test_var("ENDEAVOR_TEST_INSTALL_PAGES").filter(|file| !file.is_empty()) {
        use std::io::Write;
        return std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{url}")).is_ok();
    }
    super::browser::open(url)
}

const CANT_ASK: &str = "Endeavor can't install it from this app: it asks the user itself before it installs anything, and here it can neither show its dialog nor open a page in their browser. Setting `install` installs nothing here. Tell the user what it needs and where; to install it, they can ask for it from Claude Code in a terminal (not `claude -p`), which shows Endeavor's dialog, or from an app whose Endeavor runs on a computer that can open their browser. Don't work around it: don't make the notebook on this computer instead, and don't suggest commands or settings that these messages don't name; there are none that install it from here.";

const CANT_ASK_ERROR: &str = "install_cant_ask::Endeavor can't install it from this app: it asks the user itself before it installs anything, and here it can neither show its dialog nor open a page in their browser. Nothing was installed, and calling again won't install it. Tell the user what it needs and where; to install it, they can ask for it from Claude Code in a terminal (not `claude -p`), which shows Endeavor's dialog, or from an app whose Endeavor runs on a computer that can open their browser. Don't work around it: don't make the notebook on this computer instead, and don't suggest commands or settings that these messages don't name; there are none that install it from here.";

fn cant_ask(text: &str) -> String {
    let Some(at) = text.find(ASK_IN_CHAT) else { return text.to_owned() };
    let rest = &text[at + ASK_IN_CHAT.len()..];
    let Some(end) = rest.find(CALL_END_IN_CHAT) else { return text.to_owned() };
    format!("{}{CANT_ASK}{}", &text[..at], &rest[end + CALL_END_IN_CHAT.len()..])
}

pub(super) const PAGE_CALL: Duration = Duration::from_secs(55);

const INSTALL_SHARE: Duration = Duration::from_secs(20);

pub(super) const MIN_INSTALL: Duration = Duration::from_secs(5);

pub(super) fn page_wait() -> Duration {
    test_var("ENDEAVOR_TEST_PAGE_WAIT_MS").and_then(|ms| ms.to_str()?.parse().ok()).map_or(Duration::from_secs(45), Duration::from_millis)
}

fn install_question(name: &str, items: &[wire::Item], tool: &str) -> String {
    let what = if items.is_empty() { format!("Endeavor needs to install its helper program on {name}.") } else { wire::needs_text(items, name) };
    let later = if tool == "use_machine" && name != LOCAL && !items.iter().any(|item| item.kind == wire::KIND_RUNTIME) {
        " The same yes lets Endeavor download its own Julia there if a notebook needs Julia and none is found."
    } else {
        ""
    };
    format!("{what}{later} Install it now?")
}

fn leave_question(machine: &str) -> String {
    if machine == LOCAL || machine.is_empty() {
        "If an R notebook needs R and none is found on this computer, may Endeavor install its own copy of R?".to_owned()
    } else {
        format!("If a notebook on {machine} needs Julia and none is found there, may Endeavor download its own copy of Julia?")
    }
}

fn install_text(name: &str, info: &InstallInfo, tool: &str) -> String {
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
            Some(Running::Process { pid, checked: true }) => format!(" Endeavor is already running there (process {pid}); {attach}"),
            Some(Running::Process { pid, checked: false }) => format!(" A process ({pid}) that was recorded as Endeavor's is alive there, but Endeavor couldn't check what it is; {attach}"),
            Some(Running::Job { id, listed: true }) => format!(" A Slurm job ({id}) for Endeavor is pending or running there; {attach}"),
            Some(Running::Job { id, listed: false }) => format!(" A Slurm job ({id}) is recorded there for Endeavor, but Endeavor couldn't ask Slurm whether it still exists; {attach}"),
            None => " Endeavor isn't running there.".to_owned(),
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
    format!("{text} {ASK_IN_CHAT}{again}{CALL_END_IN_CHAT}")
}

fn install_json(info: &InstallInfo) -> Value {
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
    out
}

fn needs_install_result(name: &str, info: &InstallInfo, tool: &str) -> Value {
    let mut result = json!({
        "machine": name,
        "state": "needs_install",
        "ready": false,
        "needs_install": true,
        "install": install_json(info),
        "message": format!("{} Nothing was installed on {name}.", install_text(name, info, tool)),
    });
    if tool == "stop_machine" {
        result["stopped"] = false.into();
    }
    result
}

fn unsaved_note(updating: bool) -> &'static str {
    if updating { "The machine stays as it was; the new settings are saved once they have connected." } else { "The machine is saved when it has connected, and not before." }
}

fn job_json(status: &Status) -> Option<Value> {
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

fn queue_json(status: &Status) -> Option<Value> {
    let queue = status.state.queue()?;
    if queue.state == "RUNNING" {
        return Some(json!({ "state": queue.state, "node": queue.reason, "reason_text": format!("running on node {}, and Endeavor is starting there", queue.reason) }));
    }
    Some(json!({ "state": queue.state, "reason": queue.reason, "reason_text": queue_reason_text(&queue.reason) }))
}

fn cluster_clause(cluster: bool) -> &'static str {
    if cluster { " It runs in a Slurm job, so stopping it also gives up the job: the next start waits in the queue again. Tell the user that too." } else { "" }
}

fn status_result(name: &str, reached: &Reached, message: &str) -> Value {
    let Reached { outcome, status } = reached;
    let mut out = json!({ "machine": name, "state": state_word(&status.state), "ready": false, "message": message });
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            out[key] = value;
        }
    };
    put("step", status.step.clone().map(Into::into));
    put("error", status.state.error().map(str::to_owned).or_else(|| if let Outcome::Failed(why) = outcome { Some(why.clone()) } else { None }).map(Into::into));
    put("queue", queue_json(status));
    put("job", job_json(status));
    put("install", if let Outcome::NeedsInstall(info) = outcome { Some(install_json(info)) } else { None });
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

fn default_name(host: &str) -> String {
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let name: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    name.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_owned()
}

fn new_id(name: &str, servers: &[Server]) -> Result<String, String> {
    let base = name.to_ascii_lowercase();
    let mut id = base.clone();
    for n in 2.. {
        if !servers.iter().any(|s| s.id == id) {
            break;
        }
        id = format!("{base}-{n}");
    }
    crate::client::valid_id(&id).map_err(invalid)?;
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

#[derive(Default, Debug, PartialEq)]
struct Given {
    partition: Option<String>,
    cpus: Option<u32>,
    memory_gb: Option<u32>,
    minutes: Option<u32>,
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
    pub(super) fn update_target(&self, id: &str, change: impl FnOnce(&mut Target)) -> bool {
        let mut target = self.target.lock().unwrap();
        let on = target.id == id;
        if on {
            change(&mut target);
        }
        on
    }

    pub(super) fn held(&self, target: &Target) -> Option<Arc<dyn Provider>> {
        if target.is_local() { Some(self.local.clone()) } else { self.connections.get(&target.id).map(|session| session as Arc<dyn Provider>) }
    }

    fn provider(&self, target: &Target) -> Result<Arc<dyn Provider>, String> {
        match self.held(target) {
            Some(provider) => Ok(provider),
            None => self.connections.open(&self.listed(target)?, false).map(|session| session as Arc<dyn Provider>),
        }
    }

    fn point_at(&self, next: Target) {
        let mut target = self.target.lock().unwrap();
        *target = Target { moves: target.moves + 1, ..next };
    }

    fn remembered(&self) -> Result<Option<Remembered>, String> {
        match &self.options.folder {
            Some(folder) => self.projects.get(folder),
            None => Ok(None),
        }
    }

    fn remember(&self, what: Option<Remembered>) -> Result<(), String> {
        match &self.options.folder {
            Some(folder) => self.projects.set(folder, what),
            None => Ok(()),
        }
    }

    pub(super) fn target_from_project(&self) {
        let remembered = match self.remembered() {
            Ok(remembered) => remembered,
            Err(e) => {
                eprintln!("endeavor: {e}");
                None
            }
        };
        let Some(remembered) = remembered else { return };
        match self.machines.find_by_id(&remembered.machine) {
            Ok(Some(server)) => {
                eprintln!("endeavor: this project uses the machine {}", server.display_name());
                *self.target.lock().unwrap() = Target::new(&server, remembered.folder);
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

    pub(super) fn route(&self, need: Need, deadline: Deadline) -> Result<Route, NotReady> {
        let target = self.current();
        let name = target.name.clone();
        if !target.active {
            return Err(self.stopped(&target));
        }
        let provider = self.provider(&target).map_err(NotReady::plain)?;
        let wait = if need != Need::Peek {
            deadline.left()
        } else if matches!(provider.status().state, State::Connecting | State::Connected | State::NothingRunning) {
            deadline.left().min(CONNECT_WAIT)
        } else {
            Duration::ZERO
        };
        let mut outcome = provider.ensure(Want::Attach { install: false }, wait, need == Need::Start && target.failed);
        if need == Need::Start && matches!(outcome, Outcome::NothingRunning) && !provider.cluster() {
            let issued = {
                let now = self.target.lock().unwrap();
                if let Some(unready) = self.moved(&now, &target, &provider) {
                    return Err(unready);
                }
                provider.ensure(Want::Start { job: None, install: false }, Duration::ZERO, false)
            };
            outcome = if matches!(issued, Outcome::StillWorking(_)) { provider.ensure(Want::Start { job: None, install: false }, deadline.left(), false) } else { issued };
        }
        if let Some(unready) = self.moved(&self.target.lock().unwrap(), &target, &provider) {
            return Err(unready);
        }
        if need == Need::Start {
            self.update_target(&target.id, |t| t.failed = matches!(outcome, Outcome::Failed(_)));
        }
        let status = provider.status();
        let outcome = if matches!(outcome, Outcome::NothingRunning) && matches!(status.state, State::Starting { .. }) { Outcome::StillWorking(String::new()) } else { outcome };
        if let Outcome::Ready(runtime) = &outcome {
            if target.is_local() {
                match self.local.other_build(runtime, need == Need::Start) {
                    OtherBuild::Stopped => return self.route(need, deadline),
                    OtherBuild::Kept(notice) => self.add_notice(notice),
                    OtherBuild::Fine => {}
                }
                if let Some(notice) = self.local.network_differs(runtime) {
                    self.add_notice(notice);
                }
            } else if let Some(notice) = self.machine_other_build(&target, runtime, provider.cluster()) {
                self.add_notice(notice);
            }
        }
        match outcome {
            Outcome::Ready(runtime) => Ok(self.ready(&target, &runtime, status.hello.as_ref().map(|h| h.home.as_str()))),
            Outcome::NothingRunning if provider.cluster() => Err(NotReady::of(&name, Outcome::NothingRunning, status, self.needs_job_message(&target))),
            Outcome::NothingRunning => {
                let message = format!("Endeavor on {} isn't running. It starts at the first notebook tool call, which then takes a few minutes the first time.", place(&name));
                Err(NotReady { idle: true, ..NotReady::of(&name, Outcome::NothingRunning, status, message) })
            }
            other => {
                let reached = Reached { outcome: other, status };
                let message = not_ready_message(&name, &reached);
                Err(NotReady { name, message, reached: Some(Box::new(reached)), idle: false, stopped: false })
            }
        }
    }

    fn machine_other_build(&self, target: &Target, runtime: &RuntimeInfo, cluster: bool) -> Option<String> {
        if !runtime.reattached || runtime.usable_as_is() || !self.told_other_build.lock().unwrap().insert((target.id.clone(), runtime.pid)) {
            return None;
        }
        Some(format!("{}{}", super::target::other_build_notice(&target.name, &target.id, runtime.interface, None), cluster_clause(cluster)))
    }

    fn add_notice(&self, notice: String) {
        let mut owed = self.notice.lock().unwrap();
        *owed = Some(match owed.take() {
            Some(earlier) => format!("{earlier} {notice}"),
            None => notice,
        });
    }

    fn moved(&self, now: &Target, target: &Target, provider: &Arc<dyn Provider>) -> Option<NotReady> {
        if now.id != target.id || now.moves != target.moves {
            return Some(NotReady::plain("This session moved to another machine while the call waited. Try the call again."));
        }
        if !self.held(now).is_some_and(|now| std::ptr::addr_eq(Arc::as_ptr(&now), Arc::as_ptr(provider))) {
            return Some(NotReady::plain(format!("Endeavor's connection to {} was replaced while the call waited. Try the call again.", now.name)));
        }
        (!now.active).then(|| self.stopped(now))
    }

    fn stopped(&self, target: &Target) -> NotReady {
        let name = &target.name;
        let message = format!("Endeavor on {} was stopped from this session with stop_machine. Call `use_machine` with machine \"{name}\" to start it again.", place(name));
        let unready = match self.held(target) {
            Some(provider) => NotReady::of(name, Outcome::NothingRunning, provider.status(), message),
            None => NotReady::plain(message),
        };
        NotReady { stopped: true, ..unready }
    }

    fn listed(&self, target: &Target) -> Result<Server, String> {
        self.machines.find_by_id(&target.id)?.ok_or_else(|| format!("{} isn't in the list of machines ({}) any more.", target.name, self.machines.path().display()))
    }

    fn ready(&self, target: &Target, runtime: &RuntimeInfo, home: Option<&str>) -> Route {
        if target.told != Some(runtime.pid) {
            let folder = match self.local_folder(target) {
                Some(Folder::In(folder)) => Some(Some(folder)),
                Some(_) => Some(None),
                None => target.folder.clone().or_else(|| home.filter(|h| !h.is_empty()).map(str::to_owned)).map(Some),
            };
            if folder.is_none_or(|folder| self.tell_session_folder(runtime.port, &runtime.token, folder.as_deref())) {
                self.update_target(&target.id, |t| t.told = Some(runtime.pid));
            }
        }
        Route { port: runtime.port, token: runtime.token.clone(), page_token: runtime.page_token.clone(), host: target.host() }
    }

    fn needs_job_message(&self, target: &Target) -> String {
        let defaults = self.machines.find_by_id(&target.id).ok().flatten().and_then(|s| s.cluster).map(|c| {
            let partition = c.resources.partition.as_deref().map_or("the cluster's default partition".to_owned(), |p| format!("partition {p}"));
            format!("{} on {partition}", c.resources.summary())
        });
        let name = &target.name;
        let defaults = defaults.map(|d| format!(" The defaults would be {d}.")).unwrap_or_default();
        format!(
            "This project uses {name}, a Slurm cluster, and no job is running there. Starting Endeavor there means submitting a job that waits in the queue and uses the user's allocation, so nothing was submitted.{defaults} Ask the user to confirm those resources or choose others, then call `use_machine` with machine \"{name}\" and the resources to submit it."
        )
    }

    pub(super) fn unready(&self, message: &Value, tool: Option<&str>, unready: NotReady) {
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else { return };
        let answer = |result: Value| to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": text_result(&to_json(&result)) }));
        let reply = match (tool, &unready.reached) {
            (Some("list_notebooks"), _) if unready.idle => answer(json!([])),
            (Some("session_status"), _) if unready.idle => answer(json!({ "pluto": "not running", "notebooks": [], "message": unready.message })),
            (Some("session_status"), Some(reached)) => {
                let mut result = status_result(&unready.name, reached, &unready.message);
                if unready.stopped {
                    result["state"] = "stopped".into();
                }
                answer(result)
            }
            (_, Some(reached)) if message["method"] == "tools/call" && matches!(reached.outcome, Outcome::StillWorking(_) | Outcome::Queued { .. }) => {
                to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": text_result(&unready.message) }))
            }
            _ => tool_failure(id, message, &unready.message),
        };
        self.write(&self.decorate(message, tool, reply));
    }

    pub(super) fn decorate(&self, message: &Value, tool: Option<&str>, reply: String) -> String {
        if tool.is_some_and(crate::views::is_view_tool) {
            return reply;
        }
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        if parsed["id"] != message["id"] || (parsed.get("result").is_none() && parsed.get("error").is_none()) {
            return reply;
        }
        let mut changed = false;
        if tool == Some("session_status") && parsed["result"]["isError"] == false {
            changed |= self.add_machine_fields(&mut parsed);
        }
        if let Some(tool) = tool
            && crate::mcp::refuses_r_for_want_of_it(tool, &message["params"]["arguments"], &parsed["result"])
            && let Some(text) = self.no_r_text()
        {
            parsed["result"] = tool_error(&text, false);
            changed = true;
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

    fn no_r_text(&self) -> Option<String> {
        let machine = self.current();
        let status = if machine.is_local() { Some(self.local.status()) } else { self.connections.status(&machine.id) };
        if status.as_ref().and_then(|status| status.state.runtime()).is_some_and(|runtime| runtime.usable_as_is()) {
            return None;
        }
        let place = if machine.is_local() { "this computer" } else { machine.name.as_str() };
        let cluster = !machine.is_local() && self.connections.get(&machine.id).is_some_and(|session| session.cluster());
        let id = &machine.id;
        let restart = format!("Don't stop it yourself: tell the user, and ask whether to restart it, which closes the notebooks open in it. Only if the user says yes, call `stop_machine` with machine \"{id}\", then `use_machine` with machine \"{id}\".{}", cluster_clause(cluster));
        Some(crate::no_r_text(place, &restart))
    }

    fn add_machine_fields(&self, reply: &mut Value) -> bool {
        let machine = self.current();
        let status = if machine.is_local() { Some(self.local.status()) } else { self.connections.status(&machine.id) };
        let other = status.as_ref().and_then(|status| status.state.runtime()).filter(|runtime| !runtime.usable_as_is() && (!machine.is_local() || self.local.kept(runtime.pid)));
        if machine.is_local() && other.is_none() {
            return false;
        }
        let Some(Value::Object(mut fields)) = reply["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()) else { return false };
        if let Some(runtime) = other {
            let place = if machine.is_local() { "this computer" } else { machine.name.as_str() };
            let cluster = !machine.is_local() && self.connections.get(&machine.id).is_some_and(|session| session.cluster());
            fields.insert("other_version".into(), format!("{}{}", crate::other_version_text(place, &machine.id, runtime.interface, ""), cluster_clause(cluster)).into());
            fields.insert("runtime_build".into(), runtime.build.clone().map_or(Value::Null, Value::from));
        }
        if machine.is_local() {
            reply["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
            return true;
        }
        fields.insert("machine".into(), machine.name.clone().into());
        if let Some(status) = status {
            if let Some(job) = job_json(&status) {
                fields.insert("job".into(), job);
            }
            if let Some(port) = status.state.runtime().and_then(|r| r.remote_port) {
                fields.insert("remote_port".into(), port.into());
            }
        }
        reply["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        true
    }

    pub(super) fn machine_tool(self: &Arc<Self>, message: &Value, tool: &str, deadline: Deadline) {
        if message.get("id").is_none_or(Value::is_null) {
            return;
        }
        let arguments = match crate::mcp::checked_call(&message["params"], false, true) {
            Ok((_, arguments)) => arguments,
            Err(result) => return self.answer_call(message, Some(tool), result),
        };
        let dialog = self.consent.can_ask();
        let result = if tool != "list_machines" {
            match self.install_asked(message, tool, &arguments, deadline) {
                Some(result) => result,
                None => return,
            }
        } else {
            self.run_machine_tool(tool, &arguments, deadline)
        };
        let dialog = dialog && self.consent.can_ask();
        let page = !dialog && self.page_shows();
        let result = match result {
            Ok(mut result) => {
                if let Some(text) = result["message"].as_str() {
                    result["message"] = if dialog {
                        asked_in_dialog(text)
                    } else if page {
                        asked_on_page(text)
                    } else {
                        cant_ask(text)
                    }
                    .into();
                }
                text_result(&to_json(&result))
            }
            Err(e) => tool_error(&e, false),
        };
        self.answer_call(message, Some(tool), result);
    }

    fn run_machine_tool(self: &Arc<Self>, tool: &str, arguments: &Value, deadline: Deadline) -> Result<Value, String> {
        match tool {
            "list_machines" => self.list_machines(),
            "add_machine" => self.add_machine(arguments, deadline),
            "use_machine" => self.use_machine(arguments, deadline),
            _ => self.stop_machine(arguments, deadline),
        }
    }

    pub(super) fn page_shows(&self) -> bool {
        test_var("ENDEAVOR_TEST_INSTALL_PAGES").is_some_and(|file| !file.is_empty()) || super::browser::shows(|name| std::env::var(name).ok())
    }

    fn install_asked(self: &Arc<Self>, message: &Value, tool: &str, arguments: &Value, deadline: Deadline) -> Option<Result<Value, String>> {
        let began = Instant::now();
        let wants_install = match flag_arg(arguments, "install") {
            Ok(install) => install,
            Err(e) => return Some(Err(e)),
        };
        self.consent.looking(message);
        let mut look = arguments.clone();
        if let Some(fields) = look.as_object_mut() {
            fields.remove("install");
        }
        let looked = self.run_machine_tool(tool, &look, deadline);
        let machine = arguments["machine"].as_str().or(arguments["name"].as_str()).or(arguments["host"].as_str()).unwrap_or_default().to_owned();
        let question = match &looked {
            Ok(result) if result["state"] == "needs_install" => {
                let name = result["machine"].as_str().unwrap_or(&machine).to_owned();
                let items: Vec<wire::Item> = serde_json::from_value(result["install"]["items"].clone()).unwrap_or_default();
                Some(install_question(&name, &items, tool))
            }
            Ok(result) if tool == "use_machine" && wants_install && result["ready"] == true => Some(leave_question(&machine)),
            _ => None,
        };
        let question = question.filter(|_| self.consent.can_ask() || wants_install);
        let Some(question) = question else {
            return (!self.consent.not_asking(message)).then_some(looked);
        };
        let again = format!("`{tool}` again");
        let mut install = arguments.clone();
        install["install"] = true.into();
        let page = self.page_shows();
        let held = page && self.pages.held(&question);
        let mut dialog = self.consent.can_ask() && !held;
        let mut asked = if held {
            self.ask_on_page(message, &question, &again, Duration::ZERO)
        } else if dialog {
            self.consent.ask_install(message, &question, &again, page, &|line| self.write(line))
        } else {
            Err(super::consent::NO_DIALOG.to_owned())
        };
        if asked.as_ref().is_err_and(|e| e == super::consent::SILENT) {
            let wait = super::consent::ANSWER_WAIT.saturating_sub(began.elapsed());
            asked = self.ask_on_page(message, &question, &again, wait);
        }
        if asked.as_ref().is_err_and(|e| e == super::consent::NO_DIALOG) {
            dialog = false;
            if !wants_install {
                return (!self.consent.not_asking(message)).then_some(looked);
            }
            asked = if page {
                self.ask_on_page(message, &question, &again, page_wait().min(PAGE_CALL.saturating_sub(began.elapsed() + INSTALL_SHARE)))
            } else {
                self.consent.not_asking(message);
                Err(CANT_ASK_ERROR.to_owned())
            };
        }
        let install_wait = if dialog { super::start_wait() } else { PAGE_CALL.saturating_sub(began.elapsed()).max(MIN_INSTALL) };
        match asked {
            Ok(true) => Some(self.run_machine_tool(tool, &install, Deadline::after(install_wait))),
            Ok(false) => None,
            Err(refused) if tool == "use_machine" && looked.as_ref().is_ok_and(|r| r["ready"] == true) => {
                let mut result = looked.unwrap();
                let said = refused.split_once("::").map_or(refused.as_str(), |(_, said)| said);
                result["install_declined"] = true.into();
                result["message"] = format!("{} {said}", result["message"].as_str().unwrap_or_default()).into();
                Some(Ok(result))
            }
            Err(refused) => Some(Err(refused)),
        }
    }

    fn ask_on_page(&self, message: &Value, question: &str, again: &str, wait: Duration) -> Result<bool, String> {
        let again_after_no = self.pages.held(question);
        let clicked = self.pages.ask(question, true, wait, &|| self.consent.cancelled(message) || self.consent.dialog_answered(message), &open_page);
        if let Some(answer) = self.consent.install_answer_after_page(message, again, matches!(clicked, Clicked::Install | Clicked::Declined), &|line| self.write(line)) {
            self.pages.retire(question);
            self.consent.not_asking(message);
            return answer;
        }
        if self.consent.not_asking(message) || clicked == Clicked::Dropped {
            return Ok(false);
        }
        Err(match clicked {
            Clicked::Install => return Ok(true),
            Clicked::Dropped => return Ok(false),
            Clicked::Declined if again_after_no => format!("install_declined::{}{again} with `install: true` gets this same answer until those 10 minutes pass. {}", super::consent::HELD_NO, super::consent::CHANGED_MIND),
            Clicked::Declined => format!("install_declined::The user clicked Don't install on a web page (not a dialog) that Endeavor opened in their browser, so nothing was installed. Their no stands for 10 minutes: until then, {again} with `install: true` gets this same answer. Your reply is one plain sentence that nothing was installed, as they chose, and nothing more: don't say how to get it later (no tool names, no prompt, no page, no browser, no button), don't ask them to reconsider, and don't call again unless they ask for it themselves. After the 10 minutes, a new {again} call asks them again."),
            Clicked::NotYet => format!("install_not_answered::Endeavor opened a web page (not a dialog) in the user's browser that names what it would install, and it installs it only once they click Install there. They haven't answered yet, so nothing was installed. Tell the user that a web page is open in their browser and to answer it there (don't call it a dialog), and end your reply. Once they say they clicked Install, call {again} with `install: true`, and it installs without asking again: their click holds for 10 minutes."),
            Clicked::NoBrowser => CANT_ASK_ERROR.to_owned(),
        })
    }

    pub(super) fn answer_call(&self, message: &Value, tool: Option<&str>, result: Value) {
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else { return };
        self.write(&self.decorate(message, tool, to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))));
    }

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
        let target = self.current();
        let machines: Vec<Value> = servers
            .iter()
            .map(|server| {
                let status = self.connections.status(&server.id);
                let state = status.as_ref().map_or("not connected", |s| state_word(&s.state));
                let mut entry = json!({ "name": server.display_name(), "host": server.ssh_target(), "cluster": server.cluster.is_some(), "state": state, "this_session": target.id == server.id });
                if let Some(error) = status.as_ref().and_then(|s| s.state.error()) {
                    entry["error"] = error.into();
                }
                entry
            })
            .collect();
        let running = crate::runtime::look(&self.local.state_dir(), false, false).alive().is_some();
        let local_state = match (target.is_local() && !target.active, running) {
            (true, _) => "stopped from this session",
            (false, true) => "running",
            (false, false) => "not running",
        };
        let ssh_hosts: Vec<String> = ssh_config_hosts().into_iter().filter(|h| !servers.iter().any(|s| s.ssh_host == *h || s.name.eq_ignore_ascii_case(h))).collect();
        let used = &target.name;
        Ok(json!({
            "machines": machines,
            "local": { "name": LOCAL, "state": local_state, "this_session": target.is_local() },
            "this_session": { "machine": used },
            "ssh_hosts_not_added": ssh_hosts,
            "message": format!("This session works on {}. A machine shows a state only while this session is connected to it: \"not connected\" says nothing about whether Endeavor runs there, and `use_machine` finds out. `add_machine` adds a server from the ssh hosts listed; `use_machine` moves the session to a machine, or back to \"{LOCAL}\".", place(used)),
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
            Some(_) => return Err(invalid("slurm must be true (run notebooks in Slurm jobs) or false (run them directly on the machine)")),
        };
        let servers = self.machines.load_writable()?;
        let find_existing = |servers: &[Server]| {
            let by_name = |n: &str| servers.iter().find(|s| s.name.eq_ignore_ascii_case(n)).cloned();
            match &given_name {
                Some(name) => by_name(name),
                None => by_name(&name).or_else(|| servers.iter().find(|s| s.ssh_host == host && s.port == port).cloned()),
            }
        };
        let existing = find_existing(&servers);
        let updating = existing.is_some();
        let mut record = match &existing {
            Some(existing) => existing.clone(),
            None => Server { id: new_id(&name, &servers)?, name: name.clone(), ..Default::default() },
        };
        record.ssh_host = host.clone();
        record.port = port;
        if julia.is_some() {
            record.julia = julia;
        }
        let launcher = match (&existing, slurm) {
            (Some(_), _) => None,
            (None, Some(true)) => Some(Launcher::Slurm),
            (None, Some(false)) => Some(Launcher::Process),
            (None, None) => Some(Launcher::Auto),
        };
        let (session, mut unsaved) = self.connections.trying(&record, install, launcher)?;
        let outcome = session.ensure(Want::Attach { install }, deadline.left(), true);
        match &outcome {
            Outcome::NeedsInstall(info) => {
                let mut result = needs_install_result(&record.name, info, "add_machine");
                result["host"] = record.ssh_target().into();
                result["saved"] = updating.into();
                result["message"] = format!("{} {}", result["message"].as_str().unwrap_or_default(), unsaved_note(updating)).into();
                return Ok(result);
            }
            Outcome::Failed(why) => {
                return Err(format!(
                    "Couldn't connect to {}: {}\nNothing was saved. This is for the user to fix in a terminal (never ask them for a password or passphrase here, and don't run ssh yourself), then call `add_machine` again.",
                    record.ssh_target(),
                    if why.is_empty() { "no reason was given" } else { why }
                ));
            }
            Outcome::StillWorking(step) => {
                let result = json!({
                    "machine": record.name, "host": record.ssh_target(), "state": "connecting", "saved": updating,
                    "step": step,
                    "message": format!("Still connecting to {}. Call `add_machine` again with the same host to continue. {}", record.ssh_target(), unsaved_note(updating)),
                });
                unsaved.leave = true;
                return Ok(result);
            }
            Outcome::Ready(_) | Outcome::Queued { .. } | Outcome::NothingRunning => {}
        }
        let mut status = session.status();
        let connected_as_cluster = session.cluster();
        let found = status.hello.as_ref().is_some_and(|h| h.slurm);
        let cluster = match choose_mode(slurm, existing.as_ref(), found) {
            Ok(cluster) => cluster,
            Err(why) => return Err(format!("{why}\nNothing was saved.")),
        };
        if existing.as_ref().is_some_and(|p| p.cluster.is_some() != cluster) && (status.job.is_some() || matches!(status.state, State::Starting { .. } | State::Queued(_) | State::Ready(_))) {
            return Err(format!(
                "Endeavor is running, or starting, on {} through the way it is saved now. Changing between running in Slurm jobs and running it directly would leave that one where `stop_machine` can't reach it. Nothing was changed. Call `stop_machine` first (with the user's agreement), then call `add_machine` again.",
                record.display_name()
            ));
        }
        let listed = cluster || slurm != Some(false);
        if listed {
            status = session.wait_for(deadline.left(), |s| s.hello.as_ref().is_none_or(|h| !h.slurm || h.partitions.is_some()));
        }
        let hello = status.hello.clone().unwrap_or_default();
        if listed && hello.slurm && hello.partitions.is_none() && record.cluster.as_ref().is_none_or(|c| c.partitions.is_empty()) {
            let result = json!({
                "machine": record.name, "host": record.ssh_target(), "state": "connecting", "saved": updating,
                "step": status.step,
                "message": format!("Connected to {}, but Slurm hasn't listed its partitions yet. Call `add_machine` again with the same host to continue. {}", record.ssh_target(), unsaved_note(updating)),
            });
            unsaved.leave = true;
            return Ok(result);
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
        self.machines.save_expecting(record.clone(), existing.as_ref().map(|e| e.id.as_str()), &|servers| find_existing(servers).map(|e| e.id))?;
        if record.cluster.is_some() != connected_as_cluster {
            self.connections.forget(&record.id);
        } else {
            self.connections.save(&record);
        }
        let partitions: Vec<Partition> = record.cluster.as_ref().map(|c| c.partitions.clone()).or_else(|| hello.partitions.clone()).unwrap_or_default();
        let mut message = format!("Connected to {} (node {}, home folder {}). ", record.ssh_target(), hello.node, hello.home);
        if let Some(saved) = &record.cluster {
            message.push_str("It has Slurm, and notebooks run in Slurm jobs there");
            if partitions.is_empty() {
                message.push_str(if hello.partitions_failed { ", but its partitions couldn't be read. " } else { ", but it didn't list its partitions. " });
            } else {
                message.push_str(&format!(". Partitions: {}. ", partitions.iter().map(partition_text).collect::<Vec<_>>().join("; ")));
            }
            message.push_str(&format!("Default job: {}. ", saved.resources.summary()));
            message.push_str("To run notebooks directly on the machine instead, call `add_machine` again with slurm false (the saved job defaults are then dropped), but only if the machine is the user's own workstation or the user confirms it isn't a shared cluster: on a cluster that runs them on the login node, which other people share. ");
        } else if hello.slurm {
            let why = if slurm == Some(false) { "as asked" } else { "since it was saved before as a plain server" };
            message.push_str(&format!("It has Slurm, but notebooks run on it directly and not in a job, {why}. To run them in Slurm jobs instead, call `add_machine` again with slurm true. "));
        } else {
            message.push_str("It has no Slurm, so notebooks run there directly. ");
        }
        if hello.found.is_empty() {
            message.push_str("Endeavor looks for Julia there when a Julia notebook first needs it; R notebooks don't need it. ");
        }
        for found in &hello.found {
            message.push_str(&format!("{} {} is at {}. ", found.name, found.version, found.path));
        }
        message.push_str(&format!("The machine is saved as \"{}\". ", record.name));
        message.push_str(if record.cluster.is_some() && slurm != Some(true) {
            "Unless the user already said notebooks should run in Slurm jobs here, tell them it will and check they agree before calling `use_machine`."
        } else {
            "Call `use_machine` to work on it."
        });
        Ok(json!({
            "machine": record.name,
            "host": record.ssh_target(),
            "state": "connected",
            "saved": true,
            "updated": updating,
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

    fn find_machine(&self, key: &str) -> Result<Server, String> {
        if key.eq_ignore_ascii_case(LOCAL) {
            return Ok(local_server());
        }
        match self.machines.find(key)? {
            Some(server) => Ok(server),
            None => {
                let names: Vec<String> = self.machines.load()?.iter().map(Server::display_name).collect();
                let known = if names.is_empty() { "No machine is added yet; `add_machine` adds one.".to_owned() } else { format!("Machines: {}.", names.join(", ")) };
                Err(format!("ArgumentError: machine_not_found::There is no machine \"{key}\". {known} \"{LOCAL}\" is this computer."))
            }
        }
    }

    fn use_machine(self: &Arc<Self>, args: &Value, deadline: Deadline) -> Result<Value, String> {
        let _one = self.lock_ops(deadline)?;
        self.connections.drop_unsaved(None);
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        let server = self.find_machine(&key)?;
        let local = server.id == LOCAL;
        let install = flag_arg(args, "install")?;
        let given = Given::parse(args)?;
        let given_folder = if local { None } else { text_arg(args, "folder")? };
        let from_memory = !local && given_folder.is_none();
        let folder = if local {
            self.options.folder.as_ref().map(|folder| folder.display().to_string())
        } else {
            match given_folder {
                Some(folder) => Some(folder),
                None => self.remembered().ok().flatten().filter(|remembered| remembered.machine == server.id).and_then(|remembered| remembered.folder),
            }
        };
        if given.any() && server.cluster.is_none() {
            return Err(invalid(format!("{} isn't a Slurm cluster, so partition, cpus, memory_gb, hours, gpus, account and extra_sbatch_flags don't apply to it. Leave them out.", if local { "This computer".to_owned() } else { server.display_name() })));
        }
        let planned = match &server.cluster {
            Some(cluster) if given.any() => Some(given.over(cluster)?),
            _ => None,
        };
        let name = server.display_name();
        let provider = self.provider_for(&server, install, false)?;
        let was_ready = matches!(provider.status().state, State::Ready(_));
        let mut notes: Vec<String> = Vec::new();
        if let Some(path) = folder.as_ref().filter(|_| !local) {
            let which = if from_memory { format!("The folder this project used on {name}, {path},") } else { format!("The folder \"{path}\"") };
            let ask = if from_memory { "Ask the user which folder to use and pass it as `folder` (`\"~\"` is the home folder)." } else { "Ask the user which folder to use, or leave `folder` out." };
            match provider.files(wire::files::Request::Folder { path: path.clone() }, deadline.left()) {
                Some(Ok(wire::files::Reply::Folder { path, created: true })) => {
                    let gone = if from_memory { " (the folder this project used there was gone)" } else { "" };
                    notes.push(format!("Made a new folder for the session on {name}: {path}{gone}. Tell the user it was made."));
                }
                Some(Ok(wire::files::Reply::Folder { .. })) => {}
                Some(Ok(wire::files::Reply::NoFolder { parent, .. })) => {
                    let why = if from_memory { "It may have been removed since.".to_owned() } else { format!("The path may be mistyped, or be a path on another computer: paths here are {name}'s.") };
                    return Err(invalid(format!("{which} can't be the session's folder: neither it nor the folder it would go in, {}, exists on {name}. Nothing was started. {why} {ask}", parent)));
                }
                Some(Ok(other)) => return Err(format!("{name}'s helper answered the folder check with {other:?}.")),
                Some(Err(message)) => return Err(invalid(format!("{which} can't be the session's folder on {name}: {message} Nothing was started. {ask}"))),
                None => notes.push(format!("The folder {path} wasn't checked, because {name} wasn't connected yet; the next `use_machine` checks it.")),
            }
        }
        let mut saved_resources = None;
        let outcome = match &server.cluster {
            None => provider.ensure(Want::Start { job: None, install }, deadline.left(), true),
            Some(cluster) => {
                let mut outcome = provider.ensure(Want::Attach { install }, deadline.left(), true);
                if matches!(outcome, Outcome::NothingRunning) {
                    let Some((resources, account)) = planned else { return Ok(self.needs_job(&name, cluster)) };
                    let mut job = cluster.job(&resources);
                    job.account = account.clone();
                    saved_resources = Some((resources, account));
                    outcome = provider.ensure(Want::Start { job: Some(job), install }, deadline.left(), true);
                } else if given.any() && matches!(outcome, Outcome::Ready(_) | Outcome::Queued { .. }) {
                    notes.push("A job is already queued or running there, so the resources you gave were not used.".into());
                }
                outcome
            }
        };
        let reached = Reached { outcome, status: provider.status() };
        if install && let (Outcome::Ready(_), Some(runtime)) = (&reached.outcome, reached.status.state.runtime()) {
            if server.cluster.is_some() {
                if let Some(note) = login_julia(&*provider, &name, runtime, &deadline) {
                    notes.push(note);
                }
            } else {
                let (call, what) = if local { ("endeavor/allow_r_install", "install R") } else { ("endeavor/allow_julia_install", "download Julia") };
                if let Err(e) = crate::bridge_rpc(runtime.port, &runtime.token, call, json!({})) {
                    notes.push(format!("Couldn't pass the user's leave to {what} on to {name} ({e}); calling use_machine again with `install: true` tries again."));
                }
            }
        }
        match &reached.outcome {
            Outcome::NeedsInstall(info) => return Ok(needs_install_result(&name, info, "use_machine")),
            Outcome::Failed(_) => return Err(format!("{}{}", not_ready_message(&name, &reached), notes.iter().map(|n| format!(" {n}")).collect::<String>())),
            _ => {}
        }
        self.point_at(Target::new(&server, folder.clone()));
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
        let remembered = (!local).then(|| Remembered { machine: server.id.clone(), folder });
        if let Err(e) = self.remember(remembered) {
            notes.push(format!("The project's remembered machine wasn't updated: {e}"));
        }
        self.use_result(&server, reached, was_ready, notes)
    }

    fn provider_for(&self, server: &Server, install: bool, held: bool) -> Result<Arc<dyn Provider>, String> {
        if server.id == LOCAL {
            return Ok(self.local.clone());
        }
        match self.connections.get(&server.id).filter(|_| held) {
            Some(session) => Ok(session),
            None => self.connections.open(server, install),
        }
        .map(|session| session as Arc<dyn Provider>)
    }

    fn save_defaults(&self, saved: &Server) -> Result<(), String> {
        let Some(mut current) = self.machines.find_by_id(&saved.id)? else { return Ok(()) };
        if let (Some(now), Some(new)) = (current.cluster.as_mut(), &saved.cluster) {
            now.resources = new.resources.clone();
            now.account = new.account.clone();
        }
        self.machines.save(current)
    }

    fn use_result(&self, server: &Server, reached: Reached, was_ready: bool, notes: Vec<String>) -> Result<Value, String> {
        let name = server.display_name();
        let notes = if notes.is_empty() { String::new() } else { format!(" {}", notes.join(" ")) };
        let Outcome::Ready(runtime) = &reached.outcome else {
            let mut result = status_result(&name, &reached, &format!("{}{notes}", not_ready_message(&name, &reached)));
            result["this_session"] = true.into();
            return Ok(result);
        };
        let target = self.current();
        if target.id != server.id {
            return Err("The session moved to another machine.".into());
        }
        let home = reached.status.hello.as_ref().map(|h| h.home.clone());
        let route = self.ready(&target, runtime, home.as_deref());
        let mut result = json!({
            "machine": name,
            "state": "ready",
            "ready": true,
            "browser_url": browser_link(route.port, "/"),
            "folder": target.folder.clone().or(home),
        });
        if target.is_local() {
            result["message"] = format!("This session works on this computer again. The host tools (`list_folder`, `read_file`, `run_shell`) don't apply here: use your own file and shell tools. The session has no notebook here yet, unless it is still in one it made here that is open (`list_notebooks` shows `this_session`); `new_notebook` or `open_notebook` makes one.{notes}").into();
            return Ok(result);
        }
        result["node"] = runtime.node.clone().into();
        result["remote_port"] = runtime.remote_port.into();
        result["already_running"] = (runtime.reattached || was_ready).into();
        if let Some(job) = job_json(&reached.status) {
            result["job"] = job;
        }
        let on = if runtime.reattached || was_ready { "A runtime was already running there, and this session uses it" } else { "Endeavor started there" };
        let julia = match test_var("ENDEAVOR_TEST_JULIA_AT_START") {
            None => " Julia starts there when a Julia notebook needs it, or may be starting already; the first start can take a few minutes, and calls say `julia_starting` until it's ready. R notebooks don't need Julia.",
            Some(_) => "",
        };
        let notes = match self.machine_other_build(&target, runtime, server.cluster.is_some()) {
            Some(other) => format!("{notes} {other}"),
            None => notes,
        };
        let ends = job_json(&reached.status).and_then(|j| j["ends_in_minutes"].as_u64()).map(|m| format!(" The job ends in {}.", wire::slurm::duration_text(m as u32))).unwrap_or_default();
        result["message"] = format!(
            "{on} (node {}).{julia} When you make or open a notebook there, it usually opens in the user's browser (`opened_in_browser` says); give the user its `browser_url` too.{ends}{} This session has no notebook on {name} yet, unless it is still in one it made there that is open (`list_notebooks` shows `this_session`): create one with `new_notebook` or open one with `open_notebook`; paths and files are {name}'s.{notes}",
            runtime.node,
            reach_text(server, runtime)
        )
        .into();
        Ok(result)
    }

    fn stop_machine(self: &Arc<Self>, args: &Value, deadline: Deadline) -> Result<Value, String> {
        let _one = self.lock_ops(deadline)?;
        self.connections.drop_unsaved(None);
        let key = text_arg(args, "machine")?.ok_or_else(|| invalid("machine is required: a name from list_machines, or \"local\""))?;
        let force = match args.get("force") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(force)) => *force,
            Some(_) => return Err(invalid("force must be true or false")),
        };
        let install = flag_arg(args, "install")?;
        let server = self.find_machine(&key)?;
        let name = server.display_name();
        let at = place(&name);
        let provider = self.provider_for(&server, install, true)?;
        if !matches!(provider.status().state, State::Ready(_) | State::Starting { .. } | State::Queued(_)) {
            let starting_here = server.id == LOCAL && crate::runtime::lock_state(&self.local.state_dir()).is_held();
            let wait = if starting_here { Duration::ZERO } else { deadline.left() };
            match provider.ensure(Want::Attach { install }, wait, true) {
                Outcome::NeedsInstall(info) => return Ok(needs_install_result(&name, &info, "stop_machine")),
                Outcome::NothingRunning => return Ok(json!({ "machine": name, "stopped": false, "message": format!("Endeavor isn't running on {at}, so there is nothing to stop.") })),
                _ => {}
            }
        }
        let status = provider.status();
        if !force && matches!(status.state, State::Starting { .. } | State::Queued(_)) {
            return Ok(waiting_result(&name, &status));
        }
        if !force && let Some(runtime) = status.state.runtime() {
            let others = self.recent_others(runtime.port, &runtime.token, deadline.left().min(CHECK_WAIT))?;
            if others.count > 0 {
                return Ok(others_result(&name, &others));
            }
        }
        let mut was_active = false;
        self.update_target(&server.id, |t| {
            was_active = std::mem::replace(&mut t.active, false);
            t.told = None;
        });
        let (stopping, relay, id) = (provider.clone(), self.clone(), server.id.clone());
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || {
            let stopped = stopping.stop(force);
            if stopped.is_err() && was_active {
                relay.update_target(&id, |t| t.active = true);
            }
            drop(done.send(stopped));
        });
        match waited.recv_timeout(deadline.left()) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return Err(why),
            Err(_) => return Ok(json!({ "machine": name, "stopped": false, "message": format!("Stopping Endeavor on {at} is taking a while. It goes on in the background: call `session_status` or `list_machines` later to see whether it ended.") })),
        }
        let cluster = if server.cluster.is_some() { " and its Slurm job was cancelled" } else { "" };
        Ok(json!({
            "machine": name,
            "stopped": true,
            "message": format!("Endeavor on {at} was stopped{cluster}. Every notebook running there ended. `use_machine` with machine \"{name}\" starts it again."),
        }))
    }

    fn recent_others(&self, port: u16, token: &str, quiet: Duration) -> Result<Others, String> {
        let unknown = |why: String| format!("Nothing was stopped: couldn't check who else is active there ({why}). Stopping ends everyone's notebooks there, so call `stop_machine` again with `force: true` only if the user agrees to stop it anyway.");
        if quiet.is_zero() {
            return Err(unknown("no time was left to ask".into()));
        }
        let params = json!({ "owner": self.session, "within_seconds": RECENT_SECONDS });
        let (status, body) = Relay::tell(port, token, "endeavor/recent_sessions", params, Some(Instant::now() + quiet)).map_err(|e| {
            unknown(match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "the runtime didn't answer in time".to_owned(),
                _ => e.to_string(),
            })
        })?;
        let reply: Value = serde_json::from_slice(&body).map_err(|e| unknown(format!("{status}: {e}")))?;
        let malformed = || unknown(format!("{status}: {}", reply["error"]["message"].as_str().unwrap_or("no usable answer")));
        let found = reply.get("result").filter(|_| status == 200).ok_or_else(malformed)?;
        let count = found["count"].as_u64().ok_or_else(malformed)?;
        let seconds_ago = if count == 0 { 0 } else { found["active_seconds_ago"].as_u64().ok_or_else(malformed)? };
        Ok(Others { count, seconds_ago })
    }

    fn needs_job(&self, name: &str, cluster: &Cluster) -> Value {
        let defaults = resources_json(&cluster.resources, cluster.account.as_deref());
        let partitions: Vec<Value> = cluster.partitions.iter().map(partition_json).collect();
        let partition = cluster.resources.partition.as_deref().map_or("the cluster's default partition".to_owned(), |p| format!("partition {p}"));
        let target = self.current();
        let stays = if target.is_local() { "this computer" } else { &target.name };
        json!({
            "machine": name,
            "state": "needs_job",
            "ready": false,
            "needs_job": true,
            "defaults": defaults,
            "partitions": partitions,
            "message": format!(
                "No job is running on {name}, and starting Endeavor there means submitting a Slurm job that waits in the queue and uses the user's allocation, so nothing was submitted and this session has not moved: it stays on {stays} until `use_machine` is called with machine \"{name}\" and the resources. The saved defaults are {} on {partition}. Ask the user to confirm them or choose others, then call `use_machine` again with machine \"{name}\" and the resources to submit it.",
                cluster.resources.summary()
            ),
        })
    }
}

fn reach_text(server: &Server, runtime: &RuntimeInfo) -> String {
    let Some(port) = runtime.remote_port else { return " The page works while this session is connected.".into() };
    if server.cluster.is_some() {
        return format!(" The page works while this session is connected. The runtime is on node {}, port {port}, behind the login node, so there is no ssh command for it between sessions.", runtime.node);
    }
    let via = server.port.map(|p| format!(" -p {p}")).unwrap_or_default();
    format!(" The page works while this session is connected. Once it has ended, `ssh -L {port}:127.0.0.1:{port}{via} {}` run on the user's computer reaches the runtime on port {port}, at http://localhost:{port}/ in a browser that opened its notebooks in this session.", server.ssh_host)
}

fn waiting_result(name: &str, status: &Status) -> Value {
    let at = place(name);
    let cancels = " Stopping cancels it. Endeavor can't see which other sessions are waiting for a runtime that isn't up yet, and they would lose it. Tell the user, and call `stop_machine` again with force true only if they agree.";
    let (what, then) = match (&status.job, &status.state) {
        _ if name == LOCAL => (format!("Endeavor is starting on {at}"), cancels),
        (job, State::Queued(queue)) => {
            let id = job.as_ref().map_or(String::new(), |j| format!(" {}", j.id));
            (format!("the Slurm job{id} on {name} is {} ({})", queue.state.to_lowercase(), queue_reason_text(&queue.reason)), cancels)
        }
        (Some(job), _) => (format!("the Slurm job {} on {name} is starting Endeavor", job.id), cancels),
        _ => (format!("Endeavor is starting on {name}"), cancels),
    };
    let mut result = json!({
        "machine": name,
        "stopped": false,
        "state": state_word(&status.state),
        "message": format!("Nothing was stopped: {what}.{then}"),
    });
    if let Some(job) = job_json(status) {
        result["job"] = job;
    }
    if let Some(queue) = queue_json(status) {
        result["queue"] = queue;
    }
    result
}

fn choose_mode(slurm: Option<bool>, prior: Option<&Server>, found: bool) -> Result<bool, String> {
    match (slurm, prior) {
        (Some(true), _) if !found => Err("slurm true can't be used: Endeavor's helper found no Slurm on that machine (no sinfo). Leave slurm out, or give false, to run notebooks there directly.".into()),
        (Some(wanted), _) => Ok(wanted),
        (None, Some(prior)) => Ok(prior.cluster.is_some()),
        (None, None) => Ok(found),
    }
}

#[derive(Debug)]
struct Others {
    count: u64,
    seconds_ago: u64,
}

fn others_result(name: &str, others: &Others) -> Value {
    let minutes = others.seconds_ago / 60;
    let ago = if minutes == 0 { "less than a minute ago".to_owned() } else { format!("{minutes} min ago") };
    let who = if others.count == 1 { "another session was".to_owned() } else { format!("{} other sessions were", others.count) };
    json!({
        "machine": name,
        "stopped": false,
        "active_sessions": others.count,
        "active_seconds_ago": others.seconds_ago,
        "message": format!("Nothing was stopped: {who} active on {} in the last 15 minutes, the latest {ago}. Stopping ends their notebooks too. Tell the user, and call `stop_machine` again with force true only if they agree.", place(name)),
    })
}

#[cfg(test)]
mod tests;

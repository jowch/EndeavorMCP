use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::asks::{Answer, Ask, Outcome};
use crate::guide;
use crate::host_tools;
use crate::http::{self, Head};
use crate::notebooks::{self, Folder, IDLE_HOURS, Julia, Notebooks, Reply};
use crate::results::Results;

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

pub const NOTEBOOK_TOOLS_JSON: &str = include_str!("notebook_tools.json");
static NOTEBOOK_TOOLS: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(NOTEBOOK_TOOLS_JSON).expect("notebook_tools.json"));

pub const MACHINE_TOOLS_JSON: &str = include_str!("machine_tools.json");
static MACHINE_TOOLS: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(MACHINE_TOOLS_JSON).expect("machine_tools.json"));

pub const MACHINE_NAMES: [&str; 4] = ["list_machines", "add_machine", "use_machine", "stop_machine"];

const RENAMED_TOOLS: [(&str, &str); 0] = [];

pub(crate) fn current_name(name: &str) -> &str {
    RENAMED_TOOLS.iter().find(|(old, _)| *old == name).map_or(name, |(_, now)| now)
}

const JULIA_CALLS: [&str; 12] = ["endeavor/set_folder", "endeavor/shutdown", "endeavor/allow_julia_install", "endeavor/use_julia", "endeavor/allow_r_install", "endeavor/allow_uv_install", "endeavor/julia_status", "endeavor/start_julia", "endeavor/r_status", "endeavor/leave_to_plugin", "endeavor/keep_for_app", "endeavor/end_when_idle"];

pub fn is_tool(name: &str) -> bool {
    known_tool(current_name(name), false).is_some()
}

pub const WRITE_TOOLS: [&str; 19] = [
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution", "run_shell", "interrupt_notebook",
    "answer_prompt", "restart_notebook", "close_notebook",
    "apply_package_update", "approve_install", "set_input",
];

pub fn runs_code(tool: &str, arguments: &Value) -> bool {
    match tool {
        "execute_cell" | "submit_changes" | "run_all_cells" | "allow_execution" | "delete_cell" => true,
        "restart_notebook" => true,
        "close_notebook" => true,
        "apply_package_update" => true,
        "set_input" => true,
        "approve_install" => true,
        "run_shell" => true,
        "answer_prompt" => true,
        "add_cell" | "edit_cell" => arguments["run_after"].as_bool() == Some(true),
        "open_notebook" => arguments["run_notebook"].as_bool() == Some(true),
        _ => false,
    }
}

pub(crate) fn names_r_notebook(tool: &str, arguments: &Value) -> bool {
    matches!(tool, "new_notebook" | "open_notebook") && arguments["path"].as_str().is_some_and(|path| crate::notebooks::engines::of_path(path) == wire::backend::Backend::Ember)
}

pub(crate) fn refuses_r_for_want_of_it(tool: &str, arguments: &Value, result: &Value) -> bool {
    names_r_notebook(tool, arguments)
        && result["isError"] == true
        && result["content"].as_array().is_some_and(|content| content.iter().any(|c| c["text"].as_str().is_some_and(|text| text.contains("must end in .jl") && !text.contains("or .R"))))
}

pub(crate) const PYTHON_NOT_IN_APP: &str = "ArgumentError: unsupported::This version of the Endeavor app doesn't show Python notebooks yet. The Endeavor plugin on this computer does (`endeavor serve` or `endeavor mcp`)";

fn opens_python(tool: &str, arguments: &Value, kind: Option<wire::backend::Backend>) -> bool {
    use wire::backend::Backend;
    match tool {
        "open_notebook" => arguments["path"].as_str().is_some_and(|path| crate::notebooks::engines::of_path(path) == Backend::Marimo),
        "new_notebook" => match arguments["path"].as_str() {
            Some(path) => crate::notebooks::engines::of_path(path) == Backend::Marimo,
            None => kind == Some(Backend::Marimo),
        },
        _ => false,
    }
}

pub fn changes_notebook(tool: &str) -> bool {
    matches!(tool, "edit_cell" | "edit_cells" | "add_cell" | "delete_cell" | "move_cell" | "fold_cell" | "new_notebook" | "apply_package_update")
}

pub fn asks_first(tool: &str, arguments: &Value, policy: &str, edits: bool) -> bool {
    policy != "plan" && (tool == "approve_install" || (policy == "ask" && runs_code(tool, arguments)) || (edits && changes_notebook(tool)))
}

struct Policy {
    policy: String,
    asks: bool,
    edits: bool,
}

pub struct Standalone {
    pub port: u16,
    pub folder: String,
    pub no_folder: bool,
    pub host: Option<String>,
}

pub(crate) fn browser_link(port: u16, target: &str) -> String {
    format!("http://localhost:{port}{target}")
}

pub(crate) fn entry_link(url: &str, token: &str) -> String {
    let join = if url.contains('?') { '&' } else { '?' };
    format!("{url}{join}token={token}")
}

pub(crate) fn without_token(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else { return url.to_owned() };
    let rest: Vec<&str> = query.split('&').filter(|pair| !pair.is_empty() && !pair.starts_with("token=")).collect();
    if rest.is_empty() { path.to_owned() } else { format!("{path}?{}", rest.join("&")) }
}

pub type OnFolder = Box<dyn Fn(&str, &str) + Send + Sync>;

pub struct Bridge {
    pub julia: Arc<Julia>,
    pub notebooks: Arc<Notebooks>,
    pub token: String,
    policies: Mutex<HashMap<String, Policy>>,
    folders: Mutex<HashMap<String, Option<String>>>,
    sessions_showing_python: Mutex<std::collections::HashSet<String>>,
    results: Results,
    starting_calls: Mutex<HashMap<String, (String, Value, Instant)>>,
    shell_env: Vec<(&'static str, Option<String>)>,
    pub standalone: Option<Standalone>,
    pub on_folder: std::sync::OnceLock<OnFolder>,
    pub views: crate::views::Views,
}

#[derive(Default)]
pub struct Caller {
    pub owner: String,
    pub host: String,
    pub has_skills: bool,
    pub browser_port: Option<u16>,
    pub front: bool,
    pub no_folder: bool,
    pub views: bool,
    pub several: bool,
    pub app: bool,
    pub consent: bool,
    pub auto_mode: Option<bool>,
    pub front_process: Option<Front>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Front {
    pub pid: i32,
    pub started: Option<u64>,
    pub boot: Option<String>,
}

impl Front {
    pub fn own() -> Front {
        Front { pid: std::process::id() as i32, started: crate::own_start_time(), boot: crate::own_boot() }
    }

    pub fn header(&self) -> String {
        let mut head = format!("X-Endeavor-Front-Pid: {}\r\n", self.pid);
        if let Some(started) = self.started {
            head.push_str(&format!("X-Endeavor-Front-Started: {started}\r\n"));
        }
        if let Some(boot) = self.boot.as_deref().and_then(clean_label) {
            head.push_str(&format!("X-Endeavor-Front-Boot: {boot}\r\n"));
        }
        head
    }

    fn of(request: &Head) -> Option<Front> {
        let pid = request.header("X-Endeavor-Front-Pid")?.trim().parse().ok().filter(|&pid: &i32| pid > 0)?;
        let started = request.header("X-Endeavor-Front-Started").and_then(|t| t.trim().parse().ok());
        let boot = request.header("X-Endeavor-Front-Boot").map(|b| b.trim().to_owned()).filter(|b| !b.is_empty());
        Some(Front { pid, started, boot })
    }

    pub fn alive(&self) -> bool {
        crate::pid_alive(self.pid, self.started, self.boot.as_deref())
    }
}

pub(crate) const ASK_IN_CHAT: &str = "It doesn't ask the user: calling it is the user's yes. Ask them first, end your reply, and call it only once they have said yes in a later message; never on your own.";

const NO_ONE_ASKS: &str = "Endeavor can't ask the user over this connection (a client connected straight to `endeavor serve`), so it can't be approved here: calling it installs nothing. Tell the user what it would install and its size where known, that it needs their OK, and that it can be approved only from a client connected through `endeavor mcp` that shows Endeavor's dialog, such as a new Claude Code session in a terminal (not `claude -p`), or from the Endeavor desktop app (a program they install, not a web page): they close this notebook here, then open its file in the app, which asks them. The notebook's page (its browser_url) can't approve it either: it has no button for this, so don't send them there to approve it. Don't work around it.";

const ASK_STEP_NO_ONE: &str = "Endeavor can't ask the user over this connection, so this can't be approved here: approve_install installs nothing. Tell the user what it would install and its size where known, that it needs their OK, and that it can be approved only from a client connected through `endeavor mcp` that shows Endeavor's dialog, such as a new Claude Code session in a terminal (not `claude -p`), or from the Endeavor desktop app (a program they install, not a web page): they close this notebook here, then open its file in the app, which asks them. The notebook's page (its browser_url) can't approve it either: it has no button for this, so don't send them there to approve it. Then end your reply.";

const NO_ONE_ASKS_ERROR: &str = "download_cant_ask::Endeavor can't ask the user over this connection: this client is connected straight to `endeavor serve`, which can neither show Endeavor's dialog nor open a page in their browser, and no dialog was shown. So this can't be approved here: nothing is installed, and calling approve_install again won't change that. Tell the user what it would install and its size where known, that it needs their OK, and that it can be approved only from a client connected through `endeavor mcp` that shows Endeavor's dialog, such as a new Claude Code session in a terminal (not `claude -p`), or from the Endeavor desktop app (a program they install, not a web page): they close this notebook here, then open its file in the app, which asks them. The notebook's page (its browser_url) can't approve it either: it has no button for this, so don't send them there to approve it. Don't work around it.";

pub(crate) fn clean_label(text: &str) -> Option<String> {
    let label: String = text.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(LABEL_MAX).collect();
    let label = label.trim_end().to_owned();
    (!label.is_empty()).then_some(label)
}

const LABEL_MAX: usize = 80;

impl Caller {
    fn of(request: &Head, app_runtime: bool) -> Caller {
        let header = |name| request.header(name).unwrap_or_default().to_owned();
        let owner = request.header("X-Endeavor-Session").or_else(|| request.header("Mcp-Session-Id")).unwrap_or_default().to_owned();
        let browser_port = request.header("X-Endeavor-Browser-Port").and_then(|port| port.trim().parse().ok()).filter(|&port| port != 0);
        let several = header("X-Endeavor-Notebooks") == "several";
        let app = match header("X-Endeavor-Client").as_str() {
            "app" => true,
            "plugin" => false,
            _ => app_runtime,
        };
        let consent = header("X-Endeavor-Consent") == "front" || header("X-Endeavor-Client") == "app";
        let auto_mode = (header("X-Endeavor-Client") == "plugin").then(|| header("X-Endeavor-Mode") == "auto");
        Caller { owner, host: header("X-Endeavor-Host"), has_skills: header("X-Endeavor-Skills") == "plugin", browser_port, front: false, no_folder: false, views: false, several, app, consent, auto_mode, front_process: Front::of(request) }
    }

    fn no_one_asks(&self) -> bool {
        !self.front && !self.consent
    }
}

fn no_one_asks_said(reply: String) -> String {
    if !reply.contains(ASK_IN_CHAT) && !reply.contains(crate::notebooks::ASK_STEP) {
        return reply;
    }
    reply.replace(ASK_IN_CHAT, NO_ONE_ASKS).replace(crate::notebooks::ASK_STEP, ASK_STEP_NO_ONE)
}

impl Bridge {
    pub fn new(token: String, engine_token: String, depot: &str) -> Bridge {
        let shell_env = vec![
            ("JULIA_DEPOT_PATH", Some(crate::paths::with_user_depot(depot))),
            ("ENDEAVOR_TOKEN", None),
            ("ENDEAVOR_STATE", None),
            ("ENDEAVOR_LAUNCHER", None),
            ("ENDEAVOR_BUILD", None),
            ("ENDEAVOR_PORT", None),
            ("ENDEAVOR_FOLDER", None),
            ("ENDEAVOR_NO_FOLDER", None),
            ("ENDEAVOR_HOST_TOOLS", None),
            ("ENDEAVOR_IDLE_HOURS", None),
            ("ENDEAVOR_EXIT_IDLE", None),
        ];
        let julia = Arc::new(Julia::new(engine_token));
        let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        Bridge {
            notebooks: Arc::new(Notebooks::new(None, Box::new(clock))),
            julia,
            token,
            policies: Mutex::default(),
            folders: Mutex::default(),
            sessions_showing_python: Mutex::default(),
            results: Results::default(),
            starting_calls: Mutex::default(),
            shell_env,
            standalone: None,
            on_folder: std::sync::OnceLock::new(),
            views: crate::views::Views::default(),
        }
    }

    fn shows_python(&self, owner: &str) -> bool {
        self.sessions_showing_python.lock().unwrap().contains(owner)
    }

    fn folder(&self, owner: &str) -> Folder {
        match self.folders.lock().unwrap().get(owner) {
            Some(Some(dir)) => Folder::In(dir.clone()),
            Some(None) => Folder::Unknown,
            None => match &self.standalone {
                Some(standalone) => Folder::In(standalone.folder.clone()),
                None => Folder::Process,
            },
        }
    }

    pub fn app_call(&self, raw: &[u8]) -> Option<String> {
        let message = serde_json::from_slice::<Value>(raw).ok().filter(Value::is_object)?;
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let answer = |result: Result<Value, String>| {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            to_json(&match result {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
            })
        };
        let text = |key: &str, default: &str| params.get(key).map_or(default.to_owned(), julia_string);
        match message["method"].as_str().unwrap_or_default() {
            "endeavor/set_policy" => {
                let (owner, policy) = (text("owner", ""), text("policy", "ask"));
                let (asks, edits) = (params["asks"] == true, params["edits"] == true);
                let what = match (asks, edits) {
                    _ if policy == "plan" => "",
                    (true, true) => ", runs and edits ask first",
                    (true, false) => ", runs ask first",
                    _ => "",
                };
                eprintln!("[ Info: Session {owner} policy: {policy}{what}");
                self.starting_calls.lock().unwrap().remove(&owner);
                self.notebooks.set_auto(&owner, policy == "auto");
                self.policies.lock().unwrap().insert(owner, Policy { policy, asks, edits });
            }
            "endeavor/answer_run" => {
                let given = Answer {
                    allow: params["allow"] == true,
                    user_ran: params.get("user_ran").cloned().unwrap_or_else(|| json!([])),
                    packages: params.get("packages").filter(|p| p.is_array()).cloned(),
                };
                let id = params["id"].as_u64().ok_or_else(|| "ArgumentError: invalid_argument::id must be a number".to_owned());
                return Some(answer(id.and_then(|id| self.notebooks.asks.answer(id, given)).map(|()| json!({}))));
            }
            "endeavor/set_notebook" => {
                let (owner, notebook) = (text("owner", ""), params.get("notebook").filter(|n| !n.is_null()).map_or(String::new(), julia_string));
                self.notebooks.bind(&owner, &notebook);
                eprintln!("[ Info: Session {owner} notebook: {}", if notebook.is_empty() { "(none)" } else { &notebook });
            }
            "endeavor/recent_sessions" => {
                let within = params["within_seconds"].as_f64().ok_or_else(|| "ArgumentError: invalid_argument::within_seconds must be a number".to_owned());
                return Some(answer(within.and_then(|within| self.notebooks.recent_sessions(&text("owner", ""), within))));
            }
            "endeavor/set_idle_limit" => {
                let hours = match params.get("hours") {
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(IDLE_HOURS),
                    Some(Value::Bool(b)) => *b as u8 as f64,
                    _ => IDLE_HOURS,
                };
                self.notebooks.set_idle_limit(hours);
                let shown = params.get("hours").map_or(IDLE_HOURS.to_string(), julia_string);
                eprintln!("[ Info: Idle notebooks stop after: {}", if hours == 0.0 { "never".into() } else { format!("{shown} hours") });
            }
            "endeavor/stop_notebook" => {
                let path = text("path", "");
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let reply = match self.notebooks.stop_notebook(&path) {
                    Ok(result) => {
                        eprintln!("[ Info: Stopped notebook {path}: {}", result["stopped"]);
                        json!({ "jsonrpc": "2.0", "id": id, "result": result })
                    }
                    Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e } }),
                };
                return Some(to_json(&reply));
            }
            "endeavor/set_session_folder" => {
                let (owner, folder) = (text("owner", ""), params.get("folder").filter(|f| !f.is_null()).map_or(String::new(), julia_string));
                let kind = match params["kind"].as_str() {
                    Some(said) => {
                        let kind = match said {
                            "julia" => Some(wire::backend::Backend::Pluto),
                            "r" => Some(wire::backend::Backend::Ember),
                            "python" => Some(wire::backend::Backend::Marimo),
                            _ => None,
                        };
                        self.notebooks.set_kind(&owner, kind);
                        kind
                    }
                    None => self.notebooks.kind(&owner),
                };
                if let Some(shows) = params["shows_python"].as_bool() {
                    let mut showing = self.sessions_showing_python.lock().unwrap();
                    if shows { showing.insert(owner.clone()) } else { showing.remove(&owner) };
                }
                let mut folders = self.folders.lock().unwrap();
                if params["no_folder"] == true {
                    folders.insert(owner, None);
                } else if folder.is_empty() {
                    folders.remove(&owner);
                } else {
                    folders.insert(owner, Some(folder.clone()));
                    drop(folders);
                    if let Some(told) = self.on_folder.get() {
                        told(&folder, kind.map_or("unknown", |kind| match kind {
                            wire::backend::Backend::Ember => "r",
                            wire::backend::Backend::Marimo => "python",
                            wire::backend::Backend::Pluto => "julia",
                        }));
                    }
                }
            }
            "endeavor/tool_result" => {
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let call_id = params.get("call_id").and_then(Value::as_str);
                return Some(answer(Ok(self.results.find(&text("owner", ""), call_id, &text("tool", ""), &arguments))));
            }
            "endeavor/run_preview" => {
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                return Some(answer(self.notebooks.run_preview(&text("tool", ""), &arguments)));
            }
            "endeavor/restart_notebook" => return Some(answer(self.notebooks.restart(&text("notebook_id", ""), false))),
            "endeavor/restore_cells" => return Some(answer(self.notebooks.restore_cells(&text("notebook_id", ""), &params["cells"]))),
            "endeavor/stop_session" => {
                let owner = text("owner", "");
                let stopping = self.notebooks.session_runs(&owner);
                for notebook in stopping.clone() {
                    let (notebooks, owner) = (self.notebooks.clone(), owner.clone());
                    std::thread::spawn(move || notebooks.stop_for(&owner, &notebook, false));
                }
                return Some(answer(Ok(json!({ "notebooks": stopping }))));
            }
            "endeavor/move_notebook" => return Some(answer(self.notebooks.move_notebook(&text("notebook_id", ""), &text("path", "")))),
            "endeavor/file_info" => return Some(answer(notebooks::file_info(&text("path", "")))),
            "endeavor/new_notebook" => {
                let owner = text("owner", "");
                if !self.shows_python(&owner) && opens_python("new_notebook", &json!({}), self.notebooks.kind(&owner)) {
                    return Some(answer(Err(PYTHON_NOT_IN_APP.to_owned())));
                }
                let folder = self.folder(&owner);
                return Some(answer(self.notebooks.new_for(&owner, &folder)));
            }
            method if JULIA_CALLS.contains(&method) => return None,
            _ => return Some(self.dispatch(&message, &Caller { app: true, consent: true, ..Caller::default() }, &|| false).unwrap_or_else(|| "{}".into())),
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })))
    }

    pub fn mcp(&self, request: &Head, reader: &mut BufReader<wire::Shared>, client: &mut wire::Shared) -> io::Result<bool> {
        let mut caller = Caller::of(request, self.standalone.is_none());
        if caller.host.is_empty()
            && !caller.app
            && let Some(host) = self.standalone.as_ref().and_then(|s| s.host.clone())
        {
            caller.host = host;
        }
        let no_one_asks = caller.no_one_asks();
        post(request, reader, client, request.keeps_alive(), caller.owner.is_empty(), |message, gone| {
            let reply = self.dispatch(message, &caller, gone);
            if no_one_asks { reply.map(no_one_asks_said) } else { reply }
        })
    }

    fn dispatch(&self, message: &Value, caller: &Caller, gone: &dyn Fn() -> bool) -> Option<String> {
        let began = Instant::now();
        if message["method"] == "notifications/cancelled" && !caller.owner.is_empty() {
            let request = &message["params"]["requestId"];
            self.starting_calls.lock().unwrap().remove(&caller.owner);
            if self.notebooks.asks.cancel(&caller.owner, request) {
                self.notebooks.publish();
            } else if let Some((notebook, runs_code)) = self.notebooks.cancelled(&caller.owner, request) {
                let (notebooks, owner) = (self.notebooks.clone(), caller.owner.clone());
                std::thread::spawn(move || notebooks.stop_for(&owner, &notebook, runs_code));
            }
        }
        answer(message, caller, |params| {
            if let Some(name) = params["name"].as_str().filter(|name| crate::views::is_view_tool(name)) {
                return self.view_call(name, params);
            }
            if let Some(auto) = caller.auto_mode.filter(|_| !caller.owner.is_empty()) {
                self.notebooks.set_auto(&caller.owner, auto);
            }
            let call = Call { caller, request: &message["id"], call_id: params["_meta"]["claudecode/toolUseId"].as_str(), gone, began };
            let result = self.call_tool(params, &call);
            if !caller.owner.is_empty() {
                let arguments = params.get("arguments").unwrap_or(&Value::Null);
                self.results.record(&caller.owner, call.call_id, current_name(params["name"].as_str().unwrap_or_default()), arguments, &result);
            }
            self.notebooks.publish();
            result
        })
    }

    fn view_call(&self, name: &str, params: &Value) -> Value {
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if let Err(result) = check_arguments(name, &arguments, false) {
            return result;
        }
        let page = |backend, notebook: Option<&str>| match (backend, notebook) {
            (wire::backend::Backend::Ember, Some(id)) => self.notebooks.ember_page(id),
            (wire::backend::Backend::Marimo, Some(_)) => Err("not_supported: Python notebooks can't show in this view yet; open the notebook's browser link instead".to_owned()),
            _ => {
                let port = *self.julia.port.get().ok_or("not_ready: Julia isn't running")?;
                crate::views::page_from_julia(port, &self.julia.token)
            }
        };
        match self.views.call(name, &arguments, page, |id| self.notebooks.open_in(id)) {
            Ok(result) => json!({ "content": [{ "type": "text", "text": "ok" }], "structuredContent": result, "isError": false }),
            Err(error) => json!({ "content": [{ "type": "text", "text": error }], "isError": true }),
        }
    }

    fn call_tool(&self, params: &Value, call: &Call) -> Value {
        let caller = call.caller;
        let text = |result: &Value| json!({ "content": [{ "type": "text", "text": to_json(result) }], "isError": false });
        let help = !caller.has_skills;
        let (name, mut arguments) = match call_parts(params, help, false) {
            Ok(call) => call,
            Err(result) => return result,
        };
        let refusal = self.refusal(caller, name, &arguments);
        if refusal.is_none() {
            if let Err(result) = check_arguments(name, &arguments, help) {
                return result;
            }
            if let Some((why, says_what_to_do)) = notebooks::path_refusal(name, &arguments, &self.folder(&caller.owner)) {
                return tool_error(&why, help && !says_what_to_do);
            }
        }
        self.notebooks.note_call(&caller.owner, caller.app, caller.front_process.as_ref());
        if caller.several {
            self.notebooks.allow_several(&caller.owner);
        }
        if !caller.owner.is_empty() && acts(name, &arguments) {
            if self.notebooks.asks.moved_on(&caller.owner, name, &arguments) {
                self.notebooks.publish();
            }
            self.starting_calls.lock().unwrap().retain(|owner, (tool, args, _)| *owner != caller.owner || (tool == name && *args == arguments));
        }
        self.notebooks.note_activity(&arguments);
        if let Some(refusal) = refusal {
            return tool_error(&refusal, false);
        }
        if name == guide::TOOL {
            return match guide::read(&arguments) {
                Ok(guide) => json!({ "content": [{ "type": "text", "text": guide }], "isError": false }),
                Err(error) => tool_error(&error, false),
            };
        }
        if host_tools::NAMES.contains(&name) {
            let folder = self.folder(&caller.owner);
            let env: Vec<_> = self.shell_env.iter().map(|(name, value)| (*name, value.as_deref())).collect();
            let shell = host_tools::Shell { folder: folder.dir(), env: &env, owner: &caller.owner, wait: Duration::ZERO, started: None };
            let started = host_tools::started(name, &arguments, &shell);
            if started.is_none()
                && let Err(result) = self.ask_first(call, name, &mut arguments)
            {
                return result;
            }
            let shell = host_tools::Shell { wait: shell_wait(call.began), started: started.as_ref(), ..shell };
            return match host_tools::call(name, &arguments, &shell) {
                Ok(result) => text(&result),
                Err(error) => tool_error(&error, false),
            };
        }
        if name == "keep_notebook_alive" {
            return self.notebooks.keep_alive(&arguments).map_or_else(|e| tool_error(&e, help), |r| text(&r));
        }
        let folder = self.folder(&caller.owner);
        if let Some(refusal) = self.notebooks.refusal(&caller.owner, name, &arguments, &folder) {
            return tool_error(&refusal, help);
        }
        if name == "view_cell_output" && let Some(crop) = arguments["crop"].as_str() {
            let notebook = arguments["notebook_id"].as_str().unwrap_or_default();
            return match self.views.crop(notebook, crop) {
                Some(png) => image_result(
                    &json!({ "notebook_id": notebook, "crop": crop, "source": "view",
                        "message": "The box the user drew in the notebook view, as the view could draw it: its pictures, plots and drawings. Text, tables and 3D plots under the box don't show in it; read those cells with read_cell or view_cell_output." }),
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png.as_slice()),
                ),
                None => tool_error(&format!("not_found: no picture {crop} for notebook {notebook}. No picture was kept for this box: nothing in it could be drawn (text, HTML and 3D views can't be), it wasn't saved, or it's over an hour old. Read the box's cells instead."), false),
            };
        }
        let run = match self.ask_first(call, name, &mut arguments) {
            Ok(run) => run,
            Err(result) => return result,
        };
        let notebook = arguments.get("notebook_id").and_then(Value::as_str);
        let _running = match notebook {
            Some(notebook) if !caller.owner.is_empty() && !call.request.is_null() => {
                Some(self.notebooks.running(&caller.owner, call.request, notebook, run && runs_code(name, &arguments)))
            }
            _ => None,
        };
        let reply = if run {
            self.notebooks.tool_watched(&caller.owner, name, &arguments, &folder, call.began, call.gone)
        } else {
            self.notebooks.tool_unrun(&caller.owner, name, &arguments, &folder, call.began)
        };
        let waits_on_engine = matches!(&reply, Err(error) if starting(error)) && !(call.gone)();
        if matches!(name, "open_notebook" | "new_notebook") && !waits_on_engine {
            self.starting_calls.lock().unwrap().remove(&caller.owner);
        }
        match reply {
            Ok(Reply::Json(mut result)) => {
                self.add_browser_url(name, caller, &mut result);
                text(&result)
            }
            Ok(Reply::Image { meta, png_base64 }) => image_result(&meta, png_base64),
            Err(error) => tool_error(&crate::core::offer_for(&error, caller.app), help),
        }
    }

    fn add_browser_url(&self, tool: &str, caller: &Caller, result: &mut Value) {
        let Some(port) = caller.browser_port.or_else(|| self.standalone.as_ref().filter(|_| !caller.app).map(|s| s.port)) else { return };
        let Value::Object(fields) = result else { return };
        let target = match tool {
            "new_notebook" | "open_notebook" => match fields.get("notebook_id").and_then(Value::as_str) {
                Some(id) => match self.notebooks.backend_of(id) {
                    wire::backend::Backend::Pluto => format!("/edit?id={id}"),
                    wire::backend::Backend::Ember => format!("/ember/edit?id={id}"),
                    wire::backend::Backend::Marimo => match fields.get("path").and_then(Value::as_str) {
                        Some(path) => format!("/marimo/?file={}", wire::backend::query_value(path)),
                        None => return,
                    },
                },
                None => return,
            },
            "session_status" => "/".to_owned(),
            _ => return,
        };
        fields.insert("browser_url".into(), browser_link(port, &target).into());
    }

    fn ask_first(&self, call: &Call, tool: &str, arguments: &mut Value) -> Result<bool, Value> {
        let owner = &call.caller.owner;
        let (held, edits) = match self.policies.lock().unwrap().get(owner) {
            Some(p) if p.asks => (asks_first(tool, arguments, &p.policy, p.edits), p.edits),
            _ => (false, false),
        };
        if owner.is_empty() || !held {
            if !owner.is_empty() && self.notebooks.asks.moved_on(owner, "", &Value::Null) {
                self.notebooks.publish();
            }
            return Ok(true);
        }
        let carried_on = self.starting_calls.lock().unwrap().get(owner).is_some_and(|(t, args, allowed)| t == tool && args == arguments && allowed.elapsed() < STARTING_CALL_KEPT);
        if carried_on {
            eprintln!("[ Info: Session {owner} makes its allowed {tool} again while the engine starts; not asked again");
            return Ok(true);
        }
        if !self.notebooks.followed() {
            return Err(tool_error("ArgumentError: no_app::Endeavor isn't connected to ask the user about this. Try again once Endeavor is open.", false));
        }
        let code = arguments.get("notebook_id").and_then(Value::as_str).and_then(|id| self.notebooks.code_print(id));
        let ask = Ask { owner, call_id: call.call_id, request: call.request, tool, arguments, code, since: self.notebooks.now() };
        let id = self.notebooks.asks.add(ask);
        eprintln!("[ Info: Session {owner} asks before {tool} (ask {id}, call {})", call.call_id.unwrap_or("?"));
        self.notebooks.publish();
        let deadline = call.began + ask_wait(tool);
        let outcome = self.notebooks.asks.wait(id, call.gone, deadline);
        self.notebooks.publish();
        match outcome {
            Outcome::Answered(Answer { allow: true, user_ran, packages }) => {
                if tool == "approve_install"
                    && let Some(packages) = packages
                {
                    arguments["packages"] = crate::notebooks::seen_packages(arguments.get("packages"), &packages);
                }
                if matches!(tool, "open_notebook" | "new_notebook") {
                    self.starting_calls.lock().unwrap().insert(owner.clone(), (tool.to_owned(), arguments.clone(), Instant::now()));
                }
                let notebook = arguments.get("notebook_id").map(julia_string).unwrap_or_default();
                if user_ran.as_array().is_some_and(|cells| !cells.is_empty()) {
                    let _ = self.notebooks.run_anyway(&notebook, &user_ran);
                }
                Ok(true)
            }
            Outcome::Answered(_) if edits && changes_notebook(tool) => Err(tool_error("ArgumentError: not_approved::The user chose not to make this change.", false)),
            Outcome::Answered(_) if matches!(tool, "edit_cell" | "add_cell") => Ok(false),
            Outcome::Answered(_) => Err(tool_error("ArgumentError: not_approved::The user chose not to run this.", false)),
            Outcome::Cancelled | Outcome::Gone => Err(tool_error("ArgumentError: cancelled::The call was cancelled before the user answered.", false)),
            Outcome::Unanswered => Err(tool_error(&unanswered(tool), false)),
        }
    }

    fn refusal(&self, caller: &Caller, tool: &str, arguments: &Value) -> Option<String> {
        if tool == "approve_install" && caller.no_one_asks() {
            return Some(NO_ONE_ASKS_ERROR.to_owned());
        }
        if host_tools::NAMES.contains(&tool) && caller.host.is_empty() {
            return Some(host_tool_refusal(tool));
        }
        if caller.app && !caller.owner.is_empty() && !self.shows_python(&caller.owner) && opens_python(tool, arguments, self.notebooks.kind(&caller.owner)) {
            return Some(PYTHON_NOT_IN_APP.to_owned());
        }
        let plan = self.policies.lock().unwrap().get(&caller.owner).is_some_and(|p| p.policy == "plan");
        if plan && (WRITE_TOOLS.contains(&tool) || runs_code(tool, arguments)) {
            let what = if tool == "run_shell" { "run a command on the server" } else { "change or run the notebook" };
            return Some(format!(
                "ArgumentError: plan_mode::Plan mode is read-only: `{tool}` would {what}. Finish the plan; the user switches modes to carry it out."
            ));
        }
        None
    }
}

const STARTING_CALL_KEPT: Duration = Duration::from_secs(5 * 60);

fn starting(error: &str) -> bool {
    error.split_once("::").is_some_and(|(kind, _)| STARTING_KINDS.contains(&kind))
}

fn ask_wait(tool: &str) -> Duration {
    match tool {
        "open_notebook" | "new_notebook" => Duration::from_secs(20),
        _ => Duration::from_secs_f64(notebooks::WAIT_SECONDS),
    }
}

fn shell_wait(began: Instant) -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_SHELL_WAIT_SECS").ok()?.parse().ok()).flatten();
    if let Some(seconds) = test {
        return Duration::from_secs_f64(seconds);
    }
    Duration::from_secs_f64(notebooks::WAIT_SECONDS).saturating_sub(began.elapsed()).max(Duration::from_secs_f64(notebooks::WAIT_FLOOR_SECONDS))
}

fn acts(tool: &str, arguments: &Value) -> bool {
    WRITE_TOOLS.contains(&tool) || runs_code(tool, arguments)
}

fn unanswered(tool: &str) -> String {
    format!(
        "ArgumentError: waiting_for_user::The user hasn't answered yet, so nothing was changed or run. \
The request is still on their screen. To keep waiting, call `{tool}` again with the same arguments, and don't try another way meanwhile. \
If you stop waiting, tell the user the request is waiting for their answer. When they write back, call `{tool}` again with the same arguments: it goes ahead if they allowed it."
    )
}

pub(crate) fn host_tool_refusal(tool: &str) -> String {
    format!("ArgumentError: host_tools::`{tool}` is only for sessions on a server. This session runs on the user's computer: use your own file and shell tools.")
}

pub(crate) fn call_arguments(params: &Value, help: bool) -> Result<Value, Value> {
    match params.get("arguments") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(arguments) if arguments.is_object() => Ok(arguments.clone()),
        Some(_) => Err(tool_error("ArgumentError: invalid_argument::arguments must be an object", help)),
    }
}

fn unknown_tool_result(name: &str, help: bool) -> Value {
    tool_error(&format!("ArgumentError: unknown_tool::Unknown tool: '{name}'"), help)
}

static INPUT_SCHEMAS: LazyLock<HashMap<String, Value>> = LazyLock::new(|| {
    let listed = |tools: &Value| tools.as_array().cloned().unwrap_or_default();
    let tools = [listed(&NOTEBOOK_TOOLS), listed(&MACHINE_TOOLS), host_tools::schemas(), vec![guide::schema()], crate::views::schemas()];
    (tools.into_iter().flatten())
        .filter_map(|tool| Some((tool["name"].as_str()?.to_owned(), tool["inputSchema"].clone())))
        .collect()
});

fn known_tool(name: &str, machines: bool) -> Option<&'static Value> {
    INPUT_SCHEMAS.get(name).filter(|_| machines || !MACHINE_NAMES.contains(&name))
}

pub(crate) fn check_arguments(tool: &str, arguments: &Value, help: bool) -> Result<(), Value> {
    let Some(schema) = known_tool(tool, true) else { return Ok(()) };
    let known: Vec<&str> = schema["properties"].as_object().into_iter().flatten().map(|(name, _)| name.as_str()).collect();
    if known.is_empty() {
        return Ok(());
    }
    let quote = |names: &[&str]| names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ");
    let unknown: Vec<&str> = (arguments.as_object().into_iter().flatten()).map(|(name, _)| name.as_str()).filter(|name| !known.contains(name)).collect();
    let help = help && !host_tools::NAMES.contains(&tool);
    let fail = |message: String| Err(tool_error(&format!("ArgumentError: invalid_argument::{message}"), help));
    if !unknown.is_empty() {
        let (these, are) = if unknown.len() == 1 { ("is", "an argument") } else { ("are", "arguments") };
        return fail(format!("{} {these} not {are} of `{tool}`. Its arguments: {}.", quote(&unknown), quote(&known)));
    }
    let missing: Vec<&str> = (schema["required"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|name| arguments.get(*name).is_none())
        .collect();
    if !missing.is_empty() {
        return fail(format!("`{tool}` needs {}.", quote(&missing)));
    }
    Ok(())
}

pub(crate) fn call_parts(params: &Value, help: bool, machines: bool) -> Result<(&str, Value), Value> {
    let arguments = call_arguments(params, help)?;
    match params.get("name") {
        Some(Value::String(name)) if known_tool(current_name(name), machines).is_some() => Ok((current_name(name), arguments)),
        Some(Value::String(name)) => Err(unknown_tool_result(name, help)),
        Some(other) => Err(unknown_tool_result(&julia_string(other), help)),
        None => Err(unknown_tool_result("", help)),
    }
}

pub(crate) fn checked_call(params: &Value, help: bool, machines: bool) -> Result<(&str, Value), Value> {
    let (name, arguments) = call_parts(params, help, machines)?;
    check_arguments(name, &arguments, help)?;
    Ok((name, arguments))
}

pub(crate) fn method_not_found(id: &Value, method: &str) -> String {
    to_json(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {method}") } }))
}

struct Call<'a> {
    caller: &'a Caller,
    request: &'a Value,
    call_id: Option<&'a str>,
    gone: &'a dyn Fn() -> bool,
    began: Instant,
}

#[cfg(unix)]
fn closed(socket: &TcpStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: a one-byte peek into a local buffer; MSG_DONTWAIT keeps it from blocking.
    let n = unsafe { libc::recv(socket.as_raw_fd(), (&mut byte as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n == 0 || (n < 0 && !matches!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted))
}

#[cfg(windows)]
fn closed(_socket: &TcpStream) -> bool {
    false
}

pub(crate) fn post(
    request: &Head,
    reader: &mut BufReader<wire::Shared>,
    client: &mut wire::Shared,
    keep_alive: bool,
    issue_session: bool,
    answer_with: impl FnOnce(&Value, &dyn Fn() -> bool) -> Option<String>,
) -> io::Result<bool> {
    let body = http::read_body(reader, request.request_body()?)?;
    if request.header("MCP-Protocol-Version").is_some_and(|v| !SUPPORTED_VERSIONS.contains(&v)) {
        http::respond(client, "400 Bad Request", Some("application/json"), br#"{"error":"unsupported_protocol_version"}"#, keep_alive)?;
        return Ok(keep_alive);
    }
    let Some(message) = serde_json::from_slice::<Value>(&body).ok().filter(Value::is_object) else {
        http::respond(client, "400 Bad Request", None, br#"{"error":"Invalid JSON"}"#, keep_alive)?;
        return Ok(keep_alive);
    };
    reply(request, client, &message, keep_alive, issue_session, answer_with)
}

pub(crate) fn reply(
    request: &Head,
    client: &mut wire::Shared,
    message: &Value,
    keep_alive: bool,
    issue_session: bool,
    reply: impl FnOnce(&Value, &dyn Fn() -> bool) -> Option<String>,
) -> io::Result<bool> {
    let held = Held::new(client.clone(), request, message, keep_alive);
    let reply = reply(message, &|| held.waiting());
    if held.streaming() {
        if let Some(reply) = reply {
            http::write_chunk(client, format!("event: message\ndata: {reply}\n\n").as_bytes())?;
        }
        http::write_chunk(client, b"")?;
        return Ok(keep_alive);
    }
    let session = (issue_session && message["method"] == "initialize").then(new_session_id).flatten();
    let headers: Vec<(&str, &str)> = session.iter().map(|id| ("Mcp-Session-Id", id.as_str())).collect();
    match reply {
        Some(reply) => http::respond_with(client, "200 OK", Some("application/json"), &headers, reply.as_bytes(), keep_alive)?,
        None => http::respond(client, "202 Accepted", None, b"", keep_alive)?,
    }
    Ok(keep_alive)
}

fn new_session_id() -> Option<String> {
    match crate::random_hex::<16>() {
        Ok(hex) => Some(format!("mcp-{hex}")),
        Err(e) => {
            eprintln!("┌ Warning: No MCP session id for a client without X-Endeavor-Session: {e}");
            None
        }
    }
}

const KEEP_WAITING: Duration = Duration::from_secs(15);

pub(crate) struct Waits(&'static str);

impl Waits {
    pub(crate) const WORKING: Waits = Waits("Still working on the call");
    pub(crate) const ANSWER: Waits = Waits("Waiting for the user's answer");
    pub(crate) const CELLS: Waits = Waits("Waiting for the cells to finish");
    pub(crate) const STOP: Waits = Waits("Waiting for the cells to stop");
}

thread_local! {
    static WAITING_ON: Cell<&'static str> = const { Cell::new(Waits::WORKING.0) };
}

pub(crate) fn waiting_on(what: Waits, check: &dyn Fn() -> bool) -> bool {
    let before = WAITING_ON.replace(what.0);
    let gone = check();
    WAITING_ON.set(before);
    gone
}

struct Held {
    socket: wire::Shared,
    streams: bool,
    keep_alive: bool,
    progress_token: Option<Value>,
    sent: RefCell<Option<(u64, Instant)>>,
}

impl Held {
    fn new(socket: wire::Shared, request: &Head, message: &Value, keep_alive: bool) -> Held {
        let streams = request.version() == "HTTP/1.1" && request.header("Accept").is_some_and(|accept| accept.contains("text/event-stream"));
        let progress_token = message["params"]["_meta"].get("progressToken").filter(|t| t.is_string() || t.is_number()).cloned();
        Held { socket, streams, keep_alive, progress_token, sent: RefCell::new(None) }
    }

    fn streaming(&self) -> bool {
        self.sent.borrow().is_some()
    }

    fn waiting(&self) -> bool {
        let due = self.sent.borrow().is_none_or(|(_, last)| last.elapsed() >= KEEP_WAITING);
        if self.streams && due && self.keep_waiting().is_err() {
            return true;
        }
        closed(&self.socket)
    }

    fn keep_waiting(&self) -> io::Result<()> {
        let mut out = &self.socket;
        let mut sent = self.sent.borrow_mut();
        if sent.is_none() {
            let close = if self.keep_alive { "" } else { "Connection: close\r\n" };
            write!(out, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n{close}\r\n")?;
        }
        let count = sent.map_or(0, |(count, _)| count) + 1;
        let what = WAITING_ON.get();
        let event = match &self.progress_token {
            Some(token) => {
                let params = json!({ "progressToken": token, "progress": count, "message": what });
                format!("event: message\ndata: {}\n\n", to_json(&json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": params })))
            }
            None => format!(": {}\n\n", what.to_lowercase()),
        };
        *sent = Some((count, Instant::now()));
        http::write_chunk(&mut out, event.as_bytes())
    }
}

fn answer(message: &Value, caller: &Caller, call: impl FnOnce(&Value) -> Value) -> Option<String> {
    let id = message.get("id").filter(|id| !id.is_null())?;
    let ok = |result: Value| Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result })));
    let method = message.get("method").map_or(String::new(), julia_string);
    match method.as_str() {
        "initialize" => {
            let requested = message["params"]["protocolVersion"].as_str();
            let version = requested.filter(|v| SUPPORTED_VERSIONS.contains(v)).unwrap_or(SUPPORTED_VERSIONS[0]);
            let mut result = json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "endeavor-runtime", "version": env!("CARGO_PKG_VERSION") },
            });
            if caller.views {
                result["capabilities"]["resources"] = json!({});
            }
            if let Some(instructions) = guide::instructions(!caller.app, caller.has_skills, caller.front, caller.views) {
                result["instructions"] = instructions.into();
            }
            ok(result)
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let mut tools = NOTEBOOK_TOOLS.as_array().cloned().unwrap_or_default();
            if !caller.has_skills {
                tools.insert(0, guide::schema());
            }
            if !caller.host.is_empty() || caller.front {
                tools.extend(host_tools::schemas());
            }
            if caller.front {
                tools.extend(MACHINE_TOOLS.as_array().cloned().unwrap_or_default());
            }
            for tool in &mut tools {
                if caller.no_folder {
                    let described = match tool["name"].as_str() {
                        Some("new_notebook") => Some(NO_FOLDER_NEW),
                        Some("open_notebook") => Some(NO_FOLDER_OPEN),
                        _ => None,
                    };
                    if let Some(described) = described {
                        tool["inputSchema"]["properties"]["path"]["description"] = described.into();
                    }
                }
                let read_only = !tool["name"].as_str().is_some_and(|name| WRITE_TOOLS.contains(&name) || name == "open_notebook" || (MACHINE_NAMES.contains(&name) && name != "list_machines"));
                tool["annotations"] = json!({ "readOnlyHint": read_only });
            }
            if caller.views {
                crate::views::add_to(&mut tools);
            }
            ok(json!({ "tools": tools }))
        }
        "resources/list" if caller.views => ok(crate::views::resources_list()),
        "resources/read" if caller.views => match crate::views::resources_read(&message["params"]) {
            Some(read) => ok(read),
            None => Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32002, "message": "Resource not found" } }))),
        },
        "tools/call" => ok(call(&message["params"])),
        _ => Some(method_not_found(id, &method)),
    }
}

const NO_FOLDER_NEW: &str = "Where to create it: an absolute path ending in `.jl` for a Julia notebook, `.R` for an R notebook or `.py` for a Python (marimo) notebook (R and Python not on Windows). The file must not exist yet and its folder must. \
Required on this computer: this server was not told the project folder, so it can't choose one unless the client names it with the call, as Codex does. \
A relative path starts in that folder, and without it is refused. \
On a server after `use_machine`, a relative path starts in the session's folder there.";
const NO_FOLDER_OPEN: &str = "The notebook file, as an absolute path: this server was not told the project folder. \
A relative path starts in the project folder when the client names that folder with the call, as Codex does; otherwise it is refused. \
On a server after `use_machine`, a relative path starts in the session's folder there.";

pub(crate) fn answer_unreachable(message: &Value, request: &Head, why: &str) -> Option<String> {
    answer(message, &Caller::of(request, true), |_| json!({ "content": [{ "type": "text", "text": why }], "isError": true }))
}

pub(crate) fn answer_locally(message: &Value, has_skills: bool, no_folder: bool, views: bool) -> Option<String> {
    let local = matches!(message["method"].as_str(), Some("initialize" | "ping" | "tools/list" | "resources/list" | "resources/read"));
    let caller = Caller { has_skills, front: true, no_folder, views, ..Caller::default() };
    local.then(|| answer(message, &caller, |_| unreachable!("tools/call isn't answered locally"))).flatten()
}

const MISUSE_KINDS: [&str; 14] = [
    "invalid_argument", "unknown_tool", "cell_not_found", "notebook_not_found",
    "invalid_cell_id", "invalid_notebook_id", "invalid_order", "invalid_path",
    "read_required", "stale_read", "placement_required", "not_staged", "one_notebook", "run_conflict",
];

const ERRORS_TOPIC_KINDS: [&str; 6] = ["invalid_argument", "cell_not_found", "notebook_not_found", "invalid_path", "placement_required", "not_staged"];
const ERRORS_TOPIC_HINT: &str = "See `notebook_guide` with `topic` set to `endeavor-notebooks/reference/errors.md` for what this error means and what to do.";

const STARTING_KINDS: [&str; 4] = ["julia_starting", "r_installing", "python_installing", "notebook_opening"];

pub(crate) fn image_result(meta: &Value, png_base64: String) -> Value {
    json!({
        "content": [
            { "type": "text", "text": to_json(meta) },
            { "type": "image", "data": png_base64, "mimeType": "image/png" },
        ],
        "isError": false,
    })
}

pub(crate) fn tool_error(raw: &str, help: bool) -> Value {
    let unwrapped = unwrap_key_error(raw);
    let raw = unwrapped.as_deref().unwrap_or(raw);
    let (kind, message) = match raw.split_once("::") {
        Some((kind, message)) => {
            let kind = kind.trim();
            (kind.rsplit(':').next().unwrap_or_default().trim(), message)
        }
        None => ("tool_error", raw),
    };
    let message = if help && ERRORS_TOPIC_KINDS.contains(&kind) {
        format!("{message}\n{ERRORS_TOPIC_HINT}")
    } else if help && MISUSE_KINDS.contains(&kind) {
        format!("{message}\nSee `notebook_guide` for how to use these tools.")
    } else {
        message.to_owned()
    };
    let text = to_json(&json!({ "error": kind, "message": message }));
    json!({ "content": [{ "type": "text", "text": text }], "isError": !STARTING_KINDS.contains(&kind) })
}

fn unwrap_key_error(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix("KeyError: key \"")?.strip_suffix("\" not found")?;
    let unescaped = unescape_julia_repr(inner);
    unescaped.contains("::").then_some(unescaped)
}

fn unescape_julia_repr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(escaped) => out.push(escaped),
            None => out.push('\\'),
        }
    }
    out
}

pub fn julia_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "nothing".into(),
        other => other.to_string(),
    }
}

pub fn to_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value);
    out
}

fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_json(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::String(text) => write_string(out, text),
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push_str(&Value::from(text).to_string().replace('\x7f', "\\u007f"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_link_without_the_token_and_the_one_that_lets_a_browser_in() {
        assert_eq!(super::browser_link(9, "/edit?id=a"), "http://localhost:9/edit?id=a");
        assert_eq!(super::entry_link("http://localhost:9/edit?id=a", "t"), "http://localhost:9/edit?id=a&token=t");
        assert_eq!(super::entry_link("http://localhost:9/", "t"), "http://localhost:9/?token=t");
        assert_eq!(super::without_token("http://localhost:9/edit?id=a&token=t"), "http://localhost:9/edit?id=a");
        assert_eq!(super::without_token("http://localhost:9/?token=t"), "http://localhost:9/");
        assert_eq!(super::without_token("http://localhost:9/edit?token=t&id=a"), "http://localhost:9/edit?id=a");
        assert_eq!(super::without_token("http://localhost:9/edit?id=a"), "http://localhost:9/edit?id=a");
    }

    #[test]
    fn an_open_waits_less_for_the_users_answer_than_a_run_so_the_open_after_it_fits_in_a_minute() {
        use std::time::Duration;
        let (open, run) = (super::ask_wait("new_notebook"), super::ask_wait("execute_cell"));
        assert_eq!(super::ask_wait("open_notebook"), open);
        assert!(open + Duration::from_secs(25) < Duration::from_secs(50), "{open:?}");
        assert!(run > open && run + Duration::from_secs(5) < Duration::from_secs(55), "{run:?}");
    }

    #[test]
    fn reads_leave_an_ask_left_up_and_changes_or_runs_take_it_down() {
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        assert!(!super::acts("read_cell", &cell) && !super::acts("open_notebook", &json!({ "path": "a.jl" })));
        assert!(super::acts("edit_cell", &cell) && super::acts("execute_cell", &cell));
        assert!(super::acts("open_notebook", &json!({ "path": "a.jl", "run_notebook": true })));
    }

    #[test]
    fn a_held_call_says_it_waits_for_the_user_only_while_it_does() {
        let now = || WAITING_ON.get();
        assert_eq!(now(), "Still working on the call", "a wait with no words of its own, such as an engine starting");
        let inner = waiting_on(Waits::CELLS, &|| {
            assert_eq!(now(), "Waiting for the cells to finish");
            waiting_on(Waits::ANSWER, &|| now() == "Waiting for the user's answer")
        });
        assert!(inner, "a question asked during a run says so while it is asked");
        assert_eq!(now(), "Still working on the call", "and the words go back after the wait");
    }

    #[test]
    fn an_unanswered_call_says_how_to_keep_waiting_and_what_to_do_after_stopping() {
        let text = super::unanswered("new_notebook");
        assert!(text.starts_with("ArgumentError: waiting_for_user::"), "{text}");
        assert!(text.contains("call `new_notebook` again with the same arguments, and") && text.contains("When they write back, call `new_notebook` again"), "{text}");
    }

    use super::*;

    fn tools_fingerprint() -> String {
        fn without_descriptions(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove("description");
                    map.values_mut().for_each(without_descriptions);
                }
                Value::Array(items) => items.iter_mut().for_each(without_descriptions),
                _ => {}
            }
        }
        let mut tools: Vec<Value> = NOTEBOOK_TOOLS.as_array().unwrap().iter().chain(crate::views::schemas().iter()).map(|tool| json!({ "name": tool["name"], "inputSchema": tool["inputSchema"] })).collect();
        tools.iter_mut().for_each(without_descriptions);
        let digest = <sha2::Sha256 as sha2::Digest>::digest(serde_json::to_string(&tools).unwrap());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn only_a_refusal_of_an_r_path_by_a_core_without_r_notebooks_is_one_for_want_of_them() {
        let refused = |text: &str| json!({ "content": [{ "type": "text", "text": text }], "isError": true });
        let old = refused(r#"{"error":"invalid_path","message":"Notebook path must end in .jl: '/w/a.R'"}"#);
        let r = json!({ "path": "/w/a.R" });
        assert!(refuses_r_for_want_of_it("new_notebook", &r, &old));
        assert!(refuses_r_for_want_of_it("open_notebook", &json!({ "path": "/w/a.r" }), &old));
        assert!(!refuses_r_for_want_of_it("new_notebook", &json!({ "path": "/w/a.jl" }), &old), "not an R path");
        assert!(!refuses_r_for_want_of_it("move_notebook", &r, &old), "not a new or opened notebook");
        assert!(!refuses_r_for_want_of_it("new_notebook", &json!({}), &old), "no path");
        let now = refused(r#"{"error":"invalid_path","message":"Notebook path must end in .jl (Julia) or .R (R): '/w/a.Rx'"}"#);
        assert!(!refuses_r_for_want_of_it("new_notebook", &r, &now), "a core with R notebooks");
        assert!(!refuses_r_for_want_of_it("new_notebook", &r, &refused("unsupported::R notebooks aren't running in this runtime")));
        assert!(!refuses_r_for_want_of_it("new_notebook", &r, &json!({ "content": [{ "type": "text", "text": "must end in .jl" }], "isError": false })), "not an error");
    }

    #[test]
    fn a_python_notebook_is_one_by_its_path_or_for_a_new_one_by_the_sessions_kind() {
        use wire::backend::Backend;
        assert!(opens_python("open_notebook", &json!({ "path": "/w/a.py" }), None));
        assert!(!opens_python("open_notebook", &json!({ "path": "/w/a.jl" }), Some(Backend::Marimo)));
        assert!(opens_python("new_notebook", &json!({}), Some(Backend::Marimo)));
        assert!(!opens_python("new_notebook", &json!({}), Some(Backend::Ember)));
        assert!(!opens_python("new_notebook", &json!({ "path": "/w/a.R" }), Some(Backend::Marimo)), "the path decides");
        assert!(!opens_python("read_cell", &json!({ "path": "/w/a.py" }), None));
    }

    #[test]
    fn the_notebook_tools_names_and_arguments_are_those_of_the_cores_interface() {
        assert_eq!((crate::core::INTERFACE, tools_fingerprint().as_str()), (49, "31806aaaff1c73b0"), "see the comment in this test");
    }

    fn code(source: &str) -> &str {
        source.split("#[cfg(test)]").next().unwrap()
    }

    fn quoted_names(code: &str, prefix: &str) -> Vec<String> {
        let opening = format!("\"{prefix}");
        let mut names: Vec<String> = code
            .match_indices(&opening)
            .filter_map(|(at, _)| {
                let rest = &code[at + 1..];
                let end = rest.find('"')?;
                let name = &rest[..end];
                name[prefix.len()..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_').then(|| name.to_owned())
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn record_fields() -> Vec<String> {
        let core = code(include_str!("core.rs"));
        let (from, to) = (core.find("let mut state = json!({").unwrap(), core.find("join(\"runtime.json\"), state").unwrap());
        let record = &core[from..to];
        let mut fields: Vec<String> = quoted_names(record, "").into_iter().filter(|name| record.contains(&format!("\"{name}\":")) || record.contains(&format!("state[\"{name}\"]"))).collect();
        fields.dedup();
        fields
    }

    #[test]
    fn the_app_calls_and_the_records_fields_are_those_of_the_cores_interface() {
        let calls: Vec<String> = quoted_names(code(include_str!("mcp.rs")), "endeavor/").iter().map(|call| call["endeavor/".len()..].to_owned()).collect();
        let calls_then = [
            "allow_julia_install", "allow_r_install", "allow_uv_install", "answer_run", "end_when_idle", "file_info", "julia_status", "keep_for_app", "leave_to_plugin", "move_notebook", "new_notebook", "r_status", "recent_sessions", "restart_notebook", "restore_cells", "run_preview", "set_folder",
            "set_idle_limit", "set_notebook", "set_policy", "set_session_folder", "shutdown", "start_julia", "stop_notebook", "stop_session", "tool_result", "use_julia",
        ];
        let fields_then = ["boot", "build", "exits_when_idle", "folder", "interface", "job", "launcher", "network", "no_folder", "node", "page_token", "pid", "port", "started", "token"];
        assert_eq!((crate::core::INTERFACE, calls, record_fields()), (49, calls_then.map(String::from).to_vec(), fields_then.map(String::from).to_vec()), "see the comment in this test");
    }

    #[test]
    fn writes_json_as_julia_does() {
        let value = json!({ "b": 1, "A": [true, null, 2.5], "a": { "z": "x\u{7f}\u{1}/\"é", "_": {} }, "aa": [] });
        assert_eq!(to_json(&value), r#"{"A":[true,null,2.5],"a":{"_":{},"z":"x\u007f\u0001/\"é"},"aa":[],"b":1}"#);
    }

    #[test]
    fn the_app_or_the_plugin_is_the_callers_not_the_runtimes() {
        let head = |client: &str| crate::http::Head::read(&mut format!("POST /mcp HTTP/1.1\r\nX-Endeavor-Session: s\r\n{client}\r\n").as_bytes()).unwrap().unwrap();
        for app_runtime in [true, false] {
            assert!(Caller::of(&head("X-Endeavor-Client: app\r\n"), app_runtime).app);
            assert!(!Caller::of(&head("X-Endeavor-Client: plugin\r\n"), app_runtime).app);
            assert_eq!(Caller::of(&head(""), app_runtime).app, app_runtime, "a caller that doesn't say is the starter's");
            assert!(Caller::of(&head("X-Endeavor-Client: plugin\r\n"), app_runtime).no_one_asks());
            assert!(!Caller::of(&head("X-Endeavor-Client: plugin\r\nX-Endeavor-Consent: front\r\n"), app_runtime).no_one_asks());
            assert!(!Caller::of(&head("X-Endeavor-Client: app\r\n"), app_runtime).no_one_asks());
            assert!(Caller::of(&head(""), app_runtime).no_one_asks());
        }
        let asking = to_json(&json!({ "text": format!("R has to download ALL. {} Don't call read_cell to wait.", notebooks::ASK_STEP) }));
        let said = no_one_asks_said(asking);
        assert!(said.contains(ASK_STEP_NO_ONE) && !said.contains(notebooks::ASK_STEP), "{said}");
        assert_eq!(no_one_asks_said("ran".into()), "ran");
        let instructions = |app: bool| {
            let caller = Caller { app, ..Caller::default() };
            let reply = answer(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }), &caller, |_| json!(null)).unwrap();
            serde_json::from_str::<Value>(&reply).unwrap()["result"]["instructions"].clone()
        };
        assert_eq!(instructions(true), json!(guide::instructions(false, false, false, false)));
        assert_eq!(instructions(false), json!(guide::instructions(true, false, false, false)));
        assert_ne!(instructions(true), instructions(false));
    }

    #[test]
    fn a_front_without_a_folder_lists_the_same_tools_with_two_path_descriptions_replaced() {
        let list = |no_folder: bool| {
            let caller = Caller { has_skills: true, front: true, no_folder, ..Caller::default() };
            let reply = answer(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }), &caller, |_| json!(null)).unwrap();
            serde_json::from_str::<Value>(&reply).unwrap()["result"]["tools"].clone()
        };
        let (with, without) = (list(false), list(true));
        let mut changed = Vec::new();
        for (a, b) in with.as_array().unwrap().iter().zip(without.as_array().unwrap()) {
            if a != b {
                let (before, after) = (a["inputSchema"]["properties"]["path"]["description"].as_str().unwrap(), b["inputSchema"]["properties"]["path"]["description"].as_str().unwrap());
                assert!(before.contains("a relative path is inside the session's folder"), "{before}");
                assert!(after.contains("absolute path") && after.contains("this server was not told the project folder") && !after.contains("is inside the session's folder") && !after.contains("generated"), "{after}");
                let (mut a, mut b) = (a.clone(), b.clone());
                a["inputSchema"]["properties"]["path"]["description"] = Value::Null;
                b["inputSchema"]["properties"]["path"]["description"] = Value::Null;
                assert_eq!(a, b, "nothing else about the tool differs");
                changed.push(a["name"].as_str().unwrap().to_owned());
            }
        }
        assert_eq!(with.as_array().unwrap().len(), without.as_array().unwrap().len());
        assert_eq!(changed, ["open_notebook", "new_notebook"]);
    }

    #[test]
    fn an_agent_without_the_plugin_is_told_to_read_the_guide() {
        let ask = |method: &str, has_skills: bool| {
            let caller = Caller { has_skills, app: true, ..Caller::default() };
            let reply = answer(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }), &caller, |_| json!(null)).unwrap();
            serde_json::from_str::<Value>(&reply).unwrap()["result"].clone()
        };
        let names = |result: Value| result["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(
            ask("initialize", false)["instructions"],
            "These tools edit and run a live notebook, in Julia (Pluto) or R (Ember), that the user sees in Endeavor, next to this chat. \
Before your first notebook tool call in a session, call `notebook_guide` once with no arguments and follow what it says: \
how to find this session's notebook, the read-edit-run loop, and the rules for a cell. \
In your replies, link each cell you talk about the first time it comes up, as `[a few words](notebook://pluto/{notebook_id}/cell/{cell_id})` \
(`notebook://ember/...` in R), with the cell's real id: the user clicks it to scroll the notebook to that cell. \
Start a code cell that defines no name, such as a plot or a check, with a `# ` comment of at most 28 characters, in words with no code, saying what it does; the app names the cell by it. The rules for runs the user must approve are in the topic `endeavor-notebooks/reference/app.md`."
        );
        assert_eq!(names(ask("tools/list", false))[0], "notebook_guide");
        assert_eq!(ask("tools/list", false)["tools"][0]["annotations"]["readOnlyHint"], true);

        let told = ask("initialize", true)["instructions"].as_str().unwrap().to_owned();
        assert!(told.starts_with("Before your first notebook tool call in a session, load the `endeavor-notebooks` skill") && !told.contains("notebook_guide"), "Claude Code has the plugin's skills: {told}");
        assert!(!names(ask("tools/list", true)).contains(&"notebook_guide".to_owned()));
        assert!(is_tool("notebook_guide") && is_tool("edit_cell") && is_tool("run_shell") && !is_tool("edit"));
    }

    #[test]
    fn arguments_are_checked_by_name_against_the_tools_schema() {
        let said = |tool: &str, arguments: Value| match check_arguments(tool, &arguments, false) {
            Ok(()) => None,
            Err(result) => Some(serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()),
        };
        let message = |tool: &str, arguments: Value| said(tool, arguments).map(|said| (said["error"].as_str().unwrap().to_owned(), said["message"].as_str().unwrap().to_owned()));
        let invalid = |text: &str| Some(("invalid_argument".to_owned(), text.to_owned()));
        assert_eq!(message("new_notebook", json!({ "name": "remote.jl" })), invalid("`name` is not an argument of `new_notebook`. Its arguments: `path`."));
        assert_eq!(message("list_notebooks", json!({ "input": "" })), None, "a tool with no properties ignores what it is given");
        assert_eq!(message("list_machines", json!({ "input": "" })), None);
        assert_eq!(message("add_cell", json!({ "notebook_id": "n" })), None, "an empty cell is a call");
        assert_eq!(message("read_cell", json!({ "notebook_id": "n", "cell_id": "c", "a": 1, "b": 2 })).unwrap().1.split(". ").next(), Some("`a`, `b` are not arguments of `read_cell`"));
        assert_eq!(message("edit_cell", json!({ "code": "1" })), invalid("`edit_cell` needs `notebook_id`, `cell_id`."));
        assert_eq!(message("use_machine", json!({})), invalid("`use_machine` needs `machine`."));
        assert_eq!(message("run_shell", json!({ "cmd": "ls" })).unwrap().1.split(". ").next(), Some("`cmd` is not an argument of `run_shell`"));
        assert_eq!(message("notebook_guide", json!({ "page": "x" })), invalid("`page` is not an argument of `notebook_guide`. Its arguments: `topic`."));
        assert_eq!(message("read_cell", json!({ "notebook_id": null, "cell_id": 3 })), None, "types are the tool's to check");
        assert_eq!(message("new_notebook", json!({})), None);
        assert_eq!(message("no_such_tool", json!({ "x": 1 })), None, "an unknown tool is `unknown_tool`'s");
        let guided = check_arguments("edit_cell", &json!({}), true).unwrap_err();
        assert!(guided["content"][0]["text"].as_str().unwrap().contains("See `notebook_guide`"));
        let host = check_arguments("run_shell", &json!({}), true).unwrap_err();
        assert!(!host["content"][0]["text"].as_str().unwrap().contains("notebook_guide"), "the guide is for the notebook tools");
    }

    #[test]
    fn missing_or_null_arguments_are_none_and_a_tool_is_known_from_its_schema() {
        let call = |params: Value, machines: bool| checked_call(&params, false, machines).map(|(name, arguments)| (name.to_owned(), arguments)).map_err(|result| result["content"][0]["text"].as_str().unwrap().to_owned());
        for params in [json!({ "name": "list_notebooks" }), json!({ "name": "list_notebooks", "arguments": null })] {
            assert_eq!(call(params, false), Ok(("list_notebooks".to_owned(), json!({}))));
        }
        assert_eq!(call(json!({ "name": "list_machines", "arguments": null }), true), Ok(("list_machines".to_owned(), json!({}))));
        assert!(call(json!({ "name": "list_notebooks", "arguments": 5 }), false).unwrap_err().contains("arguments must be an object"));
        assert!(call(json!({ "name": "list_notebooks", "arguments": [] }), false).unwrap_err().contains("arguments must be an object"));
        assert!(call(json!({ "name": "list_machines" }), false).unwrap_err().contains("Unknown tool: 'list_machines'"), "a runtime has no machine tools");
        assert!(call(json!({ "name": "edit_cell", "arguments": {} }), false).unwrap_err().contains("needs `notebook_id`"));
        assert!(call(json!({ "arguments": {} }), true).unwrap_err().contains("Unknown tool: ''"));
        assert!(is_tool("run_shell") && is_tool("notebook_guide") && !is_tool("use_machine"));
    }

    #[test]
    fn a_label_is_printable_trimmed_and_short() {
        assert_eq!(clean_label("  Claude Code on jc-workstation \r\n"), Some("Claude Code on jc-workstation".into()));
        assert_eq!(clean_label("a\u{7}b\tc"), Some("abc".into()));
        assert_eq!(clean_label(" \n\u{7}"), None);
        assert_eq!(clean_label(&"é".repeat(100)), Some("é".repeat(80)));
        assert_eq!(clean_label(&format!("{} z", "y".repeat(79))), Some("y".repeat(79)), "no trailing space after the cut");
    }

    #[test]
    fn only_calls_that_run_code_count_as_runs() {
        let runs = |tool: &str, arguments: Value| runs_code(tool, &arguments);
        for tool in ["execute_cell", "submit_changes", "run_all_cells", "allow_execution", "delete_cell", "run_shell"] {
            assert!(runs(tool, json!({})), "{tool}");
        }
        assert!(runs("add_cell", json!({ "code": "1", "run_after": true })));
        assert!(runs("edit_cell", json!({ "code": "1", "run_after": true })));
        assert!(!runs("add_cell", json!({ "code": "1" })));
        assert!(!runs("edit_cell", json!({ "code": "1", "run_after": false })));
        assert!(!runs("edit_cells", json!({ "cells": [] })));
        assert!(runs("open_notebook", json!({ "path": "/x.jl", "run_notebook": true })));
        assert!(!runs("open_notebook", json!({ "path": "/x.jl" })));
        assert!(!runs("open_notebook", json!({ "path": "/x.jl", "run_notebook": false })));
        assert!(!runs("read_cell", json!({})));
        assert!(!runs("read_file", json!({ "path": "/tmp/x" })));
    }

    #[test]
    fn manual_holds_every_write_and_no_read() {
        let held = |tool: &str, policy: &str, edits: bool| asks_first(tool, &json!({ "cell_id": "a", "code": "1" }), policy, edits);
        for tool in WRITE_TOOLS.iter().filter(|tool| **tool != "interrupt_notebook") {
            assert!(held(tool, "ask", true), "{tool} in Manual");
        }
        assert!(!held("interrupt_notebook", "ask", true), "stopping a run never asks");
        for tool in ["read_cell", "read_notebook_code", "list_notebooks", "view_cell_output", "search_code", "open_notebook", "keep_notebook_alive", "read_file"] {
            assert!(!held(tool, "ask", true) && !held(tool, "auto", true), "{tool} only reads");
        }
        assert!(!held("edit_cell", "ask", false), "Ask to run doesn't hold an edit that doesn't run");
        assert!(held("execute_cell", "ask", false));
        let open = |run: bool, policy: &str| asks_first("open_notebook", &json!({ "path": "/x.jl", "run_notebook": run }), policy, false);
        assert!(open(true, "ask") && !open(false, "ask") && !open(true, "auto") && !open(true, "plan"), "opening asks only when it runs the notebook");
        assert!(held("move_cell", "auto", true) && !held("execute_cell", "auto", true), "Manual with runs allowed still asks before changes");
        assert!(!held("edit_cell", "plan", true), "plan refuses it instead");
        for (policy, edits) in [("ask", false), ("ask", true), ("auto", false), ("auto", true)] {
            assert!(held("approve_install", policy, edits), "a large download asks in {policy} (edits {edits})");
        }
        assert!(!held("approve_install", "plan", false));
    }

    #[test]
    fn reads_errors_as_julia_did() {
        let text = |raw: &str| tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(text("ArgumentError: not_found::No folder at /x::y"), r#"{"error":"not_found","message":"No folder at /x::y"}"#);
        assert_eq!(text("SystemError: opening file \"/x\": Permission denied"), r#"{"error":"tool_error","message":"SystemError: opening file \"/x\": Permission denied"}"#);
        assert_eq!(text("IOError: readdir(\"/a::b\"): denied"), r#"{"error":"readdir(\"/a","message":"b\"): denied"}"#);
        assert_eq!(tool_error("x", false)["isError"], true);
        let starting = tool_error("julia_starting::Julia 1.12.6 is starting and loading Pluto (40 seconds so far).", false);
        assert_eq!(starting["isError"], false);
        assert_eq!(text("julia_starting::Julia is starting"), r#"{"error":"julia_starting","message":"Julia is starting"}"#);
        assert_eq!(tool_error("r_installing::Installing Ember", false)["isError"], false);
        assert_eq!(tool_error("python_installing::Setting up Python notebooks", false)["isError"], false);
        assert_eq!(tool_error("notebook_opening::Opening", false)["isError"], false);
        assert_eq!(tool_error("julia_failed::It stopped", false)["isError"], true);
    }

    #[test]
    fn a_wrapped_key_error_unwraps_to_its_own_kind_and_message() {
        let text = |raw: &str| tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(
            text("KeyError: key \"notebook_not_found::No notebook with id 'x' in the current session. Run list_notebooks to see what's open.\" not found"),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session. Run list_notebooks to see what's open."}"#
        );
        assert_eq!(text("KeyError: key \"code\" not found"), r#"{"error":"tool_error","message":"KeyError: key \"code\" not found"}"#);
    }

    #[test]
    fn a_misuse_error_points_to_the_guide_only_when_asked_to_help() {
        let text = |raw: &str, help: bool| tool_error(raw, help)["content"][0]["text"].as_str().unwrap().to_owned();
        let notebook_not_found = "ArgumentError: notebook_not_found::No notebook with id 'x' in the current session.";
        assert_eq!(
            text(notebook_not_found, true),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session.\nSee `notebook_guide` with `topic` set to `endeavor-notebooks/reference/errors.md` for what this error means and what to do."}"#,
            "an agent without the plugin, on a kind the errors topic explains"
        );
        assert!(
            text("ArgumentError: stale_read::Cell c1 changed since you read it.", true).ends_with(r#"\nSee `notebook_guide` for how to use these tools."}"#),
            "a kind the guide itself explains"
        );
        assert_eq!(
            text(notebook_not_found, false),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session."}"#,
            "an agent with its own skills gets no pointer to this server's guide"
        );
        let crashed = "ArgumentError: process_exited::The notebook's Julia process exited while running a cell";
        assert_eq!(text(crashed, true), text(crashed, false), "not an agent misuse kind");
        assert!(!text(crashed, true).contains("notebook_guide"));
    }
}

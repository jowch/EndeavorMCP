//! The agent's MCP connection, MCP over Streamable HTTP (spec 2025-06-18):
//! `POST /mcp` carries one JSON-RPC message; a request gets its reply in the
//! same response, a notification or a response from the client gets `202
//! Accepted` with no body. Every tool call is request and reply, so this
//! server never needs to stream a reply back, and issues no `Mcp-Session-Id`
//! (optional in the spec; the adapter's MCP client doesn't send one back when
//! none is issued). Each agent session's messages carry `X-Endeavor-Session`
//! (its key) and, on a server, `X-Endeavor-Host`. The notebook tools are
//! `notebooks`'; host tools are `host_tools`'; the guide to both is `guide`'s.
//!
//! Each session has a run policy the app sets ("plan" | "ask" | "auto"). In
//! "plan" its notebook writes and runs are refused. In "ask", when the app
//! says it answers runs (`asks`), a call that runs code waits for the user's
//! answer (see `asks`); "auto" runs it. In Manual the app also says `edits`:
//! then a call that changes the notebook waits for the user's answer too,
//! whatever the policy says about runs.

use std::collections::HashMap;
use std::io::{self, BufReader};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};

use crate::asks::{Answer, Ask, Outcome};
use crate::guide;
use crate::host_tools;
use crate::http::{self, Head};
use crate::notebooks::{self, Julia, Notebooks, Reply};
use crate::results::Results;

/// Protocol versions this server understands, most recent first: `initialize`
/// echoes the client's requested version when it's one of these, else answers
/// the first (standard negotiation); `MCP-Protocol-Version` may name any of
/// these (a client that hasn't negotiated yet, before `initialize`, sends no
/// header).
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The notebook tools' schemas, for `tools/list`.
static NOTEBOOK_TOOLS: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(include_str!("notebook_tools.json")).expect("notebook_tools.json"));

/// `/call` methods Julia still answers: Pluto's folder for new notebooks, and
/// ending the process.
const JULIA_CALLS: [&str; 2] = ["endeavor/set_folder", "endeavor/shutdown"];

/// Whether this server offers a tool by this name, to some session.
pub fn is_tool(name: &str) -> bool {
    name == guide::TOOL || host_tools::NAMES.contains(&name) || NOTEBOOK_TOOLS.as_array().is_some_and(|tools| tools.iter().any(|t| t["name"] == name))
}

/// Tools that change the notebook or run code, here or on the server.
pub const WRITE_TOOLS: [&str; 12] = [
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution", "run_shell",
];

/// Whether a call to `tool` with `arguments` runs code: the calls Ask to run
/// asks about first.
pub fn runs_code(tool: &str, arguments: &Value) -> bool {
    match tool {
        // delete_cell re-runs the deleted cell's dependents (and can't be undone).
        "execute_cell" | "submit_changes" | "run_all_cells" | "allow_execution" | "delete_cell" => true,
        // A shell command on a session's server.
        "run_shell" => true,
        "add_cell" | "edit_cell" => arguments["run_after"].as_bool() == Some(true),
        _ => false,
    }
}

/// Whether a call to `tool` changes the notebook: what Manual asks about
/// even when runs don't ask.
pub fn changes_notebook(tool: &str) -> bool {
    matches!(tool, "edit_cell" | "edit_cells" | "add_cell" | "delete_cell" | "move_cell" | "fold_cell" | "new_notebook")
}

/// Whether the runtime holds a call to `tool` with `arguments` for the
/// user's answer, in a session whose policy is `policy` and whose edits ask
/// (`edits`, Manual). Nothing is held in "plan", which refuses writes.
pub fn asks_first(tool: &str, arguments: &Value, policy: &str, edits: bool) -> bool {
    policy != "plan" && ((policy == "ask" && runs_code(tool, arguments)) || (edits && changes_notebook(tool)))
}

/// A session's policy as the app set it.
struct Policy {
    policy: String,
    /// The app answers what the runtime holds (an app from before runtime asks doesn't).
    asks: bool,
    /// Calls that change the notebook ask too (Manual).
    edits: bool,
}

/// What every client connection shares.
pub struct Bridge {
    pub julia: Arc<Julia>,
    pub notebooks: Arc<Notebooks>,
    pub token: String,
    /// Each agent session's policy, by its key.
    policies: Mutex<HashMap<String, Policy>>,
    /// Each agent session's working folder on this machine, by its key.
    folders: Mutex<HashMap<String, String>>,
    /// Each agent session's last tool results, for the app to look up.
    results: Results,
    /// `run_shell`'s environment, changed from ours as Julia's was: its
    /// depot, and none of what the helper passes the runtime.
    shell_env: Vec<(&'static str, Option<String>)>,
}

/// Who sent a message: the agent session's key and the server it works on,
/// both empty for the app.
#[derive(Default)]
pub struct Caller {
    pub owner: String,
    pub host: String,
    /// The agent loads Endeavor's skills itself (Claude Code's plugin), so it
    /// gets no guide.
    pub has_skills: bool,
}

impl Caller {
    fn of(request: &Head) -> Caller {
        let header = |name| request.header(name).unwrap_or_default().to_owned();
        Caller { owner: header("X-Endeavor-Session"), host: header("X-Endeavor-Host"), has_skills: header("X-Endeavor-Skills") == "plugin" }
    }
}

impl Bridge {
    /// `depot` is Julia's JULIA_DEPOT_PATH.
    pub fn new(token: String, depot: &str) -> Bridge {
        let shell_env = vec![
            ("JULIA_DEPOT_PATH", Some(depot.to_owned())),
            ("ENDEAVOR_TOKEN", None),
            ("ENDEAVOR_STATE", None),
            ("ENDEAVOR_LAUNCHER", None),
            ("ENDEAVOR_BUILD", None),
        ];
        let julia = Arc::new(Julia::new(token.clone()));
        let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        Bridge {
            notebooks: Arc::new(Notebooks::new(julia.clone(), Box::new(clock))),
            julia,
            token,
            policies: Mutex::default(),
            folders: Mutex::default(),
            results: Results::default(),
            shell_env,
        }
    }

    /// The reply to one of the app's `/call`s, if the core answers it; `None`
    /// passes it to Julia's `/call`. Only the app calls this route, so its
    /// tool calls have no caller.
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
                self.policies.lock().unwrap().insert(owner, Policy { policy, asks, edits });
            }
            "endeavor/answer_run" => {
                let given = Answer { allow: params["allow"] == true, user_ran: params.get("user_ran").cloned().unwrap_or_else(|| json!([])) };
                let id = params["id"].as_u64().ok_or_else(|| "ArgumentError: invalid_argument::id must be a number".to_owned());
                return Some(answer(id.and_then(|id| self.notebooks.asks.answer(id, given)).map(|()| json!({}))));
            }
            "endeavor/set_notebook" => {
                let (owner, notebook) = (text("owner", ""), params.get("notebook").filter(|n| !n.is_null()).map_or(String::new(), julia_string));
                self.notebooks.bind(&owner, &notebook);
                eprintln!("[ Info: Session {owner} notebook: {}", if notebook.is_empty() { "(none)" } else { &notebook });
            }
            "endeavor/set_idle_limit" => {
                let hours = match params.get("hours") {
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(48.0),
                    Some(Value::Bool(b)) => *b as u8 as f64,
                    _ => 48.0,
                };
                self.notebooks.set_idle_limit(hours);
                let shown = params.get("hours").map_or("48".into(), julia_string);
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
                let mut folders = self.folders.lock().unwrap();
                if folder.is_empty() {
                    folders.remove(&owner);
                } else {
                    folders.insert(owner, folder);
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
            "endeavor/restart_notebook" => return Some(answer(self.notebooks.restart(&text("notebook_id", "")))),
            "endeavor/move_notebook" => return Some(answer(self.notebooks.move_notebook(&text("notebook_id", ""), &text("path", "")))),
            "endeavor/file_info" => return Some(answer(notebooks::file_info(&text("path", "")))),
            "endeavor/new_notebook" => {
                let owner = text("owner", "");
                let folder = self.folders.lock().unwrap().get(&owner).cloned();
                return Some(answer(self.notebooks.new_for(&owner, folder.as_deref())));
            }
            method if JULIA_CALLS.contains(&method) => return None,
            _ => return Some(self.dispatch(&message, &Caller::default(), &|| false).unwrap_or_else(|| "{}".into())),
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })))
    }

    /// Serve `POST /mcp`. Whether the connection can carry another request.
    pub fn mcp(&self, request: &Head, reader: &mut BufReader<TcpStream>, client: &mut TcpStream) -> io::Result<bool> {
        let socket = client.try_clone()?;
        let gone = move || closed(&socket);
        post(request, reader, client, request.keeps_alive(), |message| self.dispatch(message, &Caller::of(request), &gone))
    }

    /// The reply to one JSON-RPC message, if it gets one. `gone`: whether the
    /// client hung up, for a call that waits on the user.
    fn dispatch(&self, message: &Value, caller: &Caller, gone: &dyn Fn() -> bool) -> Option<String> {
        if message["method"] == "notifications/cancelled" && self.notebooks.asks.cancel(&caller.owner, &message["params"]["requestId"]) {
            self.notebooks.publish();
        }
        answer(message, caller, |params| {
            let call = Call { caller, request: &message["id"], call_id: params["_meta"]["claudecode/toolUseId"].as_str(), gone };
            let result = self.call_tool(params, &call);
            if !caller.owner.is_empty() {
                let arguments = params.get("arguments").unwrap_or(&Value::Null);
                self.results.record(&caller.owner, call.call_id, params["name"].as_str().unwrap_or_default(), arguments, &result);
            }
            self.notebooks.publish();
            result
        })
    }

    /// A `tools/call`'s result.
    fn call_tool(&self, params: &Value, call: &Call) -> Value {
        let caller = call.caller;
        let text = |result: &Value| json!({ "content": [{ "type": "text", "text": to_json(result) }], "isError": false });
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        // Whether a notebook-tool error is worth pointing at the guide: only an
        // agent without it to begin with, and only for a call it could retry
        // differently, not a host-tool mistake (its own tools, not these) or a
        // refusal that already says exactly what to do.
        let help = !caller.has_skills;
        if !arguments.is_object() {
            return tool_error("ArgumentError: invalid_argument::arguments must be an object", help);
        }
        let name = match params.get("name") {
            None => "",
            Some(Value::String(name)) => name.as_str(),
            Some(other) => return tool_error(&format!("ArgumentError: unknown_tool::Unknown tool: '{}'", julia_string(other)), help),
        };
        self.notebooks.note_activity(&arguments);
        if let Some(refusal) = self.refusal(caller, name) {
            return tool_error(&refusal, false);
        }
        if name == guide::TOOL {
            return match guide::read(&arguments) {
                Ok(guide) => json!({ "content": [{ "type": "text", "text": guide }], "isError": false }),
                Err(error) => tool_error(&error, false),
            };
        }
        if host_tools::NAMES.contains(&name) {
            if let Err(result) = self.ask_first(call, name, &arguments) {
                return result;
            }
            let folder = self.folders.lock().unwrap().get(&caller.owner).cloned();
            let env: Vec<_> = self.shell_env.iter().map(|(name, value)| (*name, value.as_deref())).collect();
            return match host_tools::call(name, &arguments, &host_tools::Shell { folder: folder.as_deref(), env: &env }) {
                Ok(result) => text(&result),
                Err(error) => tool_error(&error, false),
            };
        }
        if name == "keep_notebook_alive" {
            return self.notebooks.keep_alive(&arguments).map_or_else(|e| tool_error(&e, help), |r| text(&r));
        }
        if let Some(refusal) = self.notebooks.refusal(&caller.owner, name, &arguments) {
            return tool_error(&refusal, help);
        }
        let run = match self.ask_first(call, name, &arguments) {
            Ok(run) => run,
            Err(result) => return result,
        };
        let folder = self.folders.lock().unwrap().get(&caller.owner).cloned();
        let reply = if run {
            self.notebooks.tool(&caller.owner, name, &arguments, folder.as_deref())
        } else {
            self.notebooks.tool_unrun(&caller.owner, name, &arguments, folder.as_deref())
        };
        match reply {
            Ok(Reply::Json(result)) => text(&result),
            Ok(Reply::Image { meta, png_base64 }) => json!({
                "content": [
                    { "type": "text", "text": to_json(&meta) },
                    { "type": "image", "data": png_base64, "mimeType": "image/png" },
                ],
                "isError": false,
            }),
            Err(error) => tool_error(&error, help),
        }
    }

    /// Wait for the user's answer to a call the session's policy holds
    /// (`asks_first`). Whether it runs as asked; false for an edit the user
    /// didn't let run in Ask to run, which is made but not run. Otherwise the
    /// call's result: refused, cancelled, or with no app to ask.
    fn ask_first(&self, call: &Call, tool: &str, arguments: &Value) -> Result<bool, Value> {
        let owner = &call.caller.owner;
        let (held, edits) = match self.policies.lock().unwrap().get(owner) {
            Some(p) if p.asks => (asks_first(tool, arguments, &p.policy, p.edits), p.edits),
            _ => (false, false),
        };
        if owner.is_empty() || !held {
            return Ok(true);
        }
        if !self.notebooks.followed() {
            return Err(tool_error("ArgumentError: no_app::Endeavor isn't connected to ask the user about this. Try again once Endeavor is open.", false));
        }
        let ask = Ask { owner, call_id: call.call_id, request: call.request, tool, arguments, since: self.notebooks.now() };
        let id = self.notebooks.asks.add(ask);
        eprintln!("[ Info: Session {owner} asks before {tool} (ask {id}, call {})", call.call_id.unwrap_or("?"));
        self.notebooks.publish();
        let outcome = self.notebooks.asks.wait(id, call.gone);
        self.notebooks.publish();
        match outcome {
            Outcome::Answered(Answer { allow: true, user_ran }) => {
                let notebook = arguments.get("notebook_id").map(julia_string).unwrap_or_default();
                if user_ran.as_array().is_some_and(|cells| !cells.is_empty()) {
                    let _ = self.notebooks.run_anyway(&notebook, &user_ran);
                }
                Ok(true)
            }
            // In Manual a denied change isn't made, even one that was to run after.
            Outcome::Answered(_) if edits && changes_notebook(tool) => Err(tool_error("ArgumentError: not_approved::The user chose not to make this change.", false)),
            Outcome::Answered(_) if matches!(tool, "edit_cell" | "add_cell") => Ok(false),
            Outcome::Answered(_) => Err(tool_error("ArgumentError: not_approved::The user chose not to run this.", false)),
            Outcome::Cancelled | Outcome::Gone => Err(tool_error("ArgumentError: cancelled::The call was cancelled before the user answered.", false)),
        }
    }

    /// Why a session may not call `tool`, as the error Julia raised for it.
    fn refusal(&self, caller: &Caller, tool: &str) -> Option<String> {
        if host_tools::NAMES.contains(&tool) && caller.host.is_empty() {
            return Some(format!(
                "ArgumentError: host_tools::`{tool}` is only for sessions on a server. This session runs on the user's computer: use your own file and shell tools."
            ));
        }
        let plan = self.policies.lock().unwrap().get(&caller.owner).is_some_and(|p| p.policy == "plan");
        if plan && WRITE_TOOLS.contains(&tool) {
            let what = if tool == "run_shell" { "run a command on the server" } else { "change or run the notebook" };
            return Some(format!(
                "ArgumentError: plan_mode::Plan mode is read-only: `{tool}` would {what}. Finish the plan; the user switches modes to carry it out."
            ));
        }
        None
    }
}

/// One tool call: who made it, its JSON-RPC id, the id the agent's client
/// gave it (Claude Code's `_meta["claudecode/toolUseId"]`), and whether the
/// client has hung up.
struct Call<'a> {
    caller: &'a Caller,
    request: &'a Value,
    call_id: Option<&'a str>,
    gone: &'a dyn Fn() -> bool,
}

/// Whether the other end of `socket` has closed it, without reading from it.
fn closed(socket: &TcpStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: a one-byte peek into a local buffer; MSG_DONTWAIT keeps it from blocking.
    let n = unsafe { libc::recv(socket.as_raw_fd(), (&mut byte as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n == 0 || (n < 0 && !matches!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted))
}

/// Serve one `POST /mcp`: one JSON-RPC message in; a request gets `reply`'s
/// answer in this response, a notification or a response from the client gets
/// `202 Accepted` with no body. Whether the connection can carry another request.
pub(crate) fn post(
    request: &Head,
    reader: &mut BufReader<TcpStream>,
    client: &mut TcpStream,
    keep_alive: bool,
    reply: impl FnOnce(&Value) -> Option<String>,
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
    match reply(&message) {
        Some(reply) => http::respond(client, "200 OK", Some("application/json"), reply.as_bytes(), keep_alive)?,
        None => http::respond(client, "202 Accepted", None, b"", keep_alive)?,
    }
    Ok(keep_alive)
}

/// The reply to one JSON-RPC message, if it gets one; `call` gives a
/// `tools/call`'s result.
fn answer(message: &Value, caller: &Caller, call: impl FnOnce(&Value) -> Value) -> Option<String> {
    // Notifications get no reply.
    let id = message.get("id").filter(|id| !id.is_null())?;
    let ok = |result: Value| Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result })));
    let method = message.get("method").map_or(String::new(), julia_string);
    match method.as_str() {
        "initialize" => {
            // Standard negotiation: the client's version if we speak it, else our latest.
            let requested = message["params"]["protocolVersion"].as_str();
            let version = requested.filter(|v| SUPPORTED_VERSIONS.contains(v)).unwrap_or(SUPPORTED_VERSIONS[0]);
            let mut result = json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "endeavor-runtime", "version": env!("CARGO_PKG_VERSION") },
            });
            if !caller.has_skills {
                result["instructions"] = guide::INSTRUCTIONS.into();
            }
            ok(result)
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let mut tools = NOTEBOOK_TOOLS.as_array().cloned().unwrap_or_default();
            if !caller.has_skills {
                tools.insert(0, guide::schema());
            }
            if !caller.host.is_empty() {
                tools.extend(host_tools::schemas());
            }
            for tool in &mut tools {
                // MCP's read-only hint, what Claude Code's plan mode checks before prompting.
                let read_only = !tool["name"].as_str().is_some_and(|name| WRITE_TOOLS.contains(&name));
                tool["annotations"] = json!({ "readOnlyHint": read_only });
            }
            ok(json!({ "tools": tools }))
        }
        "tools/call" => ok(call(&message["params"])),
        _ => Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {method}") } }))),
    }
}

/// The reply to a JSON-RPC message while the app can't reach the runtime:
/// what the core would say, except that a tool call fails with `why`, plain
/// text Claude reads before trying again.
pub(crate) fn answer_unreachable(message: &Value, request: &Head, why: &str) -> Option<String> {
    answer(message, &Caller::of(request), |_| json!({ "content": [{ "type": "text", "text": why }], "isError": true }))
}

/// Kinds that mean the agent called a notebook tool the wrong way (a bad
/// argument, an unknown tool or cell, skipping the read-before-edit guard,
/// editing outside the session's one notebook) rather than hitting a runtime
/// problem in the user's Julia code, the Julia process, or the session's
/// policy. `notebook_guide` explains all of these.
const MISUSE_KINDS: [&str; 14] = [
    "invalid_argument", "unknown_tool", "cell_not_found", "notebook_not_found",
    "invalid_cell_id", "invalid_notebook_id", "invalid_order", "invalid_path",
    "read_required", "stale_read", "placement_required", "not_staged", "one_notebook", "run_conflict",
];

/// A failed tool call's result, from the text of the error Julia raised:
/// `ArgumentError: kind::message` names its kind; anything else is a
/// `tool_error`. `help`: whether to point a misuse kind at `notebook_guide`,
/// true only when the caller has no other way to learn it (no plugin) and
/// the call site is a notebook tool's own mistake, not a host tool's or a
/// refusal that already says what to do instead.
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
    let message = if help && MISUSE_KINDS.contains(&kind) {
        format!("{message}\nSee `notebook_guide` for how to use these tools.")
    } else {
        message.to_owned()
    };
    let text = to_json(&json!({ "error": kind, "message": message }));
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// A notebook or cell not found looks like a native Julia `KeyError`
/// (`notebooks::notebook_not_found`, `tools::key_error`), which wraps its own
/// `kind::message` inside the quoted key so the byte-for-byte error still
/// reads like Julia's: `KeyError: key "notebook_not_found::No notebook with
/// id '…'…" not found`. Splitting that whole thing on its first `::` lands
/// inside the wrapper, before the key's quote even closes. Unwrap the key
/// first, so the split lands on the `kind::message` it actually holds.
fn unwrap_key_error(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix("KeyError: key \"")?.strip_suffix("\" not found")?;
    let unescaped = unescape_julia_repr(inner);
    unescaped.contains("::").then_some(unescaped)
}

/// The inverse of `host_tools::julia_repr`'s escaping, for the text it quoted.
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
            Some(escaped) => out.push(escaped), // `\"`, `\\`, `\$`: the character itself
            None => out.push('\\'),
        }
    }
    out
}

/// A JSON value as Julia's `string` shows it.
pub fn julia_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "nothing".into(),
        other => other.to_string(),
    }
}

/// JSON the way Julia's JSON.jl writes it, byte for byte: object keys sorted
/// by their bytes, and DEL escaped. serde_json's own key order depends on a
/// feature another crate in the workspace turns on.
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
    use super::*;

    #[test]
    fn writes_json_as_julia_does() {
        let value = json!({ "b": 1, "A": [true, null, 2.5], "a": { "z": "x\u{7f}\u{1}/\"é", "_": {} }, "aa": [] });
        assert_eq!(to_json(&value), r#"{"A":[true,null,2.5],"a":{"_":{},"z":"x\u007f\u0001/\"é"},"aa":[],"b":1}"#);
    }

    #[test]
    fn an_agent_without_the_plugin_is_told_to_read_the_guide() {
        let ask = |method: &str, has_skills: bool| {
            let caller = Caller { has_skills, ..Caller::default() };
            let reply = answer(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }), &caller, |_| json!(null)).unwrap();
            serde_json::from_str::<Value>(&reply).unwrap()["result"].clone()
        };
        let names = |result: Value| result["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(ask("initialize", false)["instructions"], guide::INSTRUCTIONS);
        assert_eq!(names(ask("tools/list", false))[0], "notebook_guide");
        assert_eq!(ask("tools/list", false)["tools"][0]["annotations"]["readOnlyHint"], true);

        assert!(ask("initialize", true).get("instructions").is_none(), "Claude Code has the plugin's skills");
        assert!(!names(ask("tools/list", true)).contains(&"notebook_guide".to_owned()));
        assert!(is_tool("notebook_guide") && is_tool("edit_cell") && is_tool("run_shell") && !is_tool("edit"));
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
        assert!(!runs("read_cell", json!({})));
        assert!(!runs("read_file", json!({ "path": "/tmp/x" })));
    }

    #[test]
    fn manual_holds_every_write_and_no_read() {
        let held = |tool: &str, policy: &str, edits: bool| asks_first(tool, &json!({ "cell_id": "a", "code": "1" }), policy, edits);
        for tool in WRITE_TOOLS {
            assert!(held(tool, "ask", true), "{tool} in Manual");
        }
        for tool in ["read_cell", "read_notebook_code", "list_notebooks", "view_cell_output", "search_code", "open_notebook", "keep_notebook_alive", "read_file"] {
            assert!(!held(tool, "ask", true) && !held(tool, "auto", true), "{tool} only reads");
        }
        assert!(!held("edit_cell", "ask", false), "Ask to run doesn't hold an edit that doesn't run");
        assert!(held("execute_cell", "ask", false));
        assert!(held("move_cell", "auto", true) && !held("execute_cell", "auto", true), "Manual with runs allowed still asks before changes");
        assert!(!held("edit_cell", "plan", true), "plan refuses it instead");
    }

    #[test]
    fn reads_errors_as_julia_did() {
        let text = |raw: &str| tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(text("ArgumentError: not_found::No folder at /x::y"), r#"{"error":"not_found","message":"No folder at /x::y"}"#);
        assert_eq!(text("SystemError: opening file \"/x\": Permission denied"), r#"{"error":"tool_error","message":"SystemError: opening file \"/x\": Permission denied"}"#);
        assert_eq!(text("IOError: readdir(\"/a::b\"): denied"), r#"{"error":"readdir(\"/a","message":"b\"): denied"}"#);
        assert_eq!(tool_error("x", false)["isError"], true);
    }

    #[test]
    fn a_wrapped_key_error_unwraps_to_its_own_kind_and_message() {
        let text = |raw: &str| tool_error(raw, false)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(
            text("KeyError: key \"notebook_not_found::No notebook with id 'x' in the current session. Run list_notebooks to see what's open.\" not found"),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session. Run list_notebooks to see what's open."}"#
        );
        // A quoted key with no kind::message of its own reads as a plain KeyError.
        assert_eq!(text("KeyError: key \"code\" not found"), r#"{"error":"tool_error","message":"KeyError: key \"code\" not found"}"#);
    }

    #[test]
    fn a_misuse_error_points_to_the_guide_only_when_asked_to_help() {
        let text = |raw: &str, help: bool| tool_error(raw, help)["content"][0]["text"].as_str().unwrap().to_owned();
        let notebook_not_found = "ArgumentError: notebook_not_found::No notebook with id 'x' in the current session.";
        assert_eq!(
            text(notebook_not_found, true),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session.\nSee `notebook_guide` for how to use these tools."}"#,
            "an agent without the plugin, on a kind the guide explains"
        );
        assert_eq!(
            text(notebook_not_found, false),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session."}"#,
            "an agent with its own skills gets no pointer to this server's guide"
        );
        // A Julia process crash isn't something the guide explains, whether asked to help or not.
        let crashed = "ArgumentError: process_exited::The notebook's Julia process exited while running a cell";
        assert_eq!(text(crashed, true), text(crashed, false), "not an agent misuse kind");
        assert!(!text(crashed, true).contains("notebook_guide"));
    }
}

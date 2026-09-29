//! The agent's MCP connection, MCP over Streamable HTTP (spec 2025-06-18):
//! `POST /mcp` carries one JSON-RPC message; a request gets its reply in the
//! same response, a notification or a response from the client gets `202
//! Accepted` with no body. Every tool call is request and reply, so this
//! server never needs to stream a reply back, and issues no `Mcp-Session-Id`
//! (optional in the spec; the adapter's MCP client doesn't send one back when
//! none is issued). Each agent session's messages carry `X-Endeavor-Session`
//! (its key) and, on a server, `X-Endeavor-Host`. The notebook tools are
//! `notebooks`'; host tools are `host_tools`'.
//!
//! Each session has a run policy the app sets ("plan" | "ask" | "auto"). In
//! "plan" its notebook writes and runs are refused. "ask" and "auto" pass
//! through for now: runs are still gated by the app's Claude hook.

use std::collections::HashMap;
use std::io::{self, BufReader};
use std::net::TcpStream;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};

use crate::host_tools;
use crate::http::{self, Head};
use crate::notebooks::{self, Julia, Notebooks, Reply};

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

/// Tools that change the notebook or run code, here or on the server.
pub const WRITE_TOOLS: [&str; 12] = [
    "edit_cell", "edit_cells", "add_cell", "delete_cell", "move_cell", "fold_cell", "new_notebook",
    "execute_cell", "submit_changes", "run_all_cells", "allow_execution", "run_shell",
];

/// What every client connection shares.
pub struct Bridge {
    pub julia: Arc<Julia>,
    pub notebooks: Arc<Notebooks>,
    pub token: String,
    /// Each agent session's run policy, by its key.
    policies: Mutex<HashMap<String, String>>,
    /// Each agent session's working folder on this machine, by its key.
    folders: Mutex<HashMap<String, String>>,
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
}

impl Caller {
    fn of(request: &Head) -> Caller {
        let header = |name| request.header(name).unwrap_or_default().to_owned();
        Caller { owner: header("X-Endeavor-Session"), host: header("X-Endeavor-Host") }
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
        ];
        let julia = Arc::new(Julia::new(token.clone()));
        let clock = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        Bridge {
            notebooks: Arc::new(Notebooks::new(julia.clone(), Box::new(clock))),
            julia,
            token,
            policies: Mutex::default(),
            folders: Mutex::default(),
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
                eprintln!("[ Info: Session {owner} policy: {policy}");
                self.policies.lock().unwrap().insert(owner, policy);
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
            _ => return Some(self.dispatch(&message, &Caller::default()).unwrap_or_else(|| "{}".into())),
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        Some(to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": {} })))
    }

    /// Serve `POST /mcp`. Whether the connection can carry another request.
    pub fn mcp(&self, request: &Head, reader: &mut BufReader<TcpStream>, client: &mut TcpStream) -> io::Result<bool> {
        post(request, reader, client, request.keeps_alive(), |message| self.dispatch(message, &Caller::of(request)))
    }

    /// The reply to one JSON-RPC message, if it gets one.
    fn dispatch(&self, message: &Value, caller: &Caller) -> Option<String> {
        answer(message, caller, |params| {
            let result = self.call_tool(params, caller);
            self.notebooks.publish();
            result
        })
    }

    /// A `tools/call`'s result.
    fn call_tool(&self, params: &Value, caller: &Caller) -> Value {
        let text = |result: &Value| json!({ "content": [{ "type": "text", "text": to_json(result) }], "isError": false });
        let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return tool_error("ArgumentError: invalid_argument::arguments must be an object");
        }
        let name = match params.get("name") {
            None => "",
            Some(Value::String(name)) => name.as_str(),
            Some(other) => return tool_error(&format!("ArgumentError: unknown_tool::Unknown tool: '{}'", julia_string(other))),
        };
        self.notebooks.note_activity(&arguments);
        if let Some(refusal) = self.refusal(caller, name) {
            return tool_error(&refusal);
        }
        if host_tools::NAMES.contains(&name) {
            let folder = self.folders.lock().unwrap().get(&caller.owner).cloned();
            let env: Vec<_> = self.shell_env.iter().map(|(name, value)| (*name, value.as_deref())).collect();
            return match host_tools::call(name, &arguments, &host_tools::Shell { folder: folder.as_deref(), env: &env }) {
                Ok(result) => text(&result),
                Err(error) => tool_error(&error),
            };
        }
        if name == "keep_notebook_alive" {
            return self.notebooks.keep_alive(&arguments).map_or_else(|e| tool_error(&e), |r| text(&r));
        }
        if let Some(refusal) = self.notebooks.refusal(&caller.owner, name, &arguments) {
            return tool_error(&refusal);
        }
        let folder = self.folders.lock().unwrap().get(&caller.owner).cloned();
        match self.notebooks.tool(&caller.owner, name, &arguments, folder.as_deref()) {
            Ok(Reply::Json(result)) => text(&result),
            Ok(Reply::Image { meta, png_base64 }) => json!({
                "content": [
                    { "type": "text", "text": to_json(&meta) },
                    { "type": "image", "data": png_base64, "mimeType": "image/png" },
                ],
                "isError": false,
            }),
            Err(error) => tool_error(&error),
        }
    }

    /// Why a session may not call `tool`, as the error Julia raised for it.
    fn refusal(&self, caller: &Caller, tool: &str) -> Option<String> {
        if host_tools::NAMES.contains(&tool) && caller.host.is_empty() {
            return Some(format!(
                "ArgumentError: host_tools::`{tool}` is only for sessions on a server. This session runs on this Mac: use your own file and shell tools."
            ));
        }
        let plan = self.policies.lock().unwrap().get(&caller.owner).is_some_and(|p| p == "plan");
        if plan && WRITE_TOOLS.contains(&tool) {
            let what = if tool == "run_shell" { "run a command on the server" } else { "change or run the notebook" };
            return Some(format!(
                "ArgumentError: plan_mode::Plan mode is read-only: `{tool}` would {what}. Finish the plan; the user switches modes to carry it out."
            ));
        }
        None
    }
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
            ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "endeavor-runtime", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => {
            let mut tools = NOTEBOOK_TOOLS.as_array().cloned().unwrap_or_default();
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

/// A failed tool call's result, from the text of the error Julia raised:
/// `ArgumentError: kind::message` names its kind; anything else is a `tool_error`.
pub(crate) fn tool_error(raw: &str) -> Value {
    let unwrapped = unwrap_key_error(raw);
    let raw = unwrapped.as_deref().unwrap_or(raw);
    let (kind, message) = match raw.split_once("::") {
        Some((kind, message)) => {
            let kind = kind.trim();
            (kind.rsplit(':').next().unwrap_or_default().trim(), message)
        }
        None => ("tool_error", raw),
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
    fn reads_errors_as_julia_did() {
        let text = |raw: &str| tool_error(raw)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(text("ArgumentError: not_found::No folder at /x::y"), r#"{"error":"not_found","message":"No folder at /x::y"}"#);
        assert_eq!(text("SystemError: opening file \"/x\": Permission denied"), r#"{"error":"tool_error","message":"SystemError: opening file \"/x\": Permission denied"}"#);
        assert_eq!(text("IOError: readdir(\"/a::b\"): denied"), r#"{"error":"readdir(\"/a","message":"b\"): denied"}"#);
        assert_eq!(tool_error("x")["isError"], true);
    }

    #[test]
    fn a_wrapped_key_error_unwraps_to_its_own_kind_and_message() {
        let text = |raw: &str| tool_error(raw)["content"][0]["text"].as_str().unwrap().to_owned();
        assert_eq!(
            text("KeyError: key \"notebook_not_found::No notebook with id 'x' in the current session. Run list_notebooks to see what's open.\" not found"),
            r#"{"error":"notebook_not_found","message":"No notebook with id 'x' in the current session. Run list_notebooks to see what's open."}"#
        );
        // A quoted key with no kind::message of its own reads as a plain KeyError.
        assert_eq!(text("KeyError: key \"code\" not found"), r#"{"error":"tool_error","message":"KeyError: key \"code\" not found"}"#);
    }
}

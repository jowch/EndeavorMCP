//! The notebook view: Pluto's own editor in an MCP App view, for clients that draw them (Claude
//! Desktop and claude.ai, which advertise the `io.modelcontextprotocol/ui` extension).
//!
//! The view runs in the client's sandbox, which can reach nothing on this computer, so it gets
//! everything through tools only it can call (`visibility: ["app"]`): Pluto's page in parts
//! (`notebook_page`), and a WebSocket to Pluto that the core holds for it (`notebook_socket_*`).
//! The front (`endeavor mcp`) serves the view itself (`resources/read`) and forwards these calls to
//! the core like any other. The sockets live in the core: a client may run one front per surface,
//! and in Claude Desktop's chat the agent's calls and the view's reach different fronts.
//!
//! A socket goes only to this runtime's own Pluto, on loopback, with Pluto's secret, as the page
//! proxy does (`core::Served`); the view names a notebook, never an address. Opening one checks
//! that Pluto has that notebook open, but the socket is then a whole Pluto connection, as a browser
//! tab's is: its messages can name any notebook this Pluto has open. That is what the browser link
//! behind the same secret gives too, and no more.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use wire::backend::Backend;

/// The extension a client names in `initialize` when it draws MCP App views.
pub const EXTENSION: &str = "io.modelcontextprotocol/ui";
/// The one view, shown by `open_notebook` and `new_notebook`.
pub const URI: &str = "ui://endeavor/notebook";
const MIME: &str = "text/html;profile=mcp-app";

pub const TOOLS_JSON: &str = include_str!("view_tools.json");
static TOOLS: LazyLock<Value> = LazyLock::new(|| {
    let mut tools: Value = serde_json::from_str(TOOLS_JSON).expect("view_tools.json");
    for tool in tools.as_array_mut().into_iter().flatten() {
        tool["_meta"] = json!({ "ui": { "visibility": ["app"] } });
    }
    tools
});
/// The view's tools: only the view calls them, and the agent doesn't see them.
pub const NAMES: [&str; 5] = ["notebook_page", "notebook_socket_open", "notebook_socket_send", "notebook_socket_recv", "notebook_socket_close"];

/// The tools that show the view.
const SHOWN_BY: [&str; 2] = ["open_notebook", "new_notebook"];

/// The view's page: our loader with the MCP Apps SDK (`@modelcontextprotocol/ext-apps`'
/// `app-with-deps.js`) and the Endeavor look and Point (`endeavor-view.js`, the app's
/// `frontend/dist/view.js`) inside it. See `views/README.md`.
static PAGE: LazyLock<String> = LazyLock::new(|| {
    let sdk = as_global(include_str!("views/mcp-app.js")).replace("</script", "<\\/script");
    let look = include_str!("views/endeavor-view.js").replace("</script", "<\\/script");
    include_str!("views/notebook.html").replacen("/*MCP_APP_SDK*/", &sdk, 1).replacen("/*ENDEAVOR_VIEW*/", &look, 1)
});

/// The SDK's module with its closing `export{a as B,...}` made `window.McpAppLib={B:a,...}`, so
/// that it runs inline in the view's own script.
fn as_global(module: &str) -> String {
    let Some(at) = module.rfind("export{") else { return module.to_owned() };
    let Some(end) = module[at..].find('}').map(|i| at + i) else { return module.to_owned() };
    let names: Vec<String> = module[at + "export{".len()..end]
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| match item.split_once(" as ") {
            Some((local, exported)) => format!("{}:{}", exported.trim(), local.trim()),
            None => item.to_owned(),
        })
        .collect();
    format!("{}window.McpAppLib={{{}}};{}", &module[..at], names.join(","), module[end + 1..].trim_start_matches(';'))
}

pub fn is_view_tool(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Whether a client's `initialize` says it draws MCP App views.
pub fn client_draws_views(initialize: &Value) -> bool {
    initialize["params"]["capabilities"]["extensions"].get(EXTENSION).is_some()
}

/// The schemas of the view's tools, for `tools/list` and the argument check.
pub fn schemas() -> Vec<Value> {
    TOOLS.as_array().cloned().unwrap_or_default()
}

/// `new_notebook`'s and `open_notebook`'s argument, for a client that draws views, to open the
/// notebook in the browser after all.
pub const IN_BROWSER: &str = "in_browser";

/// `tools/list` for a client that draws views: the tools that open a notebook show it, and the
/// view's own tools are listed for the view.
pub fn add_to(tools: &mut Vec<Value>) {
    for tool in tools.iter_mut().filter(|tool| tool["name"].as_str().is_some_and(|name| SHOWN_BY.contains(&name))) {
        tool["_meta"] = json!({ "ui": { "resourceUri": URI }, "ui/resourceUri": URI });
        // A client may say it draws views and still show no card (Claude's desktop app, in its Chat
        // tab, 2026-11-12, #311): the way back to the browser.
        tool["inputSchema"]["properties"][IN_BROWSER] = json!({
            "type": "boolean",
            "description": "Open the notebook in the user's browser instead of beside the chat. Set it when the user says they don't see the notebook or its card. Default: false.",
        });
    }
    tools.extend(schemas());
}

pub fn resources_list() -> Value {
    json!({ "resources": [{ "uri": URI, "name": "Notebook", "description": "The notebook, live, beside the chat.", "mimeType": MIME }] })
}

/// `resources/read`'s result, or None for a resource we don't have.
pub fn resources_read(params: &Value) -> Option<Value> {
    (params["uri"] == URI).then(|| {
        json!({ "contents": [{
            "uri": URI,
            "mimeType": MIME,
            "text": PAGE.as_str(),
            // Claude's own field: open in the side panel, not in the conversation. The view also
            // declares that it shows in the panel only (`availableDisplayModes`).
            "_meta": { "ui": { "prefersBorder": false, "preferredDisplayMode": "fullscreen" } },
        }] })
    })
}

/// Pluto's page for the notebook view, from Julia (`endeavor/page_bundle`). Ember's comes from R's
/// adapter (`page_bundle`, Ember's `view_page()`), through the core's engines.
pub fn page_from_julia(port: u16, token: &str) -> Result<String, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/page_bundle" }).to_string();
    let bearer = format!("Bearer {token}");
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
    let (status, reply) = crate::http::post(port, "/call", &headers, body.as_bytes()).map_err(|e| format!("not_ready: Julia didn't answer: {e}"))?;
    let reply: Value = serde_json::from_slice::<Value>(&reply).map_err(|e| format!("Julia's page: {e} (HTTP {status})"))?;
    match reply["result"]["html"].as_str() {
        Some(html) => Ok(html.to_owned()),
        None => Err(format!("Julia couldn't make Pluto's page: {}", reply["error"]["message"].as_str().unwrap_or("no page"))),
    }
}

/// What the core keeps for the views: each engine's page, made once, and the open sockets.
#[derive(Default)]
pub struct Views {
    /// Pluto's private port and secret, once Julia is up.
    pub pluto: OnceLock<(u16, String)>,
    /// Ember's port and secret, from the R that runs now, and which start of R that is (from 1): each
    /// start may have a newer Ember, and may well reuse the port (Ember tries 4321 first).
    ember: Mutex<Option<(u16, String, u64)>>,
    /// Each engine's page, with the start of the engine that made it (0 for Julia, which starts once):
    /// a page from an R that has ended is made again.
    pages: Mutex<Vec<(Backend, u64, Arc<String>)>>,
    /// The engine of each notebook a view's socket opened for, as `open` said then: the view opens
    /// one before it reads its page, so the page's parts need no `open` (a snapshot of every
    /// notebook) each.
    engines: Mutex<HashMap<String, Backend>>,
    sockets: Mutex<HashMap<u64, Arc<Socket>>>,
    next: AtomicU64,
}

/// A socket the view hasn't asked about for this long is closed: the view went away.
const IDLE: Duration = Duration::from_secs(60);
/// A long-poll waits at most this long.
const WAIT_MAX: Duration = Duration::from_millis(1500);
/// What one receive returns at most, before base64: a larger message comes in parts (`more`), as
/// the page does, since a client may limit a tool result's size.
const RECV_BYTES: usize = 1_000_000;
/// What a socket holds for its view at most. A view that stops reading for this long is gone.
const INBOX_BYTES: usize = 256_000_000;
/// Sockets open at once, over all views. Each view has one; an old one goes when its view does.
const SOCKETS_MAX: usize = 16;
/// A page part at most.
const PART_MAX: usize = 2_000_000;

impl Views {
    /// R started: Ember's port and secret.
    pub fn set_ember(&self, port: u16, secret: String) {
        let mut ember = self.ember.lock().unwrap();
        let start = ember.as_ref().map_or(0, |(_, _, start)| *start) + 1;
        *ember = Some((port, secret, start));
    }

    /// Where an engine's page and WebSocket are, and which start of it this is, or why not.
    fn engine(&self, backend: Backend) -> Result<(u16, String, u64), String> {
        match backend {
            Backend::Pluto => self.pluto.get().map(|(port, secret)| (*port, secret.clone(), 0)).ok_or_else(|| "not_ready: Julia isn't running".to_owned()),
            Backend::Ember => self.ember.lock().unwrap().clone().ok_or_else(|| "not_ready: R isn't running".to_owned()),
        }
    }

    /// One of the view's tool calls. `page` makes an engine's page for a notebook open in it (Pluto's
    /// from Julia, Ember's from R's adapter), and `open` says which engine has a notebook id open, if
    /// any. A `notebook_page` without a notebook is Pluto's.
    pub fn call(&self, name: &str, arguments: &Value, page: impl FnOnce(Backend, Option<&str>) -> Result<String, String>, open: impl FnOnce(&str) -> Option<Backend>) -> Result<Value, String> {
        self.close_idle();
        let socket_of = || -> Result<Arc<Socket>, String> {
            let id = arguments["socket"].as_u64().ok_or("socket must be a number")?;
            self.sockets.lock().unwrap().get(&id).cloned().ok_or_else(|| "closed: that connection is closed; open another".to_owned())
        };
        match name {
            "notebook_page" => {
                let notebook = arguments["notebook_id"].as_str();
                let backend = match notebook {
                    Some(id) => match self.engines.lock().unwrap().get(id).copied() {
                        Some(backend) => backend,
                        None => open(id).ok_or_else(|| format!("not_open: no open notebook has the id {id}"))?,
                    },
                    None => Backend::Pluto,
                };
                let page = self.page(backend, || page(backend, notebook))?;
                let offset = arguments["offset"].as_u64().unwrap_or(0) as usize;
                let size = (arguments["size"].as_u64().unwrap_or(1_000_000) as usize).clamp(1, PART_MAX);
                let (text, next) = part(&page, offset, size);
                Ok(json!({ "total": page.len(), "next": next, "text": text }))
            }
            "notebook_socket_open" => {
                let notebook = arguments["notebook_id"].as_str().ok_or("notebook_id must be a string")?;
                let backend = open(notebook).ok_or_else(|| format!("not_open: no open notebook has the id {notebook}"))?;
                {
                    // A notebook's engine stays its engine while it's open; ids are few.
                    let mut engines = self.engines.lock().unwrap();
                    if engines.len() >= 1024 {
                        engines.clear();
                    }
                    engines.insert(notebook.to_owned(), backend);
                }
                let (port, secret, _) = self.engine(backend)?;
                let engine = crate::notebooks::engines::language(backend);
                if self.sockets.lock().unwrap().len() >= SOCKETS_MAX {
                    return Err("too_many: too many notebook views are open".into());
                }
                let socket = Socket::connect(port, &format!("/?secret={secret}"), IDLE, INBOX_BYTES).map_err(|e| format!("not_connected: couldn't reach {engine}'s notebook server: {e}"))?;
                // Counted again with the insert, under one lock: calls at the same moment stay under the cap.
                let mut sockets = self.sockets.lock().unwrap();
                if sockets.len() >= SOCKETS_MAX {
                    socket.close();
                    return Err("too_many: too many notebook views are open".into());
                }
                let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                sockets.insert(id, socket);
                Ok(json!({ "socket": id }))
            }
            "notebook_socket_send" => {
                let data = STANDARD.decode(arguments["data"].as_str().unwrap_or_default()).map_err(|e| format!("data isn't base64: {e}"))?;
                socket_of()?.send(&data).map_err(|e| format!("closed: {e}"))?;
                Ok(json!({ "ok": true }))
            }
            "notebook_socket_recv" => {
                let wait = Duration::from_millis(arguments["wait_ms"].as_u64().unwrap_or(1500)).min(WAIT_MAX);
                let socket = socket_of()?;
                let got = socket.recv(wait);
                if got.closed {
                    self.sockets.lock().unwrap().retain(|_, s| !Arc::ptr_eq(s, &socket));
                }
                // `more`: the last message goes on in the next receive.
                Ok(json!({ "messages": got.messages.iter().map(|m| STANDARD.encode(m)).collect::<Vec<_>>(), "more": got.more, "closed": got.closed }))
            }
            "notebook_socket_close" => {
                if let Ok(socket) = socket_of() {
                    socket.close();
                    self.sockets.lock().unwrap().retain(|_, s| !Arc::ptr_eq(s, &socket));
                }
                Ok(json!({ "ok": true }))
            }
            _ => Err(format!("unknown view tool {name}")),
        }
    }

    fn page(&self, backend: Backend, make: impl FnOnce() -> Result<String, String>) -> Result<Arc<String>, String> {
        let (_, _, start) = self.engine(backend)?;
        let mut pages = self.pages.lock().unwrap();
        if let Some((_, _, page)) = pages.iter().find(|(b, made_by, _)| *b == backend && *made_by == start) {
            return Ok(page.clone());
        }
        let made = Arc::new(split_page(&make()?)?);
        pages.retain(|(b, _, _)| *b != backend);
        pages.push((backend, start, made.clone()));
        Ok(made)
    }

    fn close_idle(&self) {
        let mut sockets = self.sockets.lock().unwrap();
        // A socket Pluto closed stays until the view reads what's left and hears of the close.
        sockets.retain(|_, socket| {
            let keep = socket.used.lock().unwrap().elapsed() < IDLE;
            if !keep {
                socket.close();
            }
            keep
        });
    }

    #[cfg(test)]
    fn open_sockets(&self) -> usize {
        self.sockets.lock().unwrap().len()
    }
}

/// The part of `page` from byte `offset`, at most `size` bytes and ending on a character, and where
/// the next part starts.
fn part(page: &str, offset: usize, size: usize) -> (&str, usize) {
    let start = offset.min(page.len());
    let start = (0..=start).rev().find(|&i| page.is_char_boundary(i)).unwrap_or(0);
    let mut end = (start + size).min(page.len());
    while !page.is_char_boundary(end) {
        end -= 1;
    }
    // A part smaller than one character still moves on.
    if end == start && start < page.len() {
        end = (start + 1..=page.len()).find(|&i| page.is_char_boundary(i)).unwrap_or(page.len());
    }
    (&page[start..end], end)
}

/// Pluto's offline page (`generate_html(offline_bundle=true)`) as JSON the view puts together:
/// `head` and `body` markup without scripts, and `scripts` in order, each `{attrs, text}` with a
/// `data:` source read into its text. The view can't parse the page itself: Pluto's code holds
/// sequences (`<!--<script`) that change how HTML reads a script once it is inline. Here the
/// scripts are still `data:` sources, so a script's end is the first `</script` after it.
/// Stylesheets from `data:` become `<style>`.
pub fn split_page(html: &str) -> Result<String, String> {
    let lower = html.to_ascii_lowercase();
    let head_start = lower.find("<head").and_then(|i| lower[i..].find('>').map(|j| i + j + 1)).ok_or("no <head> in Pluto's page")?;
    let head_end = lower.find("</head>").ok_or("no </head> in Pluto's page")?;
    let body_start = lower.find("<body").and_then(|i| lower[i..].find('>').map(|j| i + j + 1)).ok_or("no <body> in Pluto's page")?;
    let body_end = lower.rfind("</body>").ok_or("no </body> in Pluto's page")?;
    let mut scripts = Vec::new();
    let head = without_scripts(&html[head_start..head_end], &lower[head_start..head_end], &mut scripts)?;
    let body = without_scripts(&html[body_start..body_end], &lower[body_start..body_end], &mut scripts)?;
    Ok(serde_json::to_string(&json!({ "head": head, "body": body, "scripts": scripts })).expect("json"))
}

fn without_scripts(html: &str, lower: &str, scripts: &mut Vec<Value>) -> Result<String, String> {
    let mut out = String::with_capacity(html.len() / 4);
    let mut at = 0;
    loop {
        let script = lower[at..].find("<script").map(|i| at + i);
        let style = lower[at..].find("<link").map(|i| at + i);
        match (script, style) {
            (Some(s), l) if l.is_none_or(|l| s < l) => {
                out.push_str(&html[at..s]);
                let open_end = s + lower[s..].find('>').ok_or("unclosed <script")? + 1;
                let close = open_end + lower[open_end..].find("</script").ok_or("no </script")?;
                let after = close + lower[close..].find('>').ok_or("unclosed </script")? + 1;
                let mut attrs = attributes(&html[s + "<script".len()..open_end - 1]);
                let mut text = html[open_end..close].to_owned();
                if let Some(Value::String(src)) = attrs.remove("src") {
                    text = data_url(&src).ok_or("a script from outside the page")?;
                }
                scripts.push(json!({ "attrs": attrs, "text": text }));
                at = after;
            }
            (_, Some(l)) => {
                out.push_str(&html[at..l]);
                let end = l + lower[l..].find('>').ok_or("unclosed <link")? + 1;
                let tag = &html[l..end];
                let attrs = attributes(&tag["<link".len()..tag.len() - 1]);
                match (attrs.get("rel").and_then(Value::as_str), attrs.get("href").and_then(Value::as_str).and_then(data_url)) {
                    (Some("stylesheet"), Some(css)) => {
                        out.push_str("<style>");
                        out.push_str(&css.replace("</style", "<\\/style"));
                        out.push_str("</style>");
                    }
                    _ => out.push_str(tag),
                }
                at = end;
            }
            _ => {
                out.push_str(&html[at..]);
                return Ok(out);
            }
        }
    }
}

/// A tag's attributes: `name="value"`, `name='value'`, `name=value` or a bare `name`.
fn attributes(text: &str) -> serde_json::Map<String, Value> {
    let mut attrs = serde_json::Map::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        if start == i {
            break;
        }
        let name = text[start..i].to_ascii_lowercase();
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                let from = i + 1;
                i = from + bytes[from..].iter().position(|&b| b == quote).unwrap_or(bytes.len() - from);
                value = text[from..i].to_owned();
                i += 1;
            } else {
                let from = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = text[from..i].to_owned();
            }
        }
        attrs.insert(name, unescape(&value).into());
    }
    attrs
}

fn unescape(value: &str) -> String {
    value.replace("&quot;", "\"").replace("&#39;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// A `data:` URL's text: base64 or percent-encoded. None for anything else.
fn data_url(url: &str) -> Option<String> {
    let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
    if head.ends_with(";base64") {
        return String::from_utf8(STANDARD.decode(data.trim()).ok()?).ok();
    }
    let mut out = Vec::with_capacity(data.len());
    let bytes = data.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((high * 16 + low) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// A WebSocket client to Pluto on loopback: a reader thread fills the inbox.
struct Socket {
    stream: Mutex<TcpStream>,
    inbox: Mutex<Inbox>,
    arrived: Condvar,
    closed: AtomicBool,
    used: Mutex<Instant>,
}

/// Pluto's messages the view hasn't read yet, and their bytes.
#[derive(Default)]
struct Inbox {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
}

/// One receive: whole messages, except that the last goes on in the next receive when `more`.
struct Got {
    messages: Vec<Vec<u8>>,
    more: bool,
    closed: bool,
}

impl Socket {
    /// A socket that closes itself when its view hasn't asked about it for `idle`, or has left more
    /// than `inbox_max` bytes unread: the view went away without saying so (Claude quit, say).
    fn connect(port: u16, path: &str, idle: Duration, inbox_max: usize) -> io::Result<Arc<Socket>> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut key = [0u8; 16];
        fill_random(&mut key);
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            STANDARD.encode(key)
        );
        stream.write_all(request.as_bytes())?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut status = String::new();
        reader.read_line(&mut status)?;
        if !status.split_whitespace().nth(1).is_some_and(|code| code == "101") {
            return Err(io::Error::other(format!("Pluto answered {}", status.trim())));
        }
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err(io::Error::other("Pluto closed the connection"));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        stream.set_read_timeout(None)?;
        let socket = Arc::new(Socket { stream: Mutex::new(stream), inbox: Mutex::default(), arrived: Condvar::new(), closed: AtomicBool::new(false), used: Mutex::new(Instant::now()) });
        let reading = socket.clone();
        std::thread::spawn(move || reading.read_all(reader, inbox_max));
        // The watch: no view call is needed for an idle socket to close.
        let watched = Arc::downgrade(&socket);
        let every = (idle / 4).min(Duration::from_secs(5));
        std::thread::spawn(move || loop {
            std::thread::sleep(every);
            let Some(socket) = watched.upgrade() else { return };
            if socket.closed.load(Ordering::SeqCst) {
                return;
            }
            if socket.used.lock().unwrap().elapsed() >= idle {
                // Nobody will read what's left: free it now, not when a view next calls.
                socket.close();
                *socket.inbox.lock().unwrap() = Inbox::default();
                return;
            }
        });
        Ok(socket)
    }

    fn read_all(&self, mut reader: BufReader<TcpStream>, inbox_max: usize) {
        let mut message = Vec::new();
        while let Ok((fin, opcode, payload)) = read_frame(&mut reader) {
            match opcode {
                0x8 => break,
                0x9 => {
                    let _ = self.write_frame(0xA, &payload);
                }
                0xA => {}
                _ => {
                    message.extend_from_slice(&payload);
                    if fin {
                        let mut inbox = self.inbox.lock().unwrap();
                        inbox.bytes += message.len();
                        inbox.messages.push_back(std::mem::take(&mut message));
                        let over = inbox.bytes > inbox_max;
                        drop(inbox);
                        self.arrived.notify_all();
                        if over {
                            break;
                        }
                    }
                }
            }
        }
        self.close();
    }

    fn send(&self, data: &[u8]) -> io::Result<()> {
        *self.used.lock().unwrap() = Instant::now();
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("the connection to Pluto ended"));
        }
        self.write_frame(0x2, data)
    }

    /// The messages that have come, up to `RECV_BYTES`, waiting up to `wait` for the first; and
    /// whether the socket is closed (with nothing more to read).
    fn recv(&self, wait: Duration) -> Got {
        self.recv_at_most(wait, RECV_BYTES)
    }

    fn recv_at_most(&self, wait: Duration, at_most: usize) -> Got {
        *self.used.lock().unwrap() = Instant::now();
        let deadline = Instant::now() + wait;
        let mut inbox = self.inbox.lock().unwrap();
        while inbox.messages.is_empty() && !self.closed.load(Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            inbox = self.arrived.wait_timeout(inbox, left).unwrap().0;
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(message) = inbox.messages.front_mut() {
            if bytes + message.len() > at_most {
                if out.is_empty() {
                    // Too large for one receive: its first part now, the rest next time.
                    let rest = message.split_off(at_most);
                    out.push(std::mem::replace(message, rest));
                    inbox.bytes -= at_most;
                    more = true;
                }
                break;
            }
            bytes += message.len();
            inbox.bytes -= message.len();
            out.push(inbox.messages.pop_front().unwrap());
        }
        let closed = inbox.messages.is_empty() && self.closed.load(Ordering::SeqCst);
        Got { messages: out, more, closed }
    }

    fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            let _ = self.write_frame(0x8, &[]);
        }
        let _ = self.stream.lock().unwrap().shutdown(std::net::Shutdown::Both);
        self.arrived.notify_all();
    }

    /// One masked frame, as a client sends.
    fn write_frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let n = payload.len();
        if n < 126 {
            frame.push(0x80 | n as u8);
        } else if n < 65536 {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mut mask = [0u8; 4];
        fill_random(&mut mask);
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.lock().unwrap().write_all(&frame)
    }
}

/// One frame: whether it ends its message, its opcode, and its payload (unmasked).
fn read_frame(reader: &mut impl Read) -> io::Result<(bool, u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head)?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    let mut len = u64::from(head[1] & 0x7F);
    if len == 126 {
        let mut b = [0u8; 2];
        reader.read_exact(&mut b)?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        reader.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    // Pluto's messages are its notebook's state: large, but not this large.
    if len > 1 << 30 {
        return Err(io::Error::other("a message over 1 GB"));
    }
    let mask = if head[1] & 0x80 != 0 {
        let mut m = [0u8; 4];
        reader.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    if let Some(mask) = mask {
        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
    }
    Ok((fin, opcode, payload))
}

/// Bytes for the WebSocket key and masks, which need not be secret (RFC 6455 asks for
/// unpredictable masks against proxies; this connection is on loopback).
fn fill_random(bytes: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let value = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_client_draws_views_when_it_names_the_extension() {
        assert!(client_draws_views(&json!({ "params": { "capabilities": { "extensions": { EXTENSION: {} } } } })));
        assert!(!client_draws_views(&json!({ "params": { "capabilities": {} } })));
    }

    #[test]
    fn the_tools_that_open_a_notebook_show_the_view_and_the_views_tools_are_for_the_view_only() {
        let mut tools = vec![json!({ "name": "open_notebook" }), json!({ "name": "read_cell" })];
        add_to(&mut tools);
        assert_eq!(tools[0]["_meta"]["ui"]["resourceUri"], URI);
        assert!(tools[1].get("_meta").is_none());
        assert_eq!(tools[0]["inputSchema"]["properties"][IN_BROWSER]["type"], "boolean", "the way to the browser when no card shows");
        assert!(tools[1].get("inputSchema").is_none());
        let names: Vec<&str> = tools[2..].iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, NAMES);
        assert!(tools[2..].iter().all(|t| t["_meta"]["ui"]["visibility"] == json!(["app"])));
    }

    #[test]
    fn the_view_is_one_page_with_the_sdk_inside() {
        let read = resources_read(&json!({ "uri": URI })).unwrap();
        let text = read["contents"][0]["text"].as_str().unwrap();
        assert!(!text.contains("/*MCP_APP_SDK*/") && !text.contains("/*ENDEAVOR_VIEW*/"));
        assert!(text.contains("window.EndeavorView = { start }"), "the look and Point are inside");
        assert!(text.contains("availableDisplayModes"));
        assert_eq!(read["contents"][0]["mimeType"], MIME);
        assert!(resources_read(&json!({ "uri": "ui://endeavor/other" })).is_none());
    }

    #[test]
    fn the_sdk_runs_inline() {
        assert_eq!(as_global("var a=1;export{a as App,b};"), "var a=1;window.McpAppLib={App:a,b};");
        assert!(PAGE.contains("window.McpAppLib={") && !PAGE.contains("export{"));
    }

    #[test]
    fn parts_end_on_characters_and_cover_the_page() {
        let page = "aé😀b".repeat(50);
        let mut joined = String::new();
        let mut offset = 0;
        while offset < page.len() {
            let (text, next) = part(&page, offset, 3);
            assert!(next > offset);
            joined.push_str(text);
            offset = next;
        }
        assert_eq!(joined, page);
    }

    #[test]
    fn the_page_is_split_into_markup_and_scripts_in_order() {
        let js = STANDARD.encode("let s = \"<!--<script\";");
        let css = STANDARD.encode("body{color:red}");
        let html = format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><script type=\"importmap\">{{}}</script><link rel=\"stylesheet\" href=\"data:text/css;base64,{css}\" type=\"text/css\"><link rel=\"icon\" href=\"data:image/png;base64,AA\"><script src=\"data:text/javascript;base64,{js}\" type=\"module\" defer=\"\"></script></head><body><div id=app></div><script>window.x = 1</script></body></html>"
        );
        let split: Value = serde_json::from_str(&split_page(&html).unwrap()).unwrap();
        assert_eq!(split["head"], "<meta charset=\"utf-8\"><style>body{color:red}</style><link rel=\"icon\" href=\"data:image/png;base64,AA\">");
        assert_eq!(split["body"], "<div id=app></div>");
        let scripts = split["scripts"].as_array().unwrap();
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0]["attrs"]["type"], "importmap");
        assert_eq!(scripts[1]["text"], "let s = \"<!--<script\";");
        assert_eq!(scripts[1]["attrs"], json!({ "type": "module", "defer": "" }));
        assert_eq!(scripts[2]["text"], "window.x = 1");
        assert!(split_page("<html><head><script src=\"https://example.com/a.js\"></script></head><body></body></html>").is_err(), "nothing from outside");
    }

    #[test]
    fn percent_encoded_data_urls_are_read() {
        assert_eq!(data_url("data:image/svg+xml,%3Csvg%3E").as_deref(), Some("<svg>"));
        assert_eq!(data_url("data:text/plain,50%25").as_deref(), Some("50%"));
        assert_eq!(data_url("data:text/plain,%é%").as_deref(), Some("%é%"), "a % that isn't an escape stays");
        assert_eq!(data_url("https://x"), None);
    }

    /// A WebSocket server that echoes each message, then closes after `closes_after` of them.
    fn echo_server(closes_after: usize) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    assert!(line.starts_with("GET /?secret=s "));
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header == "\r\n" {
                            break;
                        }
                    }
                    stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").unwrap();
                    for _ in 0..closes_after {
                        let Ok((_, _, payload)) = read_frame(&mut reader) else { return };
                        let mut frame = vec![0x82];
                        if payload.len() < 126 {
                            frame.push(payload.len() as u8);
                        } else {
                            frame.push(126);
                            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                        }
                        frame.extend_from_slice(&payload);
                        stream.write_all(&frame).unwrap();
                    }
                    let _ = stream.write_all(&[0x88, 0]);
                });
            }
        });
        port
    }

    #[test]
    fn a_socket_relays_messages_both_ways_and_reports_its_close() {
        let views = Views::default();
        let _ = views.pluto.set((echo_server(2), "s".into()));
        let open = |id: &str| (id == "nb").then_some(Backend::Pluto);
        let refused = views.call("notebook_socket_open", &json!({ "notebook_id": "other" }), |_, _| unreachable!(), open);
        assert!(refused.unwrap_err().starts_with("not_open"));
        let socket = views.call("notebook_socket_open", &json!({ "notebook_id": "nb" }), |_, _| unreachable!(), open).unwrap()["socket"].clone();
        let long = "x".repeat(300);
        for data in ["hello", long.as_str()] {
            views.call("notebook_socket_send", &json!({ "socket": socket, "data": STANDARD.encode(data) }), |_, _| unreachable!(), open).unwrap();
        }
        let mut got = Vec::new();
        let mut closed = false;
        let began = Instant::now();
        while !closed && began.elapsed() < Duration::from_secs(5) {
            let reply = views.call("notebook_socket_recv", &json!({ "socket": socket, "wait_ms": 500 }), |_, _| unreachable!(), open).unwrap();
            got.extend(reply["messages"].as_array().unwrap().iter().map(|m| String::from_utf8(STANDARD.decode(m.as_str().unwrap()).unwrap()).unwrap()));
            closed = reply["closed"] == true;
        }
        assert_eq!(got, ["hello", long.as_str()]);
        assert!(closed, "Pluto's close reaches the view");
        assert_eq!(views.open_sockets(), 0, "a closed socket is dropped");
        let gone = views.call("notebook_socket_send", &json!({ "socket": socket, "data": "" }), |_, _| unreachable!(), open);
        assert!(gone.unwrap_err().starts_with("closed"));
    }

    /// Wait up to 3 s for `socket` to close.
    fn closes(socket: &Socket) -> bool {
        let began = Instant::now();
        while !socket.closed.load(Ordering::SeqCst) && began.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        socket.closed.load(Ordering::SeqCst)
    }

    #[test]
    fn a_socket_no_view_asks_about_closes_itself() {
        let socket = Socket::connect(echo_server(1), "/?secret=s", Duration::from_millis(200), INBOX_BYTES).unwrap();
        assert!(closes(&socket), "closed with no view call");
    }

    #[test]
    fn a_socket_whose_view_stopped_reading_closes_and_keeps_what_came() {
        let socket = Socket::connect(echo_server(2), "/?secret=s", IDLE, 100).unwrap();
        socket.send(&[7; 300]).unwrap();
        assert!(closes(&socket), "over its inbox cap");
        let got = socket.recv(Duration::ZERO);
        assert_eq!((got.messages, got.closed), (vec![vec![7; 300]], true));
    }

    #[test]
    fn a_large_message_comes_in_parts() {
        let socket = Socket::connect(echo_server(1), "/?secret=s", IDLE, INBOX_BYTES).unwrap();
        let sent: Vec<u8> = (0..250).map(|i| i as u8).collect();
        socket.send(&sent).unwrap();
        let mut parts = Vec::new();
        let mut whole = Vec::new();
        loop {
            let got = socket.recv_at_most(Duration::from_secs(2), 100);
            assert_eq!(got.messages.len(), 1);
            whole.extend(&got.messages[0]);
            parts.push(got.more);
            if !got.more {
                break;
            }
        }
        assert_eq!((parts, whole), (vec![true, true, false], sent));
        assert_eq!(socket.inbox.lock().unwrap().bytes, 0);
    }

    #[test]
    fn a_receive_waits_no_longer_than_asked() {
        let views = Views::default();
        let _ = views.pluto.set((echo_server(1), "s".into()));
        let socket = views.call("notebook_socket_open", &json!({ "notebook_id": "nb" }), |_, _| unreachable!(), |_| Some(Backend::Pluto)).unwrap()["socket"].clone();
        let began = Instant::now();
        let reply = views.call("notebook_socket_recv", &json!({ "socket": socket, "wait_ms": 200 }), |_, _| unreachable!(), |_| Some(Backend::Pluto)).unwrap();
        assert_eq!(reply["messages"], json!([]));
        assert!(began.elapsed() < Duration::from_millis(1000));
        let began = Instant::now();
        views.call("notebook_socket_recv", &json!({ "socket": socket, "wait_ms": 60000 }), |_, _| unreachable!(), |_| Some(Backend::Pluto)).unwrap();
        assert!(began.elapsed() < WAIT_MAX + Duration::from_millis(500), "capped");
        views.call("notebook_socket_close", &json!({ "socket": socket }), |_, _| unreachable!(), |_| Some(Backend::Pluto)).unwrap();
        assert_eq!(views.open_sockets(), 0);
    }

    #[test]
    fn the_page_is_made_once_and_read_in_parts() {
        let views = Views::default();
        let _ = views.pluto.set((1, "s".into()));
        let html = "<html><head><title>P</title></head><body>b</body></html>";
        let first = views.call("notebook_page", &json!({ "offset": 0, "size": 10 }), |_, _| Ok(html.to_owned()), |_| Some(Backend::Pluto)).unwrap();
        assert_eq!(first["next"], 10);
        let rest = views.call("notebook_page", &json!({ "offset": 10, "size": 1000 }), |_, _| Err("made again".into()), |_| Some(Backend::Pluto)).unwrap();
        let joined = format!("{}{}", first["text"].as_str().unwrap(), rest["text"].as_str().unwrap());
        assert_eq!(rest["next"], first["total"]);
        let split: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(split["head"], "<title>P</title>");
    }

    #[test]
    fn an_r_notebooks_page_is_embers_and_made_again_when_r_starts_again() {
        let views = Views::default();
        let _ = views.pluto.set((1, "s".into()));
        let open = |id: &str| match id {
            "jl" => Some(Backend::Pluto),
            "r" => Some(Backend::Ember),
            _ => None,
        };
        let page = |backend: Backend, id: Option<&str>| Ok(format!("<html><head><title>{backend:?} {}</title></head><body></body></html>", id.unwrap_or("-")));
        let title = |reply: Value| serde_json::from_str::<Value>(reply["text"].as_str().unwrap()).unwrap()["head"].as_str().unwrap().to_owned();
        let read = |id: &str| views.call("notebook_page", &json!({ "notebook_id": id, "offset": 0, "size": 100000 }), page, open);
        assert!(read("r").unwrap_err().starts_with("not_ready: R"), "no R yet");
        views.set_ember(2, "e".into());
        assert_eq!(title(read("r").unwrap()), "<title>Ember r</title>");
        assert_eq!(title(read("jl").unwrap()), "<title>Pluto jl</title>");
        assert!(read("gone").unwrap_err().starts_with("not_open"));
        // Made once per R: the same R's page is kept, a new R's is made again.
        let again = |id: Option<&str>| views.call("notebook_page", &json!({ "notebook_id": "r", "offset": 0, "size": 100000 }), move |_, _| id.map(|id| format!("<html><head>{id}</head><body></body></html>")).ok_or("made again".to_owned()), open);
        assert_eq!(title(again(None).unwrap()), "<title>Ember r</title>");
        views.set_ember(2, "e".into());
        assert_eq!(title(again(Some("new")).unwrap()), "new", "an R started again on the same port and secret");
    }

    #[test]
    fn an_r_notebooks_socket_goes_to_ember() {
        let views = Views::default();
        let open = |_: &str| Some(Backend::Ember);
        let refused = views.call("notebook_socket_open", &json!({ "notebook_id": "r" }), |_, _| unreachable!(), open);
        assert!(refused.unwrap_err().starts_with("not_ready: R"));
        views.set_ember(echo_server(1), "s".into());
        let socket = views.call("notebook_socket_open", &json!({ "notebook_id": "r" }), |_, _| unreachable!(), open).unwrap()["socket"].clone();
        views.call("notebook_socket_close", &json!({ "socket": socket }), |_, _| unreachable!(), open).unwrap();
        // The view's probe told the views the notebook's engine: its page's parts don't ask again.
        let part = |offset: u64| views.call("notebook_page", &json!({ "notebook_id": "r", "offset": offset, "size": 10 }), |backend, _| Ok(format!("<html><head>{backend:?}</head><body></body></html>")), |_| unreachable!("a snapshot per part"));
        assert_eq!(part(0).unwrap()["next"], 10);
        assert_eq!(part(10).unwrap()["next"], 20);
    }
}

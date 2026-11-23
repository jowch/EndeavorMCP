use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::collections::HashSet;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::http::{self, Framing, Head};
use crate::mcp::{to_json, tool_error};
use crate::notebooks::{screenshot_result, short_cell_id, snapshot_may_help, uuid_value};

pub type Snapshot = dyn Fn(&str, &str, &str) -> Option<String> + Send + Sync;

pub type Pictures = dyn Fn(&str, &str, &str) -> Option<Picture> + Send + Sync;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picture {
    pub png: String,
    pub cut: Option<(u32, u32)>,
}

impl From<String> for Picture {
    fn from(png: String) -> Picture {
        Picture { png, cut: None }
    }
}

pub struct Older<'a> {
    pub no_r: &'a dyn Fn() -> String,
    pub notice: Option<String>,
    pub told: &'a Mutex<HashSet<String>>,
}

pub const REQUEST_RESTART: &str = "request_restart";

pub type AskRestart = dyn Fn(&str, &str, &dyn Fn() -> bool, Instant) -> RestartAnswer + Send + Sync;

#[derive(Clone, Debug, PartialEq)]
pub enum RestartAnswer {
    Restarting,
    Declined,
    Unanswered,
    Unavailable(String),
}

pub fn restart_schema() -> Value {
    json!({
        "name": REQUEST_RESTART,
        "description": "Ask the user whether to restart Endeavor where this session's notebook runs (on this computer or the server), which stops every notebook open there and starts them again. \
Use it only when a restart would fix what you can't: a tool said Endeavor there was started by an older version and keeps running as it is, or Julia or R stopped unexpectedly, couldn't start, or stopped answering. \
Not for errors in the notebook's own code, and not to start one notebook afresh (`restart_notebook` does that). \
Don't stop or restart it any other way yourself: this shows the user your reason with a Restart button, and nothing restarts unless they press it. \
The result says whether they restarted, declined, or haven't answered yet.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "Why a restart would help, in one short plain sentence for the user, such as \"Julia stopped answering while `fit` ran.\"" }
            },
            "required": ["reason"]
        },
        "annotations": { "readOnlyHint": false }
    })
}

fn restart_result(answer: RestartAnswer, place: &str) -> Value {
    match answer {
        RestartAnswer::Restarting => json!({ "content": [{ "type": "text", "text": format!(
            "The user restarted Endeavor on {place}. It takes a moment to start again; a notebook tool called meanwhile says so. \
The notebooks that were running there open and run again. Then call `list_notebooks` to find this session's notebook, and `open_notebook` if it isn't open."
        ) }] }),
        RestartAnswer::Declined => tool_error(&format!(
            "ArgumentError: not_approved::The user chose not to restart Endeavor on {place}, so nothing restarted. Don't ask again for the same reason, and don't stop it yourself: tell the user what won't work until it restarts."
        ), false),
        RestartAnswer::Unanswered => tool_error(&format!(
            "ArgumentError: waiting_for_user::The user hasn't answered yet, so nothing restarted. The question is still on their screen, and Endeavor on {place} restarts if they press Restart. \
Don't stop or restart it any other way: tell the user why a restart would help, and end your turn."
        ), false),
        RestartAnswer::Unavailable(why) => tool_error(&why, false),
    }
}

const RESTART_WAIT: Duration = Duration::from_secs(50);

pub(crate) fn restart_call(message: &Value, session: &str, restart: (&AskRestart, &str), gone: &dyn Fn() -> bool) -> Option<Value> {
    if message["method"] != "tools/call" || message["id"].is_null() || message["params"]["name"] != REQUEST_RESTART {
        return None;
    }
    let (ask, place) = restart;
    let reason = message["params"]["arguments"]["reason"].as_str().map(str::trim).unwrap_or_default();
    let result = if reason.is_empty() {
        tool_error("ArgumentError: invalid_argument::Give a `reason`: one short sentence for the user on why a restart would help.", false)
    } else {
        restart_result(ask(session, reason, &|| crate::mcp::waiting_on(crate::mcp::Waits::ANSWER, gone), Instant::now() + RESTART_WAIT), place)
    };
    Some(json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }))
}

pub(crate) fn with_restart_tool(result: &Value) -> Option<Value> {
    let tools = result["tools"].as_array()?;
    if tools.iter().any(|t| t["name"] == REQUEST_RESTART) {
        return None;
    }
    let mut result = result.clone();
    result["tools"].as_array_mut()?.push(restart_schema());
    Some(result)
}

pub fn serve_guarded(
    client: TcpStream,
    upstream: TcpStream,
    refuse: &dyn Fn(&str, &str, &Value) -> Option<String>,
    snapshot: Option<&Pictures>,
    older: Option<&Older>,
    restart: Option<(&AskRestart, &str)>,
) -> io::Result<()> {
    let mut client = wire::Shared::new(client);
    let mut reader = BufReader::new(client.clone());
    let mut to_runtime = wire::Shared::new(upstream);
    let mut from_runtime = BufReader::new(to_runtime.clone());
    while next_request(&mut reader, &client, &to_runtime)?
        && let Some(mut request) = Head::read(&mut reader)?
    {
        if request.method() != "POST" || request.path() != "/mcp" {
            request.write_to(&mut to_runtime)?;
            http::copy_body(&mut reader, &mut to_runtime, &mut request.request_body()?)?;
        } else {
            let body = http::read_body(&mut reader, request.request_body()?)?;
            let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let session = request.header("X-Endeavor-Session").unwrap_or_default().to_owned();
            if let Some(restart) = restart
                && message["method"] == "tools/call"
                && message["params"]["name"] == REQUEST_RESTART
            {
                crate::mcp::reply(&request, &mut client, &message, false, false, |message, gone| {
                    restart_call(message, &session, restart, gone).map(|reply| to_json(&reply))
                })?;
                return Ok(());
            }
            let params = &message["params"];
            let refusal = (message["method"] == "tools/call" && !message["id"].is_null())
                .then(|| refuse(&session, params["name"].as_str().unwrap_or_default(), params.get("arguments").unwrap_or(&Value::Null)))
                .flatten();
            if let Some(why) = refusal {
                let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": tool_error(&why, false) });
                http::respond(&mut client, "200 OK", Some("application/json"), to_json(&reply).as_bytes(), request.keeps_alive())?;
                if !request.keeps_alive() {
                    return Ok(());
                }
                continue;
            }
            let call = message["method"] == "tools/call" && !message["id"].is_null();
            let (tool, arguments) = (params["name"].as_str().unwrap_or_default(), params.get("arguments").unwrap_or(&Value::Null));
            let view = snapshot.filter(|_| call && tool == "view_cell_output");
            let no_r = older.map(|o| o.no_r).filter(|_| call && crate::mcp::names_r_notebook(tool, arguments));
            let list = restart.is_some() && message["method"] == "tools/list" && !message["id"].is_null();
            let notice = older.and_then(|o| o.notice.as_deref().map(|notice| (o, notice))).filter(|(o, _)| {
                call && (tool == "session_status" || (!crate::mcp::asks_first(tool, arguments, "ask", true) && !o.told.lock().unwrap().contains(&session)))
            });
            request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
            request.headers.push(("Content-Length".into(), body.len().to_string()));
            let whole = view.is_some() || no_r.is_some() || list || notice.is_some();
            if whole {
                request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Accept"));
                request.headers.push(("Accept".into(), "application/json".into()));
            }
            request.write_to(&mut to_runtime)?;
            to_runtime.write_all(&body)?;
            let replaced = |result: &Value| -> Option<Value> {
                if list {
                    return with_restart_tool(result);
                }
                if let Some(snapshot) = view
                    && snapshot_may_help(result)
                {
                    let cell = &arguments["cell_id"];
                    let (notebook, cell) = (uuid_value(&arguments["notebook_id"])?, uuid_value(cell).or_else(|| short_cell_id(cell))?);
                    return screenshot_result(&cell, snapshot(&session, &notebook, &cell)?, result);
                }
                let no_r = no_r.filter(|_| crate::mcp::refuses_r_for_want_of_it(tool, arguments, result))?;
                Some(tool_error(&no_r(), false))
            };
            let instead = |result: &Value| -> Option<Value> {
                let Some((older, notice)) = notice else { return replaced(result) };
                let mut result = replaced(result).unwrap_or_else(|| result.clone());
                told_of_older(&mut result, tool, &session, older, notice);
                Some(result)
            };
            if whole {
                if !relay_result(&request, &mut client, &mut from_runtime, &instead)? {
                    return Ok(());
                }
                continue;
            }
        }
        if !http::relay_response(&request, &mut reader, &mut from_runtime, &|_| {})? {
            return Ok(());
        }
    }
    Ok(())
}

const IDLE_CHECK: Duration = Duration::from_secs(1);

fn next_request(reader: &mut BufReader<wire::Shared>, client: &TcpStream, upstream: &TcpStream) -> io::Result<bool> {
    let before = client.read_timeout()?;
    client.set_read_timeout(Some(IDLE_CHECK))?;
    let waited = loop {
        match reader.fill_buf() {
            Ok(buffered) => break Ok(!buffered.is_empty()),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if closed(upstream) {
                    break Ok(false);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    client.set_read_timeout(before)?;
    waited
}

fn closed(socket: &TcpStream) -> bool {
    if socket.set_nonblocking(true).is_err() {
        return false;
    }
    let closed = match socket.peek(&mut [0]) {
        Ok(n) => n == 0,
        Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
    };
    let _ = socket.set_nonblocking(false);
    closed
}

fn told_of_older(result: &mut Value, tool: &str, session: &str, older: &Older, notice: &str) {
    if tool == "session_status"
        && result["isError"] != true
        && let Some(Value::Object(mut fields)) = result["content"][0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok())
    {
        fields.insert("other_version".into(), notice.into());
        result["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        older.told.lock().unwrap().insert(session.to_owned());
        return;
    }
    if let Some(content) = result["content"].as_array_mut()
        && older.told.lock().unwrap().insert(session.to_owned())
    {
        content.push(json!({ "type": "text", "text": format!("Note: {notice}") }));
    }
}

fn relay_result(request: &Head, client: &mut wire::Shared, from_runtime: &mut BufReader<wire::Shared>, instead: &dyn Fn(&Value) -> Option<Value>) -> io::Result<bool> {
    let mut response = loop {
        let response = Head::read(from_runtime)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    let framing = response.response_body("POST")?;
    let keeps_alive = request.keeps_alive() && response.keeps_alive() && framing != Framing::UntilClose;
    let mut body = http::read_body(from_runtime, framing)?;
    let json = response.header("Content-Type").is_some_and(|t| t.starts_with("application/json"));
    if response.status() == 200 && json
        && let Ok(mut message) = serde_json::from_slice::<Value>(&body)
        && let Some(result) = instead(&message["result"])
    {
        message["result"] = result;
        body = to_json(&message).into_bytes();
    }
    response.headers.retain(|(name, _)| !["Transfer-Encoding", "Content-Length", "Connection"].iter().any(|h| name.eq_ignore_ascii_case(h)));
    response.headers.push(("Content-Length".into(), body.len().to_string()));
    if !keeps_alive {
        response.headers.push(("Connection".into(), "close".into()));
    }
    response.write_to(client)?;
    client.write_all(&body)?;
    Ok(keeps_alive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Arc;

    fn guarded() -> (TcpStream, std::sync::mpsc::Receiver<String>) {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            while let Ok(Some(request)) = Head::read(&mut reader) {
                if request.header("Upgrade").is_some() {
                    socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n").unwrap();
                    let _ = io::copy(&mut reader, &mut socket);
                    seen_tx.send("closed".into()).unwrap();
                    return;
                }
                let body = http::read_body(&mut reader, request.request_body().unwrap()).unwrap();
                let body = format!("{} {}", request.line, String::from_utf8(body).unwrap());
                seen_tx.send(body.clone()).unwrap();
                http::respond(&mut socket, "200 OK", None, body.as_bytes(), true).unwrap();
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            let refuse = |session: &str, tool: &str, _: &Value| (session == "7" && tool == "execute_cell").then(|| "ArgumentError: older_runtime::No runs.".to_owned());
            let _ = serve_guarded(accepted, upstream, &refuse, None, None, None);
        });
        (client, seen)
    }

    fn post(client: &mut TcpStream, path: &str, session: &str, body: &str) -> String {
        write!(client, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Endeavor-Session: {session}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = Head::read(&mut reader).unwrap().unwrap();
        let body = http::read_body(&mut reader, head.response_body("POST").unwrap()).unwrap();
        format!("{} {}", head.status(), String::from_utf8(body).unwrap())
    }

    #[test]
    fn an_idle_connection_whose_runtime_went_closes_so_the_next_request_finds_the_new_one() {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        let (end_tx, end) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            let request = Head::read(&mut reader).unwrap().unwrap();
            http::read_body(&mut reader, request.request_body().unwrap()).unwrap();
            http::respond(&mut socket, "200 OK", None, b"first", true).unwrap();
            end.recv().unwrap();
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || done_tx.send(serve_guarded(accepted, upstream, &|_, _, _| None, None, None, None).is_ok()).unwrap());
        let read = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_cell","arguments":{}}}"#;
        assert_eq!(post(&mut client, "/mcp", "7", read), "200 first");
        std::thread::sleep(IDLE_CHECK + Duration::from_millis(300));
        assert!(done.try_recv().is_err(), "still open while its runtime is");
        end_tx.send(()).unwrap();
        assert_eq!(done.recv_timeout(IDLE_CHECK * 3), Ok(true), "the guard lets the connection go");
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(io::Read::read(&mut client, &mut [0; 16]).unwrap(), 0, "the agent sees it close while idle, before it sends anything");
    }

    #[test]
    fn a_refused_call_is_answered_here_and_the_rest_reach_the_runtime() {
        let (mut client, seen) = guarded();
        let run = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"execute_cell","arguments":{}}}"#;
        assert_eq!(
            post(&mut client, "/mcp", "7", run),
            r#"200 {"id":3,"jsonrpc":"2.0","result":{"content":[{"text":"{\"error\":\"older_runtime\",\"message\":\"No runs.\"}","type":"text"}],"isError":true}}"#
        );
        assert!(seen.try_recv().is_err(), "the runtime never saw it");
        let read = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_cell","arguments":{}}}"#;
        assert_eq!(post(&mut client, "/mcp", "7", read), format!("200 POST /mcp HTTP/1.1 {read}"), "on the same connection");
        assert_eq!(post(&mut client, "/mcp", "8", run), format!("200 POST /mcp HTTP/1.1 {run}"), "another session's");
        assert_eq!(post(&mut client, "/endeavor/call", "", run), format!("200 POST /endeavor/call HTTP/1.1 {run}"), "the app's own calls pass");
    }

    #[test]
    fn a_call_after_another_request_on_the_connection_is_still_checked() {
        let (mut client, seen) = guarded();
        write!(client, "GET /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Endeavor-Session: 7\r\n\r\n").unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = Head::read(&mut reader).unwrap().unwrap();
        let body = http::read_body(&mut reader, head.response_body("GET").unwrap()).unwrap();
        assert_eq!(String::from_utf8(body).unwrap(), "GET /mcp HTTP/1.1 ", "the GET reached the runtime");
        assert_eq!(seen.try_recv().unwrap(), "GET /mcp HTTP/1.1 ");
        let run = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"execute_cell","arguments":{}}}"#;
        assert_eq!(
            post(&mut client, "/mcp", "7", run),
            r#"200 {"id":3,"jsonrpc":"2.0","result":{"content":[{"text":"{\"error\":\"older_runtime\",\"message\":\"No runs.\"}","type":"text"}],"isError":true}}"#
        );
        assert!(seen.try_recv().is_err(), "the runtime never saw the run");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"endeavor/answer_run"}"#;
        assert_eq!(post(&mut client, "/endeavor/call", "", body), format!("200 POST /endeavor/call HTTP/1.1 {body}"), "a request with a body passes whole");
        assert_eq!(post(&mut client, "/mcp", "7", run).split(' ').next(), Some("200"));
        assert!(seen.recv().unwrap().starts_with("POST /endeavor/call"));
        assert!(seen.try_recv().is_err(), "nor the second");
    }

    #[test]
    fn a_websocket_passes_through_after_its_upgrade_until_the_client_closes() {
        let (mut client, seen) = guarded();
        assert_eq!(post(&mut client, "/edit", "", ""), "200 POST /edit HTTP/1.1 ", "Pluto's page passes");
        write!(client, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = Head::read(&mut reader).unwrap().unwrap();
        assert_eq!(head.status(), 101);
        for message in [&b"frame one"[..], &[0u8, 255, 13, 10, 13, 10][..]] {
            client.write_all(message).unwrap();
            let mut back = vec![0; message.len()];
            io::Read::read_exact(&mut reader, &mut back).unwrap();
            assert_eq!(back, message, "bytes, not requests, after the upgrade");
        }
        assert_eq!(seen.recv().unwrap(), "POST /edit HTTP/1.1 ");
        client.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(seen.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), "closed", "the runtime's end closes with it");
    }

    const NOTEBOOK: &str = "a0000000-0000-4000-8000-000000000001";
    const HTML: &str = "c0000000-0000-4000-8000-000000000001";
    const PLOT: &str = "c0000000-0000-4000-8000-000000000002";
    const WIDGET: &str = "c0000000-0000-4000-8000-000000000003";
    const BROKEN: &str = "c0000000-0000-4000-8000-000000000004";
    const UNDRAWN: &str = "c0000000-0000-4000-8000-000000000005";
    const PREVIEW: &str = "c0000000-0000-4000-8000-000000000006";

    fn viewing() -> (TcpStream, std::sync::mpsc::Receiver<String>, std::sync::mpsc::Receiver<String>) {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        let (accept_tx, accepts) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            while let Ok(Some(request)) = Head::read(&mut reader) {
                accept_tx.send(request.header("Accept").unwrap_or_default().to_owned()).unwrap();
                let body = http::read_body(&mut reader, request.request_body().unwrap()).unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                let cell = message["params"]["arguments"]["cell_id"].as_str().unwrap_or_default().to_lowercase();
                let image = |meta: Value| crate::mcp::image_result(&meta, "iVBORw0KGgoAAAA=".into());
                let result = match cell.as_str() {
                    HTML | UNDRAWN => tool_error(&format!("ArgumentError: no_image::Cell {cell}: its output (text/html) has no PNG rendering; read_cell shows it as text"), false),
                    PREVIEW => tool_error(&format!("ArgumentError: no_image::Cell {cell}: the notebook has no running worker (safe preview or not yet run), so its value can't be rendered"), false),
                    PLOT => image(json!({ "cell_id": cell, "shown_as": "image/svg+xml", "png_bytes": 10 })),
                    WIDGET => image(json!({ "cell_id": cell, "shown_as": "text/html", "png_bytes": 10, "source": "embedded image" })),
                    _ => tool_error(&format!("ArgumentError: no_image::Cell {cell} errored; read_cell shows the error"), false),
                };
                let reply = to_json(&json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
                http::write_chunk(&mut socket, reply.as_bytes()).unwrap();
                http::write_chunk(&mut socket, b"").unwrap();
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let snapshot = move |session: &str, notebook: &str, cell: &str| {
                asked_tx.send(format!("{session} {notebook} {cell}")).unwrap();
                (cell != UNDRAWN).then(|| Picture::from("c25hcHNob3Q=".to_owned()))
            };
            let _ = serve_guarded(accepted, upstream, &|_, _, _| None, Some(&snapshot), None, None);
        });
        (client, accepts, asked)
    }

    fn view(client: &mut TcpStream, cell: &str) -> Value {
        let call = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "view_cell_output", "arguments": { "notebook_id": NOTEBOOK, "cell_id": cell.to_uppercase() } } });
        write!(client, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Endeavor-Session: 7\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n{call}", call.to_string().len()).unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let head = Head::read(&mut reader).unwrap().unwrap();
        assert_eq!(head.status(), 200);
        assert!(head.header("Transfer-Encoding").is_none());
        let body = http::read_body(&mut reader, head.response_body("POST").unwrap()).unwrap();
        let message: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(message["id"], 5);
        message["result"].clone()
    }

    fn meta(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn an_output_the_runtime_has_no_picture_of_is_shown_as_the_notebook_draws_it() {
        let (mut client, accepts, asked) = viewing();
        let html = view(&mut client, HTML);
        assert_eq!(accepts.recv().unwrap(), "application/json", "never a stream to rewrite");
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {HTML}"), "the ids as the runtime reads them");
        assert_eq!(html["isError"], false);
        assert_eq!(html["content"][1], json!({ "type": "image", "data": "c25hcHNob3Q=", "mimeType": "image/png" }));
        assert_eq!(meta(&html), json!({ "cell_id": HTML, "source": "screenshot", "png_bytes": 8, "shown_as": "text/html" }), "what it is, from the error");

        let widget = view(&mut client, WIDGET);
        assert_eq!((&meta(&widget)["source"], &meta(&widget)["shown_as"]), (&json!("screenshot"), &json!("text/html")));
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {WIDGET}"));
        let preview = meta(&view(&mut client, PREVIEW));
        assert_eq!((&preview["source"], preview.get("shown_as")), (&json!("screenshot"), None), "the runtime didn't say");
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {PREVIEW}"));

        let plot = view(&mut client, PLOT);
        assert_eq!(meta(&plot)["shown_as"], "image/svg+xml");
        assert_eq!(plot["content"][1]["data"], "iVBORw0KGgoAAAA=");
        let broken = view(&mut client, BROKEN);
        assert_eq!(broken["isError"], true);
        assert!(asked.try_recv().is_err(), "no snapshot for either");

        let undrawn = view(&mut client, UNDRAWN);
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {UNDRAWN}"));
        assert_eq!(undrawn["isError"], true);
        assert!(undrawn["content"][0]["text"].as_str().unwrap().contains("has no PNG rendering"));
    }

    fn without_r(older_runtime: bool) -> TcpStream {
        without_r_told(older_runtime, None, Arc::default())
    }

    fn without_r_told(older_runtime: bool, notice: Option<&'static str>, told: Arc<Mutex<HashSet<String>>>) -> TcpStream {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            while let Ok(Some(request)) = Head::read(&mut reader) {
                let body = http::read_body(&mut reader, request.request_body().unwrap()).unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                let path = message["params"]["arguments"]["path"].as_str().unwrap_or_default();
                let result = if message["params"]["name"] == "session_status" {
                    json!({ "content": [{ "type": "text", "text": "{\"pluto\":\"running\"}" }], "isError": false })
                } else if path.is_empty() || path.ends_with(".jl") {
                    json!({ "content": [{ "type": "text", "text": "[]" }], "isError": false })
                } else {
                    tool_error(&format!("ArgumentError: invalid_path::Notebook path must end in .jl: '{path}'"), false)
                };
                let reply = to_json(&json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }));
                http::respond(&mut socket, "200 OK", Some("application/json"), reply.as_bytes(), true).unwrap();
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            let no_r = || "unsupported::No R here; restart it.".to_owned();
            let older = Older { no_r: &no_r, notice: notice.map(str::to_owned), told: &told };
            let _ = serve_guarded(accepted, upstream, &|_, _, _| None, None, older_runtime.then_some(&older), None);
        });
        client
    }

    fn call_tool(client: &mut TcpStream, tool: &str, path: &str) -> Value {
        let call = json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": tool, "arguments": { "path": path } } }).to_string();
        let reply = post(client, "/mcp", "7", &call);
        let reply: Value = serde_json::from_str(reply.strip_prefix("200 ").unwrap()).unwrap();
        assert_eq!(reply["id"], 9);
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn a_runtime_from_before_r_notebooks_that_refuses_an_r_path_is_said_to_have_none() {
        let mut client = without_r(true);
        assert_eq!(call_tool(&mut client, "new_notebook", "/work/a.R"), json!({ "error": "unsupported", "message": "No R here; restart it." }));
        assert_eq!(call_tool(&mut client, "open_notebook", "/work/b.r"), json!({ "error": "unsupported", "message": "No R here; restart it." }), "on the same connection");
        assert_eq!(call_tool(&mut client, "new_notebook", "/work/c.jl"), json!([]), "a Julia path passes");
        assert_eq!(call_tool(&mut client, "new_notebook", "/work/d.txt")["message"], "Notebook path must end in .jl: '/work/d.txt'", "and so does a path of no notebook's");

        let mut client = without_r(false);
        assert_eq!(call_tool(&mut client, "new_notebook", "/work/a.R")["message"], "Notebook path must end in .jl: '/work/a.R'", "a runtime of this build's interface answers as it does");
    }

    fn call_from(client: &mut TcpStream, session: &str, tool: &str, path: &str) -> Value {
        let call = json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": tool, "arguments": { "path": path } } }).to_string();
        let reply: Value = serde_json::from_str(post(client, "/mcp", session, &call).strip_prefix("200 ").unwrap()).unwrap();
        reply["result"].clone()
    }

    #[test]
    fn each_session_is_told_once_of_another_builds_runtime_and_session_status_says_it_every_time() {
        let told = Arc::default();
        let mut client = without_r_told(true, Some("Endeavor on lab was started by an older version."), Arc::clone(&told));
        let first = call_from(&mut client, "7", "list_notebooks", "");
        assert_eq!(first["content"][0]["text"], "[]", "the runtime's answer stays first");
        assert_eq!(first["content"][1]["text"], "Note: Endeavor on lab was started by an older version.");
        assert_eq!(call_from(&mut client, "7", "list_notebooks", "")["content"].as_array().unwrap().len(), 1, "once per session");
        let status = call_from(&mut client, "7", "session_status", "");
        let fields: Value = serde_json::from_str(status["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(fields, json!({ "pluto": "running", "other_version": "Endeavor on lab was started by an older version." }), "every time");
        assert_eq!(status["content"].as_array().unwrap().len(), 1, "with no second note");
        let other = call_from(&mut client, "8", "open_notebook", "/work/c.R");
        assert_eq!((other["isError"].clone(), other["content"][1]["text"].as_str().map(|t| t.starts_with("Note: "))), (json!(true), Some(true)), "another session is told too, after a refusal it explains");
        assert_eq!(other["content"][0]["text"], json!(tool_error("unsupported::No R here; restart it.", false)["content"][0]["text"]));
        let status = call_from(&mut client, "9", "session_status", "");
        assert_eq!(status["content"].as_array().unwrap().len(), 1);
        assert_eq!(call_from(&mut client, "9", "list_notebooks", "")["content"].as_array().unwrap().len(), 1, "told by the status");
        let waits = json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "approve_install", "arguments": {} } }).to_string();
        let waited: Value = serde_json::from_str(post(&mut client, "/mcp", "10", &waits).strip_prefix("200 ").unwrap()).unwrap();
        assert_eq!(waited["result"]["content"].as_array().unwrap().len(), 1, "no note on a call that may wait");
        assert_eq!(call_from(&mut client, "10", "list_notebooks", "")["content"][1]["text"].as_str().map(|t| t.starts_with("Note: ")), Some(true));
        let mut again = without_r_told(true, Some("Endeavor on lab was started by an older version."), told);
        assert_eq!(call_from(&mut again, "7", "list_notebooks", "")["content"].as_array().unwrap().len(), 1);
    }

    fn with_restart(answer: RestartAnswer) -> (TcpStream, std::sync::mpsc::Receiver<String>, std::sync::mpsc::Receiver<String>) {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            while let Ok(Some(request)) = Head::read(&mut reader) {
                let body = http::read_body(&mut reader, request.request_body().unwrap()).unwrap();
                let message: Value = serde_json::from_slice(&body).unwrap();
                seen_tx.send(message["method"].as_str().unwrap_or_default().to_owned()).unwrap();
                let result = match message["method"].as_str() {
                    Some("tools/list") => json!({ "tools": [{ "name": "read_cell" }] }),
                    _ => json!({ "content": [{ "type": "text", "text": "ran" }] }),
                };
                let reply = to_json(&json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }));
                http::respond(&mut socket, "200 OK", Some("application/json"), reply.as_bytes(), true).unwrap();
            }
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (asked_tx, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let asked_tx = std::sync::Mutex::new(asked_tx);
            let ask = move |session: &str, reason: &str, _: &dyn Fn() -> bool, deadline: Instant| {
                assert!(deadline > Instant::now(), "it waits a while");
                asked_tx.lock().unwrap().send(format!("{session} {reason}")).unwrap();
                answer.clone()
            };
            let _ = serve_guarded(accepted, upstream, &|_, _, _| None, None, None, Some((&ask, "lab")));
        });
        (client, seen, asked)
    }

    fn rpc(client: &mut TcpStream, method: &str, params: Value) -> Value {
        let message = json!({ "jsonrpc": "2.0", "id": 5, "method": method, "params": params }).to_string();
        let reply = post(client, "/mcp", "7", &message);
        serde_json::from_str::<Value>(reply.strip_prefix("200 ").unwrap()).unwrap()["result"].clone()
    }

    #[test]
    fn the_agent_gets_request_restart_and_the_client_answers_it_not_the_runtime() {
        let (mut client, seen, asked) = with_restart(RestartAnswer::Restarting);
        let tools = rpc(&mut client, "tools/list", json!({}));
        let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["read_cell", REQUEST_RESTART], "added to the runtime's own");
        assert_eq!(seen.recv().unwrap(), "tools/list");

        let result = rpc(&mut client, "tools/call", json!({ "name": REQUEST_RESTART, "arguments": { "reason": " Julia stopped answering. " } }));
        assert_eq!(asked.try_recv().unwrap(), "7 Julia stopped answering.");
        assert_eq!(result.get("isError"), None);
        assert!(result["content"][0]["text"].as_str().unwrap().starts_with("The user restarted Endeavor on lab."));
        assert!(seen.try_recv().is_err(), "the runtime never saw it");
        let mut rest = Vec::new();
        assert_eq!(client.read_to_end(&mut rest).unwrap(), 0, "the connection closes after the answer: the runtime it led to may be gone");

        let (mut client, _seen, asked) = with_restart(RestartAnswer::Restarting);
        let no_reason = rpc(&mut client, "tools/call", json!({ "name": REQUEST_RESTART, "arguments": {} }));
        assert_eq!(no_reason["isError"], true);
        assert!(asked.try_recv().is_err(), "the user isn't asked without a reason");

        let (mut client, seen, _asked) = with_restart(RestartAnswer::Restarting);
        assert_eq!(rpc(&mut client, "tools/call", json!({ "name": "read_cell", "arguments": {} }))["content"][0]["text"], "ran", "other calls pass");
        assert_eq!(seen.recv().unwrap(), "tools/call");
    }

    #[test]
    fn a_declined_or_unanswered_restart_says_not_to_stop_it_another_way() {
        let text = |answer: RestartAnswer| {
            let (mut client, _seen, _asked) = with_restart(answer);
            let result = rpc(&mut client, "tools/call", json!({ "name": REQUEST_RESTART, "arguments": { "reason": "Older version." } }));
            assert_eq!(result["isError"], true);
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let declined = text(RestartAnswer::Declined);
        assert_eq!(declined["error"], "not_approved");
        assert!(declined["message"].as_str().unwrap().contains("don't stop it yourself"));
        let unanswered = text(RestartAnswer::Unanswered);
        assert_eq!(unanswered["error"], "waiting_for_user");
        assert!(unanswered["message"].as_str().unwrap().contains("Don't stop or restart it any other way"));
        assert_eq!(text(RestartAnswer::Unavailable("ArgumentError: restarting::Already restarting.".into()))["message"], "Already restarting.");
    }

    #[test]
    fn the_schema_says_only_the_user_restarts() {
        let schema = restart_schema();
        let description = schema["description"].as_str().unwrap();
        assert!(description.contains("nothing restarts unless they press it"));
        assert!(!description.contains("stop_machine"), "no other way to stop it is named");
        assert_eq!(schema["inputSchema"]["required"], json!(["reason"]));
    }
}

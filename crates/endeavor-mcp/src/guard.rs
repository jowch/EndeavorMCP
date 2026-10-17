//! The app's watch over a runtime from an older build: the app relays every
//! connection to a host's runtime, and on one that is older than the app it
//! answers the agent's tool calls the app won't let that runtime carry out,
//! passing everything else through unchanged. And where the app can draw a
//! notebook, it shows the agent a cell's output as the notebook draws it when
//! the runtime has no picture of it (`Snapshot`).

use std::io::{self, BufReader, Write};
use std::net::TcpStream;

use serde_json::{Value, json};

use crate::http::{self, Framing, Head};
use crate::mcp::{to_json, tool_error};
use crate::notebooks::{screenshot_result, snapshot_may_help, uuid_value};

/// A picture of a cell's output as the user's notebook draws it, given the session's key, the
/// notebook's id and the cell's: a PNG, in base64. None if it can't be taken (the notebook isn't
/// open in the app, or the cell isn't drawn).
pub type Snapshot = dyn Fn(&str, &str, &str) -> Option<String> + Send + Sync;

/// Serve `client`'s requests through `upstream`, a connection to the runtime's
/// port, one at a time. An agent's MCP tool call that `refuse` refuses, given
/// the session's key (its `X-Endeavor-Session`), the tool and its arguments,
/// fails here with that text as its error. Every other request (the app's own
/// calls and event stream, Pluto's page and its WebSocket, and the agent's
/// other messages) passes through as it is, and the requests after it on the
/// same connection are checked the same way. With `snapshot`, a `view_cell_output`
/// the runtime has no picture for gets the snapshot instead (`relay_view`).
pub fn serve_guarded(client: TcpStream, upstream: TcpStream, refuse: &dyn Fn(&str, &str, &Value) -> Option<String>, snapshot: Option<&Snapshot>) -> io::Result<()> {
    let mut client = wire::Shared::new(client);
    let mut reader = BufReader::new(client.clone());
    let mut to_runtime = wire::Shared::new(upstream);
    let mut from_runtime = BufReader::new(to_runtime.clone());
    while let Some(mut request) = Head::read(&mut reader)? {
        if request.method() != "POST" || request.path() != "/mcp" {
            request.write_to(&mut to_runtime)?;
            http::copy_body(&mut reader, &mut to_runtime, &mut request.request_body()?)?;
        } else {
            let body = http::read_body(&mut reader, request.request_body()?)?;
            let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let session = request.header("X-Endeavor-Session").unwrap_or_default().to_owned();
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
            let view = snapshot.filter(|_| message["method"] == "tools/call" && !message["id"].is_null() && params["name"] == "view_cell_output");
            request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
            request.headers.push(("Content-Length".into(), body.len().to_string()));
            if view.is_some() {
                // Only a call that waits on the user streams its answer; this keeps the answer one JSON body even so.
                request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Accept"));
                request.headers.push(("Accept".into(), "application/json".into()));
            }
            request.write_to(&mut to_runtime)?;
            to_runtime.write_all(&body)?;
            if let Some(snapshot) = view {
                let arguments = params.get("arguments").unwrap_or(&Value::Null);
                if !relay_view(&request, &mut client, &mut from_runtime, &|| {
                    let (notebook, cell) = (uuid_value(&arguments["notebook_id"])?, uuid_value(&arguments["cell_id"])?);
                    screenshot_result(&cell, snapshot(&session, &notebook, &cell)?)
                })? {
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

/// Pass on the runtime's answer to a `view_cell_output`, unless `snapshot_may_help` and `snapshot`
/// gives a result with a picture: then the answer is that. Whether the client connection can carry
/// another request.
fn relay_view(request: &Head, client: &mut wire::Shared, from_runtime: &mut BufReader<wire::Shared>, snapshot: &dyn Fn() -> Option<Value>) -> io::Result<bool> {
    let mut response = loop {
        let response = Head::read(from_runtime)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        // `100 Continue` comes before the real response; the client sent its whole body already.
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
        && snapshot_may_help(&message["result"])
        && let Some(result) = snapshot()
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
    use std::net::TcpListener;

    /// A runtime that answers every request with what it was sent, and an
    /// upgrade with `101` and then an echo, and a client connected to it
    /// through `serve_guarded`, refusing runs of session 7.
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
            let _ = serve_guarded(accepted, upstream, &refuse, None);
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

    /// A runtime that answers `view_cell_output` as a real one would for each of the cells above,
    /// and a client connected to it through `serve_guarded` with a snapshot of every cell but
    /// `UNDRAWN`. What the runtime is sent as `Accept`, and the snapshots asked for.
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
                // Chunked, so the guard can't just pass a length on.
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
                (cell != UNDRAWN).then(|| "c25hcHNob3Q=".to_owned())
            };
            let _ = serve_guarded(accepted, upstream, &|_, _, _| None, Some(&snapshot));
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
        assert_eq!(meta(&html), json!({ "cell_id": HTML, "source": "screenshot", "png_bytes": 8 }));

        // A picture from the output's HTML may be only part of it: the notebook's picture is what the user sees.
        let widget = view(&mut client, WIDGET);
        assert_eq!(meta(&widget)["source"], "screenshot");
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {WIDGET}"));
        // In safe preview the runtime can't render the value, but the notebook still shows the output.
        assert_eq!(meta(&view(&mut client, PREVIEW))["source"], "screenshot");
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {PREVIEW}"));

        // The value's own picture stands, and so does an error the notebook's picture wouldn't help with.
        let plot = view(&mut client, PLOT);
        assert_eq!(meta(&plot)["shown_as"], "image/svg+xml");
        assert_eq!(plot["content"][1]["data"], "iVBORw0KGgoAAAA=");
        let broken = view(&mut client, BROKEN);
        assert_eq!(broken["isError"], true);
        assert!(asked.try_recv().is_err(), "no snapshot for either");

        // No snapshot: the runtime's answer as it was.
        let undrawn = view(&mut client, UNDRAWN);
        assert_eq!(asked.try_recv().unwrap(), format!("7 {NOTEBOOK} {UNDRAWN}"));
        assert_eq!(undrawn["isError"], true);
        assert!(undrawn["content"][0]["text"].as_str().unwrap().contains("has no PNG rendering"));
    }
}

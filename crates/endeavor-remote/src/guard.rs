//! The app's watch over a runtime from an older build: the app relays every
//! connection to a host's runtime, and on one that is older than the app it
//! answers the agent's tool calls the app won't let that runtime carry out,
//! passing everything else through unchanged.

use std::io::{self, BufReader, Write};
use std::net::TcpStream;

use serde_json::{Value, json};

use crate::http::{self, Head};
use crate::mcp::{to_json, tool_error};

/// Serve `client`'s requests through `upstream`, a connection to the runtime's
/// port, one at a time. An agent's MCP tool call that `refuse` refuses, given
/// the session's key (its `X-Endeavor-Session`), the tool and its arguments,
/// fails here with that text as its error. Every other request (the app's own
/// calls and event stream, Pluto's page and its WebSocket, and the agent's
/// other messages) passes through as it is, and the requests after it on the
/// same connection are checked the same way.
pub fn serve_guarded(client: TcpStream, upstream: TcpStream, refuse: &dyn Fn(&str, &str, &Value) -> Option<String>) -> io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let mut from_runtime = BufReader::new(upstream.try_clone()?);
    let mut to_runtime = upstream;
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
            request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
            request.headers.push(("Content-Length".into(), body.len().to_string()));
            request.write_to(&mut to_runtime)?;
            to_runtime.write_all(&body)?;
        }
        if !http::relay_response(&request, &mut reader, &mut from_runtime, &|_| {})? {
            return Ok(());
        }
    }
    Ok(())
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
            let _ = serve_guarded(accepted, upstream, &refuse);
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
}

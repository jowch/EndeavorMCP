//! The app's watch over a runtime from an older build: the app relays every
//! connection to a host's runtime, and on one that is older than the app it
//! answers the agent's tool calls the app won't let that runtime carry out,
//! passing everything else through unchanged.

use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;

use serde_json::{Value, json};

use crate::http::{self, Head};
use crate::mcp::{to_json, tool_error};

/// Serve `client`'s requests through `upstream`, a connection to the runtime's
/// bridge. An agent's MCP tool call that `refuse` refuses, given the session's
/// key (its `X-Endeavor-Session`), the tool and its arguments, fails here with
/// that text as its error. The first request that isn't the agent's MCP
/// message (the app's own calls and event stream) and everything after it pass
/// through as they are.
pub fn serve_guarded(client: TcpStream, upstream: TcpStream, refuse: &dyn Fn(&str, &str, &Value) -> Option<String>) -> io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let mut from_runtime = BufReader::new(upstream.try_clone()?);
    let mut to_runtime = upstream;
    while let Some(mut request) = Head::read(&mut reader)? {
        let target = request.target();
        // Streamable HTTP posts each message to /mcp; the older SSE transport to /message.
        let streamable = target.starts_with("/mcp");
        if request.method() != "POST" || !(streamable || target.starts_with("/message")) {
            request.write_to(&mut to_runtime)?;
            return pass_through(reader, client, from_runtime, to_runtime);
        }
        let body = http::read_body(&mut reader, request.request_body()?)?;
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let session = request.header("X-Endeavor-Session").unwrap_or_default().to_owned();
        let params = &message["params"];
        let refusal = (message["method"] == "tools/call" && !message["id"].is_null())
            .then(|| refuse(&session, params["name"].as_str().unwrap_or_default(), params.get("arguments").unwrap_or(&Value::Null)))
            .flatten();
        if let Some(why) = refusal {
            if streamable {
                let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": tool_error(&why, false) });
                http::respond(&mut client, "200 OK", Some("application/json"), to_json(&reply).as_bytes(), request.keeps_alive())?;
            } else {
                // Its reply would go out on the SSE stream; failing the post fails the call.
                let said = why.split_once("::").map_or(why.as_str(), |(_, message)| message);
                http::respond(&mut client, "403 Forbidden", Some("text/plain"), said.as_bytes(), request.keeps_alive())?;
            }
            if !request.keeps_alive() {
                return Ok(());
            }
            continue;
        }
        request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
        request.headers.push(("Content-Length".into(), body.len().to_string()));
        request.write_to(&mut to_runtime)?;
        to_runtime.write_all(&body)?;
        let response = loop {
            let response = Head::read(&mut from_runtime)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            response.write_to(&mut client)?;
            if !(100..200).contains(&response.status()) {
                break response;
            }
        };
        let mut framing = response.response_body("POST")?;
        http::relay_body(&mut from_runtime, &mut client, &mut framing)?;
        if !(request.keeps_alive() && response.keeps_alive()) || framing == http::Framing::UntilClose {
            return Ok(());
        }
    }
    Ok(())
}

/// Copy both ways until either side closes, starting with what is already read.
fn pass_through(mut reader: BufReader<TcpStream>, mut client: TcpStream, mut from_runtime: BufReader<TcpStream>, mut to_runtime: TcpStream) -> io::Result<()> {
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut reader, &mut to_runtime);
        let _ = to_runtime.shutdown(std::net::Shutdown::Write);
    });
    let mut buffer = [0; 16 * 1024];
    loop {
        let n = from_runtime.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        client.write_all(&buffer[..n])?;
    }
    let _ = client.shutdown(std::net::Shutdown::Both);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A runtime's bridge that answers every request with what it was sent,
    /// and a client connected to it through `serve_guarded`, refusing runs
    /// of session 7.
    fn guarded() -> (TcpStream, std::sync::mpsc::Receiver<String>) {
        let runtime = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = runtime.local_addr().unwrap().port();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (socket, _) = runtime.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut socket = socket;
            while let Ok(Some(request)) = Head::read(&mut reader) {
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
        assert_eq!(post(&mut client, "/message?sessionId=s", "7", run), "403 No runs.", "the older SSE transport");
        assert_eq!(post(&mut client, "/call", "", run), format!("200 POST /call HTTP/1.1 {run}"), "the app's own calls pass");
    }
}

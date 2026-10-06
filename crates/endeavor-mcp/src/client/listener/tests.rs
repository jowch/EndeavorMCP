use std::io::{Read, Write};
use std::net::TcpStream;

use super::*;

/// A listener for lab-server whose runtime has gone away.
fn away() -> Arc<Listener> {
    let listener = Listener::start("lab-server").unwrap();
    let mux = Mux::new(std::io::sink());
    listener.attach(mux.clone(), "secret".into());
    listener.forget(&mux);
    listener
}

/// The raw HTTP response to POST `body` to `/mcp` on the listener's port.
fn post(listener: &Listener, token: &str, body: &str) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", listener.port())).unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    response
}

const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_notebooks"}}"#;

#[test]
fn a_tool_call_while_the_server_is_away_fails_with_a_reason_to_retry() {
    let response = post(&away(), "secret", r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_notebooks","arguments":{}}}"#);
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert_eq!(
        body,
        r#"{"id":7,"jsonrpc":"2.0","result":{"content":[{"text":"Endeavor lost the connection to lab-server and is reconnecting by itself. Try again in a moment.","type":"text"}],"isError":true}}"#
    );
}

#[test]
fn a_notification_while_the_server_is_away_is_accepted() {
    let response = post(&away(), "secret", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert!(response.starts_with("HTTP/1.1 202 Accepted\r\n"), "{response}");
}

#[test]
fn a_ping_while_the_server_is_away_is_answered() {
    let response = post(&away(), "secret", r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#);
    assert!(response.ends_with("\r\n\r\n{\"id\":\"p\",\"jsonrpc\":\"2.0\",\"result\":{}}"), "{response}");
}

#[test]
fn malformed_input_while_the_server_is_away_is_a_bad_request() {
    let listener = away();
    assert!(post(&listener, "secret", "{not json").starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(post(&listener, "wrong", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).starts_with("HTTP/1.1 401 Unauthorized\r\n"));
}

#[test]
fn plutos_page_while_the_server_is_away_is_closed_unanswered() {
    let mut socket = TcpStream::connect(("127.0.0.1", away().port())).unwrap();
    socket.write_all(b"GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: endeavor-abc=secret\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert_eq!(response, "");
}

#[test]
fn restarting_julia_says_so() {
    let listener = Listener::start("This Mac").unwrap();
    listener.attach(Mux::new(std::io::sink()), "secret".into());
    listener.restarting();
    let response = post(&listener, "secret", LIST);
    assert!(response.contains(r#""text":"Endeavor is restarting Julia on This Mac. Try again in a moment.""#), "{response}");
}

#[test]
fn a_restart_that_fails_says_so_instead_of_restarting_forever() {
    let listener = Listener::start("This Mac").unwrap();
    listener.attach(Mux::new(std::io::sink()), "secret".into());
    // Before restarting() ran, there's nothing to correct: still up, a no-op.
    listener.restart_failed();
    assert!(matches!(&*listener.upstream.lock().unwrap(), Upstream::Up { .. }));
    listener.restarting();
    listener.restart_failed();
    let response = post(&listener, "secret", LIST);
    assert!(response.contains(r#""text":"Julia on This Mac couldn't start. Use Restart Julia to try again.""#), "{response}");
}

#[test]
fn stopping_on_purpose_says_so_not_that_it_reconnects_by_itself() {
    let listener = Listener::start("lab-server").unwrap();
    let mux = Mux::new(std::io::sink());
    listener.attach(mux.clone(), "secret".into());
    listener.disconnected();
    let response = post(&listener, "secret", LIST);
    assert!(response.contains(r#""text":"Endeavor isn't connected to lab-server. Reconnect it to use its notebook again.""#), "{response}");
    // The drop that follows a deliberate stop doesn't overwrite that with "reconnecting by itself".
    listener.forget(&mux);
    assert_eq!(post(&listener, "secret", LIST), response);
}

#[test]
fn a_caller_can_word_the_messages_that_name_the_apps_controls() {
    let messages = Messages { restart_failed: |name| format!("Julia on {name} didn't start. Call use_machine again."), not_connected: |name| format!("Not connected to {name}. Call use_machine.") };
    let listener = Listener::new("lab-server", None, messages).unwrap();
    let mux = Mux::new(std::io::sink());
    listener.attach(mux, "secret".into());
    listener.restarting();
    listener.restart_failed();
    assert!(post(&listener, "secret", LIST).contains(r#""text":"Julia on lab-server didn't start. Call use_machine again.""#));
    listener.disconnected();
    assert!(post(&listener, "secret", LIST).contains(r#""text":"Not connected to lab-server. Call use_machine.""#));
}

#[test]
fn a_connection_before_any_runtime_is_closed() {
    let mut socket = TcpStream::connect(("127.0.0.1", Listener::start("lab-server").unwrap().port())).unwrap();
    socket.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert_eq!(response, "");
}

#[test]
fn the_refuse_hook_sees_the_session_tool_and_arguments() {
    let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
    let record = seen.clone();
    let refuse: Refuse = Box::new(move |session, tool, arguments| {
        record.lock().unwrap().push((session.to_owned(), tool.to_owned(), arguments.clone()));
        (tool == "execute_cell").then(|| "This runtime is from an older build.".to_owned())
    });
    let listener = Listener::with_refuse("lab-server", refuse).unwrap();
    listener.attach(Mux::new(std::io::sink()), "secret".into());
    let mut socket = TcpStream::connect(("127.0.0.1", listener.port())).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"execute_cell","arguments":{"cell_id":"a"}}}"#;
    let request = format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Endeavor-Session: 7\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    socket.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = socket.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n") && response.contains("This runtime is from an older build.") && response.contains(r#""isError":true"#), "{response}");
    assert_eq!(*seen.lock().unwrap(), [("7".to_owned(), "execute_cell".to_owned(), serde_json::json!({ "cell_id": "a" }))]);
}

#[test]
fn a_drop_then_a_deliberate_stop_says_the_stop() {
    let listener = away();
    assert!(post(&listener, "secret", LIST).contains("reconnecting by itself"));
    listener.disconnected();
    assert!(post(&listener, "secret", LIST).contains("Endeavor isn't connected to lab-server."));
    listener.restarting();
    assert!(post(&listener, "secret", LIST).contains("Endeavor is restarting Julia on lab-server."));
    listener.restart_failed();
    assert!(post(&listener, "secret", LIST).contains("Julia on lab-server couldn't start."));
}

#[test]
fn a_helper_the_client_let_go_is_not_reconnecting() {
    let listener = Listener::start("lab-server").unwrap();
    let mux = Mux::new(std::io::sink());
    listener.attach(mux.clone(), "secret".into());
    listener.left(&mux);
    let response = post(&listener, "secret", LIST);
    assert!(response.contains("Endeavor isn't connected to lab-server.") && !response.contains("reconnecting"), "{response}");
    // Another channel's listener use isn't undone by an older channel's end.
    let (newer, older) = (Mux::new(std::io::sink()), Mux::new(std::io::sink()));
    listener.attach(newer, "secret".into());
    listener.left(&older);
    assert!(matches!(&*listener.upstream.lock().unwrap(), Upstream::Up { .. }));
}

#[test]
fn a_failing_accept_backs_off_up_to_a_second_and_starts_over_after_one_works() {
    let mut backoff = Backoff::default();
    let ms = |(pause, first): (Duration, bool)| (pause.as_millis(), first);
    let failures: Vec<_> = (0..7).map(|_| ms(backoff.failed())).collect();
    assert_eq!(failures, [(50, true), (100, false), (200, false), (400, false), (800, false), (1000, false), (1000, false)]);
    backoff.accepted();
    assert_eq!(ms(backoff.failed()), (50, true), "reported again after a success");
}

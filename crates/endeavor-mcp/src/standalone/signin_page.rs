use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wire::askpass::{Ask, Kind};

use crate::client::Reply;
use crate::http::{self, Head};

const PATH: &str = "/sign-in/";

const MAX_BODY: u64 = 4096;

const KEEP: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(super) struct SignIn {
    shared: Arc<Shared>,
    port: OnceLock<Option<u16>>,
}

#[derive(Default)]
struct Shared {
    pages: Mutex<HashMap<String, Page>>,
    answered: Condvar,
}

struct Page {
    machine: String,
    ask: Ask,
    answer: Option<Option<String>>,
    asked: Instant,
}

impl SignIn {
    pub(super) fn ask(&self, machine: &str, ask: &Ask, wait: Duration, open: &dyn Fn(&str) -> bool) -> Reply {
        let Some(port) = self.port() else { return Reply::GaveUp };
        let Ok(secret) = crate::random_hex::<16>() else { return Reply::GaveUp };
        let page = Page { machine: machine.to_owned(), ask: ask.clone(), answer: None, asked: Instant::now() };
        self.shared.pages.lock().unwrap().insert(secret.clone(), page);
        if !open(&format!("http://127.0.0.1:{port}{PATH}{secret}")) {
            self.shared.pages.lock().unwrap().remove(&secret);
            return Reply::GaveUp;
        }
        let until = Instant::now() + wait.min(KEEP);
        let mut pages = self.shared.pages.lock().unwrap();
        loop {
            if pages.get(&secret).is_some_and(|page| page.answer.is_some()) {
                return match pages.remove(&secret).and_then(|page| page.answer).flatten() {
                    Some(text) => Reply::Answered(text),
                    None => Reply::Refused,
                };
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                pages.remove(&secret);
                return Reply::GaveUp;
            }
            pages = self.shared.answered.wait_timeout(pages, left).unwrap().0;
        }
    }

    fn port(&self) -> Option<u16> {
        *self.port.get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| eprintln!("endeavor: couldn't start the sign-in page: {e}")).ok()?;
            let port = listener.local_addr().ok()?.port();
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let shared = shared.clone();
                    std::thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = serve(stream, port, &shared);
                    });
                }
            });
            Some(port)
        })
    }
}

fn serve(stream: TcpStream, port: u16, shared: &Shared) -> std::io::Result<()> {
    let stream = wire::Shared::new(stream);
    let mut out = stream.clone();
    let mut reader = BufReader::new(stream);
    let Some(head) = Head::read(&mut reader)? else { return Ok(()) };
    let body = match (head.method(), head.request_body()) {
        ("POST", Ok(http::Framing::Length(length))) if length <= MAX_BODY => String::from_utf8_lossy(&http::read_body(&mut reader, http::Framing::Length(length))?).into_owned(),
        _ => String::new(),
    };
    let ours = |value: Option<&str>, scheme: &str| value.is_some_and(|v| v == format!("{scheme}127.0.0.1:{port}") || v == format!("{scheme}localhost:{port}"));
    let secret = head.path().strip_prefix(PATH).filter(|_| ours(head.header("Host"), "")).map(str::to_owned);
    let page = |status: &str, body: String, out: &mut wire::Shared| {
        let headers = [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "same-origin"),
            ("X-Frame-Options", "DENY"),
            ("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'"),
        ];
        http::respond_with(out, status, Some("text/html; charset=utf-8"), &headers, html(&body).as_bytes(), false)
    };
    let mut pages = shared.pages.lock().unwrap();
    let Some(entry) = secret.as_ref().and_then(|secret| pages.get_mut(secret)).filter(|page| page.asked.elapsed() < KEEP) else {
        drop(pages);
        return page("404 Not Found", "<h1>This question is gone</h1><p>It was answered, or the server stopped waiting. Endeavor asks again on the next try.</p>".to_owned(), &mut out);
    };
    match head.method() {
        "GET" => {
            let body = match &entry.answer {
                Some(_) => SENT.to_owned(),
                None => question(entry),
            };
            drop(pages);
            page("200 OK", body, &mut out)
        }
        "POST" => {
            let same_site = head.header("Sec-Fetch-Site").is_none_or(|site| site == "same-origin");
            if !ours(head.header("Origin"), "http://") || !same_site {
                drop(pages);
                return page("403 Forbidden", "<h1>Not from this page</h1>".to_owned(), &mut out);
            }
            if entry.answer.is_some() {
                drop(pages);
                return page("200 OK", SENT.to_owned(), &mut out);
            }
            let fields = form(&body);
            let field = |name: &str| fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            let answer = if field("cancel").is_some() {
                None
            } else {
                match (entry.ask.kind, field("answer")) {
                    (Kind::Secret, Some(text)) => Some(text),
                    (Kind::YesNo, Some(word)) if word == "yes" || word == "no" => Some(word),
                    (Kind::Confirm, Some(_)) => Some(String::new()),
                    _ => {
                        drop(pages);
                        return page("400 Bad Request", "<h1>No answer</h1><p>Go back and answer, or click Cancel.</p>".to_owned(), &mut out);
                    }
                }
            };
            entry.answer = Some(answer);
            drop(pages);
            shared.answered.notify_all();
            page("200 OK", SENT.to_owned(), &mut out)
        }
        _ => {
            drop(pages);
            http::respond(&mut out, "405 Method Not Allowed", None, b"", false)
        }
    }?;
    out.flush()
}

const SENT: &str = "<h1>Sent</h1><p>Your answer went to ssh, and Endeavor goes on connecting. You can close this page.</p>";

fn question(page: &Page) -> String {
    let machine = escape(&page.machine);
    let prompt = escape(&page.ask.prompt);
    let cancel = "<button name=\"cancel\" value=\"1\" formnovalidate>Cancel</button>";
    match page.ask.kind {
        Kind::Secret => format!(
            "<h1>Sign in to {machine}</h1><p>Endeavor is connecting to {machine} for your agent, and the server asks:</p><pre>{prompt}</pre>\
<form method=\"post\"><p><input type=\"password\" name=\"answer\" autocomplete=\"off\" autofocus required></p>\
<p><button>Send</button> {cancel}</p></form><p>What you type goes to ssh only. Your agent doesn't see it.</p>"
        ),
        Kind::YesNo => format!(
            "<h1>Connect to {machine}?</h1><p>Endeavor is connecting to {machine} for your agent, and ssh asks:</p><pre>{prompt}</pre>\
<form method=\"post\"><p><button name=\"answer\" value=\"yes\">Yes</button> <button name=\"answer\" value=\"no\">No</button></p></form>"
        ),
        Kind::Confirm => format!(
            "<h1>{machine}</h1><p>Endeavor is connecting to {machine} for your agent, and ssh asks:</p><pre>{prompt}</pre>\
<form method=\"post\"><p><button name=\"answer\" value=\"ok\">OK</button> {cancel}</p></form>"
        ),
    }
}

fn form(body: &str) -> Vec<(String, String)> {
    body.split('&').filter(|pair| !pair.is_empty()).filter_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        Some((crate::core::percent_decoded(key, true)?, crate::core::percent_decoded(value, true)?))
    }).collect()
}

fn html(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Endeavor: sign in</title>\
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:36em;margin:3em auto;padding:0 1em}}button,input{{font:inherit;padding:.4em 1.2em;margin-right:.5em}}pre{{white-space:pre-wrap}}</style>\
</head><body>{body}</body></html>"
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn request(port: u16, text: &str) -> (u16, String) {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
        socket.write_all(text.as_bytes()).unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), body.to_owned())
    }

    fn parts(url: &str) -> (u16, String) {
        let (port, path) = url.strip_prefix("http://127.0.0.1:").unwrap().split_once('/').unwrap();
        (port.parse().unwrap(), format!("/{path}"))
    }

    fn get(url: &str) -> (u16, String) {
        let (port, path) = parts(url);
        request(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"))
    }

    fn post(url: &str, body: &str, origin: Option<&str>) -> (u16, String) {
        let (port, path) = parts(url);
        let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
        request(port, &format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }

    fn asked(pages: &Arc<SignIn>, ask: Ask) -> (String, std::thread::JoinHandle<Reply>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let pages = pages.clone();
        let waiting = std::thread::spawn(move || {
            pages.ask("lab", &ask, Duration::from_secs(20), &|url: &str| {
                tx.send(url.to_owned()).unwrap();
                true
            })
        });
        (rx.recv().unwrap(), waiting)
    }

    #[test]
    fn a_password_typed_on_the_page_goes_to_the_call_once() {
        let pages = Arc::new(SignIn::default());
        let (url, waiting) = asked(&pages, Ask { kind: Kind::Secret, prompt: "jc@<lab>'s password:".into() });
        let origin = url[..url.find(PATH).unwrap()].to_owned();
        let (status, shown) = get(&url);
        assert!(status == 200 && shown.contains("jc@&lt;lab&gt;'s password:") && shown.contains("type=\"password\""), "{shown}");
        assert_eq!(post(&url, "answer=x", Some("https://evil.example")).0, 403);
        assert_eq!(post(&url, "answer=x", None).0, 403, "curl -d answer=x");
        assert_eq!(post(&format!("{url}0"), "answer=x", Some(&origin)).0, 404);
        assert_eq!(post(&url, "", Some(&origin)).0, 400);
        let (status, said) = post(&url, "answer=p%40ss+word%26", Some(&origin));
        assert!(status == 200 && said.contains("Sent"), "{said}");
        assert_eq!(waiting.join().unwrap(), Reply::Answered("p@ss word&".into()));
        assert_eq!(get(&url).0, 404);
        assert_eq!(post(&url, "answer=again", Some(&origin)).0, 404);
    }

    #[test]
    fn a_host_key_question_takes_yes_or_no_and_cancel_is_no_answer() {
        let pages = Arc::new(SignIn::default());
        let (url, waiting) = asked(&pages, Ask { kind: Kind::YesNo, prompt: "Are you sure you want to continue connecting (yes/no/[fingerprint])?".into() });
        let origin = url[..url.find(PATH).unwrap()].to_owned();
        assert!(get(&url).1.contains("value=\"yes\""));
        assert_eq!(post(&url, "answer=maybe", Some(&origin)).0, 400);
        post(&url, "answer=yes", Some(&origin));
        assert_eq!(waiting.join().unwrap(), Reply::Answered("yes".into()));

        let (url, waiting) = asked(&pages, Ask { kind: Kind::Secret, prompt: "Verification code:".into() });
        post(&url, "cancel=1&answer=", Some(&url[..url.find(PATH).unwrap()]));
        assert_eq!(waiting.join().unwrap(), Reply::Refused, "Cancel is a no, not a question nobody answered");
    }

    #[test]
    fn without_a_browser_or_an_answer_in_time_there_is_no_answer() {
        let pages = SignIn::default();
        let ask = Ask { kind: Kind::Secret, prompt: "Password:".into() };
        assert_eq!(pages.ask("lab", &ask, Duration::from_secs(20), &|_| false), Reply::GaveUp);
        assert!(pages.shared.pages.lock().unwrap().is_empty(), "no page is left to answer");
        assert_eq!(pages.ask("lab", &ask, Duration::from_millis(50), &|_| true), Reply::GaveUp);
        assert!(pages.shared.pages.lock().unwrap().is_empty(), "nor once the wait ran out");
    }
}

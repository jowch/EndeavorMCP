use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::http::{self, Head};

const KEEP: Duration = Duration::from_secs(30 * 60);

const REOPEN_AFTER: Duration = Duration::from_secs(60);

const MOST_OPENS: u32 = 2;

const SETTLED: Duration = Duration::from_secs(10 * 60);

const PATH: &str = "/install/";

const MAX_BODY: u64 = 256;

#[derive(Debug, PartialEq)]
pub(super) enum Clicked {
    Install,
    Declined,
    NotYet,
    NoBrowser,
    Dropped,
}

#[derive(Default)]
pub(super) struct Pages {
    shared: Arc<Shared>,
    port: OnceLock<Option<u16>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    answered: Condvar,
}

#[derive(Default)]
struct State {
    pages: HashMap<String, Page>,
    said: HashMap<String, (bool, Instant)>,
}

impl State {
    fn prune(&mut self, settled: Duration) {
        self.pages.retain(|_, page| page.asked.elapsed() < KEEP);
        self.said.retain(|_, (_, when)| when.elapsed() < settled);
    }

    fn answer(&self, question: &str) -> Option<Clicked> {
        self.said.get(question).map(|&(yes, _)| if yes { Clicked::Install } else { Clicked::Declined })
    }
}

struct Page {
    question: String,
    answer: Option<bool>,
    asked: Instant,
    opened: Instant,
    opens: u32,
}

impl Pages {
    pub(super) fn held(&self, question: &str) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        state.prune(settled());
        state.answer(question).is_some()
    }

    pub(super) fn retire(&self, question: &str) {
        let mut state = self.shared.state.lock().unwrap();
        state.pages.retain(|_, page| page.question != question);
        state.said.remove(question);
    }

    pub(super) fn ask(&self, question: &str, hold: bool, wait: Duration, cancelled: &dyn Fn() -> bool, open: &dyn Fn(&str) -> bool) -> Clicked {
        self.ask_settled(question, hold, wait, settled(), cancelled, open)
    }

    fn ask_settled(&self, question: &str, hold: bool, wait: Duration, settled: Duration, cancelled: &dyn Fn() -> bool, open: &dyn Fn(&str) -> bool) -> Clicked {
        let Some(port) = self.port() else { return Clicked::NoBrowser };
        let now = Instant::now();
        let mut state = self.shared.state.lock().unwrap();
        state.prune(settled);
        if let Some(answer) = state.answer(question) {
            if !hold && answer == Clicked::Install {
                state.said.remove(question);
            }
            return answer;
        }
        let found = state.pages.iter().find(|(_, page)| page.question == question && page.answer.is_none()).map(|(secret, _)| secret.clone());
        let (secret, open_it) = match found {
            Some(secret) => {
                let page = state.pages.get_mut(&secret).unwrap();
                let again = page.opens < MOST_OPENS && page.opened.elapsed() >= REOPEN_AFTER;
                if again {
                    (page.opened, page.opens) = (now, page.opens + 1);
                }
                (secret, again)
            }
            None => {
                let Ok(secret) = crate::random_hex::<16>() else { return Clicked::NoBrowser };
                state.pages.insert(secret.clone(), Page { question: question.to_owned(), answer: None, asked: now, opened: now, opens: 1 });
                (secret, true)
            }
        };
        drop(state);
        if open_it && !open(&format!("http://127.0.0.1:{port}{PATH}{secret}")) {
            self.shared.state.lock().unwrap().pages.remove(&secret);
            return Clicked::NoBrowser;
        }
        let until = Instant::now() + wait;
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(answer) = state.answer(question) {
                if !hold && answer == Clicked::Install {
                    state.said.remove(question);
                }
                return answer;
            }
            if cancelled() {
                return Clicked::Dropped;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Clicked::NotYet;
            }
            state = self.shared.answered.wait_timeout(state, left.min(Duration::from_millis(200))).unwrap().0;
        }
    }

    fn port(&self) -> Option<u16> {
        *self.port.get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| eprintln!("endeavor: couldn't start the install page: {e}")).ok()?;
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

fn settled() -> Duration {
    let test = if cfg!(debug_assertions) { std::env::var("ENDEAVOR_TEST_PAGE_SETTLED_MS").ok().and_then(|ms| ms.parse().ok()) } else { None };
    test.map_or(SETTLED, Duration::from_millis)
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
    let secret = head.path().strip_prefix(PATH).filter(|_| ours(head.header("Host"), ""));
    let page = |status: &str, body: String, out: &mut wire::Shared| {
        let headers = [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "same-origin"),
            ("X-Frame-Options", "DENY"),
            ("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'"),
        ];
        http::respond_with(out, status, Some("text/html; charset=utf-8"), &headers, html(&body).as_bytes(), false)
    };
    let mut state = shared.state.lock().unwrap();
    let Some(entry) = secret.and_then(|secret| state.pages.get_mut(secret)).filter(|page| page.asked.elapsed() < KEEP) else {
        drop(state);
        return page("404 Not Found", "<h1>This question is gone</h1><p>Endeavor asks again if the agent tries the install again.</p>".to_owned(), &mut out);
    };
    match head.method() {
        "GET" => {
            let body = match entry.answer {
                Some(answer) => said(answer),
                None => format!(
                    "<h1>May Endeavor install this?</h1><p>{}</p><p>Your agent asked for it. Nothing is installed unless you click Install.</p>\
<form method=\"post\"><button name=\"answer\" value=\"install\">Install</button> <button name=\"answer\" value=\"decline\">Don't install</button></form>",
                    escape(&entry.question)
                ),
            };
            drop(state);
            page("200 OK", body, &mut out)
        }
        "POST" => {
            let same_site = head.header("Sec-Fetch-Site").is_none_or(|site| site == "same-origin");
            if !ours(head.header("Origin"), "http://") || !same_site {
                drop(state);
                return page("403 Forbidden", "<h1>Not from this page</h1>".to_owned(), &mut out);
            }
            let answer = match body.trim() {
                "answer=install" => Some(true),
                "answer=decline" => Some(false),
                _ => None,
            };
            let Some(answer) = answer else {
                drop(state);
                return page("400 Bad Request", "<h1>No answer</h1><p>Go back and click Install or Don't install.</p>".to_owned(), &mut out);
            };
            let first = entry.answer.is_none() || (entry.answer == Some(false) && answer);
            if first {
                entry.answer = Some(answer);
            }
            let answer = entry.answer.unwrap_or(answer);
            if first {
                let question = entry.question.clone();
                state.said.insert(question, (answer, Instant::now()));
            }
            drop(state);
            shared.answered.notify_all();
            page("200 OK", said(answer), &mut out)
        }
        _ => {
            drop(state);
            http::respond(&mut out, "405 Method Not Allowed", None, b"", false)
        }
    }?;
    out.flush()
}

fn said(install: bool) -> String {
    if install {
        "<h1>You said yes</h1><p>Endeavor installs it now. If your chat has stopped waiting, tell the agent you said yes, and it installs it on its next try. You can close this page.</p>".to_owned()
    } else {
        "<h1>You said no</h1><p>Nothing is installed, and Endeavor won't ask about this again for 10 minutes. You can close this page.</p>\
<form method=\"post\"><p>Changed your mind? <button name=\"answer\" value=\"install\">Install</button></p></form>".to_owned()
    }
}

fn html(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Endeavor: install?</title>\
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:36em;margin:3em auto;padding:0 1em}}button{{font:inherit;padding:.4em 1.2em;margin-right:.5em}}</style>\
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

    fn post(url: &str, answer: &str, host: Option<&str>, origin: Option<&str>, fetch_site: Option<&str>) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        let body = format!("answer={answer}");
        let host = host.map_or(format!("127.0.0.1:{port}"), str::to_owned);
        let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default() + &fetch_site.map(|f| format!("Sec-Fetch-Site: {f}\r\n")).unwrap_or_default();
        request(port.parse().unwrap(), &format!("POST /{path} HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }

    fn get(url: &str) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        request(port.parse().unwrap(), &format!("GET /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"))
    }

    fn age(pages: &Pages, question: &str) {
        let mut state = pages.shared.state.lock().unwrap();
        let page = state.pages.values_mut().find(|page| page.question == question).unwrap();
        page.opened = Instant::now() - REOPEN_AFTER - Duration::from_secs(1);
    }

    #[test]
    fn only_the_pages_install_button_is_a_yes() {
        let pages = Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        let opens = || opened.lock().unwrap().len();
        let question = "Endeavor needs to install <its helper> on lab. Install it now?";
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        let url = opened.lock().unwrap()[0].clone();
        let origin = &url[..url.find("/install/").unwrap()];
        let (status, shown) = get(&url);
        assert_eq!(status, 200);
        assert!(shown.contains("&lt;its helper&gt;") && shown.contains("value=\"install\""), "{shown}");
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        assert_eq!(opens(), 1);
        assert_eq!(post(&url, "install", Some("evil.example:80"), Some(origin), None).0, 404);
        assert_eq!(post(&url, "install", None, Some("https://evil.example"), None).0, 403);
        assert_eq!(post(&url, "install", None, Some("null"), None).0, 403);
        assert_eq!(post(&url, "install", None, None, None).0, 403, "curl -d answer=install");
        assert_eq!(post(&url, "install", None, Some(origin), Some("cross-site")).0, 403);
        assert_eq!(post(&format!("{url}0"), "install", None, Some(origin), None).0, 404);
        assert_eq!(post(&url, "maybe", None, Some(origin), None).0, 400);
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        std::thread::scope(|s| {
            let waiting = s.spawn(|| pages.ask(question, true, Duration::from_secs(20), &|| false, &open));
            let also = s.spawn(|| pages.ask(question, true, Duration::from_secs(20), &|| false, &open));
            std::thread::sleep(Duration::from_millis(100));
            let (status, body) = post(&url, "install", None, Some(origin), Some("same-origin"));
            assert!(status == 200 && body.contains("You said yes"), "{body}");
            assert_eq!((waiting.join().unwrap(), also.join().unwrap()), (Clicked::Install, Clicked::Install));
        });
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::Install);
        assert!(post(&url, "decline", None, Some(origin), None).1.contains("You said yes"));
        assert_eq!(opens(), 1);
        assert_eq!(pages.ask("Install R?", true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        assert_eq!(opens(), 2);
    }

    #[test]
    fn a_click_on_a_page_that_is_gone_gets_the_page_that_says_so() {
        let pages = Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        assert_eq!(pages.ask("Install R?", true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        let url = opened.lock().unwrap()[0].clone();
        let (port, path) = url.strip_prefix("http://127.0.0.1:").unwrap().split_once('/').unwrap();
        let port: u16 = port.parse().unwrap();
        let body = "answer=install";
        for (path, origin, status) in [(format!("{path}0"), format!("http://127.0.0.1:{port}"), "404"), (path.to_owned(), "https://evil.example".to_owned(), "403")] {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(socket, "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let early = socket.read(&mut [0; 64]);
            assert!(early.as_ref().is_err_and(|e| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)), "the page answered before reading the body: {early:?}");
            socket.set_read_timeout(None).unwrap();
            socket.write_all(body.as_bytes()).unwrap();
            let mut reply = String::new();
            socket.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with(&format!("HTTP/1.1 {status}")), "{reply}");
        }
    }

    #[test]
    fn a_retrying_agent_cant_wear_the_user_down() {
        let pages = Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        let opens = || opened.lock().unwrap().len();
        let ask = |settled: Duration| pages.ask_settled("Install it now?", true, Duration::from_millis(50), settled, &|| false, &open);
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        age(&pages, "Install it now?");
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        assert_eq!(opens(), 2);
        age(&pages, "Install it now?");
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        assert_eq!(opens(), 2, "no third tab");
        let url = opened.lock().unwrap()[1].clone();
        let origin = url[..url.find("/install/").unwrap()].to_owned();
        assert!(post(&url, "decline", None, Some(&origin), None).1.contains("won't ask about this again for 10 minutes"));
        for _ in 0..3 {
            assert_eq!(ask(SETTLED), Clicked::Declined);
        }
        assert_eq!(opens(), 2);
        assert!(post(&url, "install", None, Some(&origin), None).1.contains("You said yes"));
        assert_eq!(ask(SETTLED), Clicked::Install);
        assert!(post(&url, "decline", None, Some(&origin), None).1.contains("You said yes"), "a yes stands");
        pages.shared.state.lock().unwrap().said.clear();
        pages.shared.state.lock().unwrap().pages.clear();
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        let url = opened.lock().unwrap()[2].clone();
        assert!(post(&url, "decline", None, Some(&origin), None).0 == 200);
        assert_eq!(opens(), 3);
        assert_eq!(ask(Duration::ZERO), Clicked::NotYet);
        assert_eq!(opens(), 4);
    }

    #[test]
    fn without_a_browser_no_one_is_asked_and_a_cancel_stops_the_wait() {
        let pages = Pages::default();
        assert_eq!(pages.ask("Install it now?", true, Duration::from_secs(20), &|| false, &|_| false), Clicked::NoBrowser);
        assert!(pages.shared.state.lock().unwrap().pages.is_empty(), "no page is left to answer");
        assert_eq!(pages.ask("Install it now?", true, Duration::from_secs(20), &|| true, &|_| true), Clicked::Dropped);
    }
}

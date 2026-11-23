//! The user's own yes to an install on one of their machines, where the client can't show a dialog
//! (#381). #380 has the front ask the user in a dialog (MCP elicitation) before a machine tool
//! installs anything. A client without dialogs left the agent's `install: true` as the user's yes,
//! and an agent can say that yes in the same turn, before the user has answered (#378).
//!
//! A token in the tool's reply wouldn't help: anything the agent reads, it can pass back. So the yes
//! comes through something the agent doesn't get: a page that the front opens in the user's browser
//! itself, at a link with a one-time secret that goes from this process to the browser and never
//! into a tool result (as `browser` does with a notebook's token). The page's Install button is the
//! yes. It is served on 127.0.0.1 only, answers only its own links and only to a request addressed
//! to that loopback address, takes its answer only by POST (a link that is merely fetched answers
//! nothing), and can't be shown inside another page.
//!
//! Without a browser to open (a front started over ssh, or with no display), the user can't be
//! asked, and nothing is installed.
//!
//! The user is asked no more than they need to be, so that an agent that keeps calling can't wear
//! them down into a yes: after Don't install, the same question is refused without a page for
//! `SETTLED`; an unanswered page opens again at most once; and a yes holds for the same question for
//! `SETTLED` too, so a call that stopped waiting before its install finished isn't asked about again.
//!
//! What it doesn't stop: an agent with a shell on this same computer can find the link, in the
//! opener's command line while it runs (or the browser's, on Linux, if that call started it) or in
//! the browser's history, and answer the page itself. Such an agent could install things without
//! Endeavor anyway. The page stops an agent that says yes for the user, not one that works against them.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::http::{self, Head};

/// How long a question and its answer are kept: a yes is for a call within this time.
const KEEP: Duration = Duration::from_secs(30 * 60);

/// A question asked again after this long opens its page again, once, in case the user closed it.
const REOPEN_AFTER: Duration = Duration::from_secs(60);

/// How many times one page is opened in all.
const MOST_OPENS: u32 = 2;

/// How long an answer holds for the same question (`settled`): a no is the answer without a page, and a yes
/// lets a later call install what the user agreed to.
const SETTLED: Duration = Duration::from_secs(10 * 60);

/// Where the pages are, before each one's secret.
const PATH: &str = "/install/";

/// The longest answer a page takes.
const MAX_BODY: u64 = 256;

/// What the user said on the page.
#[derive(Debug, PartialEq)]
pub(super) enum Clicked {
    /// They clicked Install.
    Install,
    /// They clicked Don't install.
    Declined,
    /// The page is open in their browser, and they haven't answered in the time the call waited.
    NotYet,
    /// No browser could be opened: they can't be asked.
    NoBrowser,
    /// The client cancelled the call meanwhile.
    Dropped,
}

#[derive(Default)]
pub(super) struct Pages {
    shared: Arc<Shared>,
    /// The page server's port, once it is started; None if it couldn't be.
    port: OnceLock<Option<u16>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    answered: Condvar,
}

#[derive(Default)]
struct State {
    /// The questions asked, by their pages' secrets.
    pages: HashMap<String, Page>,
    /// The user's answers, by question: Install (true) or Don't install (false), and when.
    said: HashMap<String, (bool, Instant)>,
}

impl State {
    /// Forget what has run out.
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
    /// Install (true) or Don't install (false), once clicked: the first click stands.
    answer: Option<bool>,
    asked: Instant,
    opened: Instant,
    opens: u32,
}

impl Pages {
    /// Ask the user `question` on a page in their browser, or find their answer to it. Wait up to
    /// `wait` for the answer, and stop waiting once `cancelled` says so. `open` opens a link in the
    /// user's browser, and says whether it could (`browser::open`). Without `hold`, a yes is for
    /// this call only: a question too vague to stand for later calls (#389).
    /// Whether the user's answer to `question` on a page still holds: `ask` gives it at once, with
    /// no page and no dialog first.
    pub(super) fn held(&self, question: &str) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        state.prune(settled());
        state.answer(question).is_some()
    }

    /// The user answered `question` elsewhere (in the dialog that was up beside the page): its
    /// page goes, and any click on it, so that it can't answer it again, perhaps the other way.
    pub(super) fn retire(&self, question: &str) {
        let mut state = self.shared.state.lock().unwrap();
        state.pages.retain(|_, page| page.question != question);
        // A click that landed as the dialog answered: the dialog's answer is the one that stands.
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
            // Without `hold`, the next call uses up a yes.
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
        // The link goes from here to the browser only: never into a tool result.
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

    /// The page server's port, started on first use.
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

/// How long an answer holds (`SETTLED`). A debug build takes `ENDEAVOR_TEST_PAGE_SETTLED_MS`.
fn settled() -> Duration {
    let test = if cfg!(debug_assertions) { std::env::var("ENDEAVOR_TEST_PAGE_SETTLED_MS").ok().and_then(|ms| ms.parse().ok()) } else { None };
    test.map_or(SETTLED, Duration::from_millis)
}

/// Answer one request on the page server.
fn serve(stream: TcpStream, port: u16, shared: &Shared) -> std::io::Result<()> {
    // One handle for both, not a `try_clone` (wire::Shared says why).
    let stream = wire::Shared::new(stream);
    let mut out = stream.clone();
    let mut reader = BufReader::new(stream);
    let Some(head) = Head::read(&mut reader)? else { return Ok(()) };
    // A POST's body is read before any answer, the early 404 and 403 too: closing a socket with
    // its body still unread sends a reset, and the browser shows that instead of the page (#538).
    let body = match (head.method(), head.request_body()) {
        ("POST", Ok(http::Framing::Length(length))) if length <= MAX_BODY => String::from_utf8_lossy(&http::read_body(&mut reader, http::Framing::Length(length))?).into_owned(),
        _ => String::new(),
    };
    let ours = |value: Option<&str>, scheme: &str| value.is_some_and(|v| v == format!("{scheme}127.0.0.1:{port}") || v == format!("{scheme}localhost:{port}"));
    // Only a request addressed to this loopback port: a page elsewhere that points a name at
    // 127.0.0.1 sends its own name as the host.
    let secret = head.path().strip_prefix(PATH).filter(|_| ours(head.header("Host"), ""));
    let page = |status: &str, body: String, out: &mut wire::Shared| {
        let headers = [
            ("Cache-Control", "no-store"),
            // Not "no-referrer": with it, a browser sends `Origin: null` with the page's own form.
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
            // Every browser sends its page's own origin with a form, and says it came from the same
            // site; a request without them isn't this page's button.
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
            // The first answer stands, except that a no may become a yes: the user changed their mind,
            // on the page, with its own button.
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

/// What the page says once it is answered.
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

    /// One request to the page server: the status and the body.
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

    /// Pretend the page for `question` was last opened over a minute ago.
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
        // Unanswered, the call says so, and nothing is a yes.
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        let url = opened.lock().unwrap()[0].clone();
        let origin = &url[..url.find("/install/").unwrap()];
        let (status, shown) = get(&url);
        assert_eq!(status, 200);
        assert!(shown.contains("&lt;its helper&gt;") && shown.contains("value=\"install\""), "{shown}");
        // Asked again at once, the open page is used: no second tab.
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        assert_eq!(opens(), 1);
        // A fetch alone, another host's name, another page's form, no origin at all, a cross-site
        // fetch, another secret or a bad answer answers nothing.
        assert_eq!(post(&url, "install", Some("evil.example:80"), Some(origin), None).0, 404);
        assert_eq!(post(&url, "install", None, Some("https://evil.example"), None).0, 403);
        assert_eq!(post(&url, "install", None, Some("null"), None).0, 403);
        assert_eq!(post(&url, "install", None, None, None).0, 403, "curl -d answer=install");
        assert_eq!(post(&url, "install", None, Some(origin), Some("cross-site")).0, 403);
        assert_eq!(post(&format!("{url}0"), "install", None, Some(origin), None).0, 404);
        assert_eq!(post(&url, "maybe", None, Some(origin), None).0, 400);
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        // The button answers a call that waits, and one that waits on the same page with it.
        std::thread::scope(|s| {
            let waiting = s.spawn(|| pages.ask(question, true, Duration::from_secs(20), &|| false, &open));
            let also = s.spawn(|| pages.ask(question, true, Duration::from_secs(20), &|| false, &open));
            std::thread::sleep(Duration::from_millis(100));
            let (status, body) = post(&url, "install", None, Some(origin), Some("same-origin"));
            assert!(status == 200 && body.contains("You said yes"), "{body}");
            assert_eq!((waiting.join().unwrap(), also.join().unwrap()), (Clicked::Install, Clicked::Install));
        });
        // The yes holds for that question, so a call that stopped waiting before its install was
        // done isn't asked about again; and the first answer stands.
        assert_eq!(pages.ask(question, true, Duration::from_millis(50), &|| false, &open), Clicked::Install);
        assert!(post(&url, "decline", None, Some(origin), None).1.contains("You said yes"));
        assert_eq!(opens(), 1);
        // Another question is another page.
        assert_eq!(pages.ask("Install R?", true, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        assert_eq!(opens(), 2);
    }

    #[test]
    fn a_click_on_a_page_that_is_gone_gets_the_page_that_says_so() {
        // The page used to answer a POST before reading its body, so a body that arrived after the
        // head was left unread, and closing the socket sent a reset in place of the 404 (#538).
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
            // macOS resets at once; Linux doesn't, so the check is that nothing comes back, and the
            // socket isn't closed, before the body is sent.
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
        // An unanswered page opens again once, a minute or more later, and not after that.
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        age(&pages, "Install it now?");
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        assert_eq!(opens(), 2);
        age(&pages, "Install it now?");
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        assert_eq!(opens(), 2, "no third tab");
        // After Don't install, asking again at once opens no page: the answer is no.
        let url = opened.lock().unwrap()[1].clone();
        let origin = url[..url.find("/install/").unwrap()].to_owned();
        assert!(post(&url, "decline", None, Some(&origin), None).1.contains("won't ask about this again for 10 minutes"));
        for _ in 0..3 {
            assert_eq!(ask(SETTLED), Clicked::Declined);
        }
        assert_eq!(opens(), 2);
        // The user changes their mind with the page's own button: that is a yes.
        assert!(post(&url, "install", None, Some(&origin), None).1.contains("You said yes"));
        assert_eq!(ask(SETTLED), Clicked::Install);
        assert!(post(&url, "decline", None, Some(&origin), None).1.contains("You said yes"), "a yes stands");
        pages.shared.state.lock().unwrap().said.clear();
        pages.shared.state.lock().unwrap().pages.clear();
        assert_eq!(ask(SETTLED), Clicked::NotYet);
        let url = opened.lock().unwrap()[2].clone();
        assert!(post(&url, "decline", None, Some(&origin), None).0 == 200);
        assert_eq!(opens(), 3);
        // Once that has run out, the question gets a new page.
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

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

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::http::{self, Head};

/// How long a question and its answer are kept: a yes is for a call within this time.
const KEEP: Duration = Duration::from_secs(30 * 60);

/// A question asked again after this long opens its page again, in case the user closed it.
const REOPEN_AFTER: Duration = Duration::from_secs(60);

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
    /// The questions asked, by their pages' secrets.
    pages: Mutex<HashMap<String, Page>>,
    answered: Condvar,
}

struct Page {
    question: String,
    /// Install (true) or Don't install (false), once clicked.
    answer: Option<bool>,
    asked: Instant,
    opened: Instant,
}

impl Pages {
    /// Ask the user `question` on a page in their browser, or find their answer on the page already
    /// open for it. Wait up to `wait` for the answer, and stop waiting once `cancelled` says so. A
    /// yes or a no is used up: the next call with the same question asks again. `open` opens a link
    /// in the user's browser, and says whether it could (`browser::open`).
    pub(super) fn ask(&self, question: &str, wait: Duration, cancelled: &dyn Fn() -> bool, open: &dyn Fn(&str) -> bool) -> Clicked {
        let Some(port) = self.port() else { return Clicked::NoBrowser };
        let now = Instant::now();
        let mut pages = self.shared.pages.lock().unwrap();
        pages.retain(|_, page| page.asked.elapsed() < KEEP);
        let found = pages.iter().find(|(_, page)| page.question == question).map(|(secret, _)| secret.clone());
        let (secret, reopen) = match found {
            Some(secret) => {
                let page = pages.get_mut(&secret).unwrap();
                if let Some(yes) = page.answer {
                    pages.remove(&secret);
                    return if yes { Clicked::Install } else { Clicked::Declined };
                }
                let reopen = page.opened.elapsed() >= REOPEN_AFTER;
                if reopen {
                    page.opened = now;
                }
                (secret, reopen)
            }
            None => {
                let Ok(secret) = crate::random_hex::<16>() else { return Clicked::NoBrowser };
                pages.insert(secret.clone(), Page { question: question.to_owned(), answer: None, asked: now, opened: now });
                (secret, true)
            }
        };
        drop(pages);
        // The link goes from here to the browser only: never into a tool result.
        if reopen && !open(&format!("http://127.0.0.1:{port}{PATH}{secret}")) {
            self.shared.pages.lock().unwrap().remove(&secret);
            return Clicked::NoBrowser;
        }
        let until = Instant::now() + wait;
        let mut pages = self.shared.pages.lock().unwrap();
        loop {
            match pages.get(&secret).map(|page| page.answer) {
                Some(Some(yes)) => {
                    pages.remove(&secret);
                    return if yes { Clicked::Install } else { Clicked::Declined };
                }
                Some(None) => {}
                None => return Clicked::NotYet,
            }
            if cancelled() {
                return Clicked::Dropped;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Clicked::NotYet;
            }
            pages = self.shared.answered.wait_timeout(pages, left.min(Duration::from_millis(200))).unwrap().0;
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

/// Answer one request on the page server.
fn serve(stream: TcpStream, port: u16, shared: &Shared) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let Some(head) = Head::read(&mut reader)? else { return Ok(()) };
    let ours = |value: Option<&str>, scheme: &str| value.is_some_and(|v| v == format!("{scheme}127.0.0.1:{port}") || v == format!("{scheme}localhost:{port}"));
    // Only a request addressed to this loopback port: a page elsewhere that points a name at
    // 127.0.0.1 sends its own name as the host.
    let secret = head.path().strip_prefix(PATH).filter(|_| ours(head.header("Host"), ""));
    let page = |status: &str, body: String, out: &mut TcpStream| {
        let headers = [
            ("Cache-Control", "no-store"),
            // Not "no-referrer": with it, a browser sends `Origin: null` with the page's own form.
            ("Referrer-Policy", "same-origin"),
            ("X-Frame-Options", "DENY"),
            ("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'"),
        ];
        http::respond_with(out, status, Some("text/html; charset=utf-8"), &headers, html(&body).as_bytes(), false)
    };
    let mut pages = shared.pages.lock().unwrap();
    let Some(entry) = secret.and_then(|secret| pages.get_mut(secret)).filter(|page| page.asked.elapsed() < KEEP) else {
        drop(pages);
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
            drop(pages);
            page("200 OK", body, &mut out)
        }
        "POST" => {
            // A form sends its page's own origin; anything else isn't this page's button.
            if head.header("Origin").is_some_and(|origin| !ours(Some(origin), "http://")) {
                drop(pages);
                return page("403 Forbidden", "<h1>Not from this page</h1>".to_owned(), &mut out);
            }
            let body = match head.request_body() {
                Ok(http::Framing::Length(length)) if length <= MAX_BODY => {
                    drop(pages);
                    let body = http::read_body(&mut reader, http::Framing::Length(length))?;
                    pages = shared.pages.lock().unwrap();
                    String::from_utf8_lossy(&body).into_owned()
                }
                _ => String::new(),
            };
            let answer = match body.trim() {
                "answer=install" => Some(true),
                "answer=decline" => Some(false),
                _ => None,
            };
            let Some(entry) = secret.and_then(|secret| pages.get_mut(secret)) else {
                drop(pages);
                return page("404 Not Found", "<h1>This question is gone</h1><p>Endeavor asks again if the agent tries the install again.</p>".to_owned(), &mut out);
            };
            let Some(answer) = answer else {
                drop(pages);
                return page("400 Bad Request", "<h1>No answer</h1><p>Go back and click Install or Don't install.</p>".to_owned(), &mut out);
            };
            // The first answer stands.
            let answer = *entry.answer.get_or_insert(answer);
            drop(pages);
            shared.answered.notify_all();
            page("200 OK", said(answer), &mut out)
        }
        _ => {
            drop(pages);
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
        "<h1>You said no</h1><p>Nothing is installed. You can close this page.</p>".to_owned()
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

    fn post(url: &str, answer: &str, host: Option<&str>, origin: Option<&str>) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        let body = format!("answer={answer}");
        let host = host.map_or(format!("127.0.0.1:{port}"), str::to_owned);
        let origin = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
        request(port.parse().unwrap(), &format!("POST /{path} HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }

    fn get(url: &str) -> (u16, String) {
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        request(port.parse().unwrap(), &format!("GET /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"))
    }

    #[test]
    fn only_the_pages_install_button_is_a_yes() {
        let pages = Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        let question = "Endeavor needs to install <its helper> on lab. Install it now?";
        // Unanswered, the call says so, and nothing is a yes.
        assert_eq!(pages.ask(question, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        let url = opened.lock().unwrap()[0].clone();
        let (status, shown) = get(&url);
        assert_eq!(status, 200);
        assert!(shown.contains("&lt;its helper&gt;") && shown.contains("value=\"install\""), "{shown}");
        // Asked again at once, the open page is used: no second tab.
        assert_eq!(pages.ask(question, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        assert_eq!(opened.lock().unwrap().len(), 1);
        // A fetch alone, another host's name, another page's form or another secret answers nothing.
        assert_eq!(post(&url, "install", Some("evil.example:80"), None).0, 404);
        assert_eq!(post(&url, "install", None, Some("https://evil.example")).0, 403);
        assert_eq!(post(&format!("{}0", url), "install", None, None).0, 404);
        assert_eq!(post(&url, "maybe", None, None).0, 400);
        assert_eq!(pages.ask(question, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        // The button answers a call that waits.
        std::thread::scope(|s| {
            let waiting = s.spawn(|| pages.ask(question, Duration::from_secs(20), &|| false, &open));
            std::thread::sleep(Duration::from_millis(100));
            let (status, body) = post(&url, "install", None, Some(&url[..url.find("/install/").unwrap()]));
            assert!(status == 200 && body.contains("You said yes"), "{body}");
            assert_eq!(waiting.join().unwrap(), Clicked::Install);
        });
        // A yes is used up: the next call asks again, on a new page.
        assert_eq!(pages.ask(question, Duration::from_millis(50), &|| false, &open), Clicked::NotYet);
        let second = opened.lock().unwrap()[1].clone();
        assert_ne!(second, url);
        assert_eq!(get(&url).0, 404, "the old page is gone");
        // A no, clicked before the next call, is that call's answer, and the first answer stands.
        assert_eq!(post(&second, "decline", None, None).0, 200);
        assert!(post(&second, "install", None, None).1.contains("You said no"));
        assert_eq!(pages.ask(question, Duration::from_millis(50), &|| false, &open), Clicked::Declined);
    }

    #[test]
    fn without_a_browser_no_one_is_asked_and_a_cancel_stops_the_wait() {
        let pages = Pages::default();
        assert_eq!(pages.ask("Install it now?", Duration::from_secs(20), &|| false, &|_| false), Clicked::NoBrowser);
        assert!(pages.shared.pages.lock().unwrap().is_empty(), "no page is left to answer");
        assert_eq!(pages.ask("Install it now?", Duration::from_secs(20), &|| true, &|_| true), Clicked::Dropped);
    }
}

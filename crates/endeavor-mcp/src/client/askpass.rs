//! The app's side of askpass over loopback TCP (`wire::askpass`): a port on
//! 127.0.0.1 and a random token, one per listener, that a connection must send
//! first. Each `Ask` that comes with the token goes to the caller's `answer`,
//! which may wait for the user. It works on every platform, and is the only way
//! on Windows. A caller passes `Auth::Env(asker.env(askpass))` to the connect.
//!
//! 127.0.0.1 is open to every user's processes; the token, which only ssh's
//! environment holds, keeps other users' out. It doesn't keep out this user's
//! own processes, which can read that environment, as they can a Unix socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use wire::askpass::{ADDRESS_ENV, Answer, Ask, TOKEN_ENV};

/// How long a connection has to send the token and its question, in all.
const READ_LIMIT: Duration = Duration::from_secs(10);

/// The most a connection may send: the token and one question, which ssh keeps short.
const MOST_BYTES: u64 = 16 * 1024;

/// Connections served at once; more are closed at once.
const MOST_AT_ONCE: usize = 8;

/// What answers a question: the text, or None when the user cancelled.
pub type AnswerFn = dyn Fn(Ask) -> Option<String> + Send + Sync;

/// Listens until dropped.
pub struct Asker {
    port: u16,
    token: String,
    stopped: Arc<AtomicBool>,
}

impl Asker {
    pub fn start(answer: impl Fn(Ask) -> Option<String> + Send + Sync + 'static) -> Result<Asker, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("Couldn't listen for ssh's questions: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let token = crate::random_hex::<16>()?;
        let stopped: Arc<AtomicBool> = Arc::default();
        let answer: Arc<AnswerFn> = Arc::new(answer);
        let serving: Arc<AtomicUsize> = Arc::default();
        std::thread::spawn({
            let (token, stopped) = (token.clone(), stopped.clone());
            move || {
                for socket in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(socket) = socket else { continue };
                    if serving.fetch_add(1, Ordering::SeqCst) >= MOST_AT_ONCE {
                        serving.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    let (token, answer, serving) = (token.clone(), answer.clone(), serving.clone());
                    std::thread::spawn(move || {
                        serve(socket, &token, &*answer);
                        serving.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }
        });
        Ok(Asker { port, token, stopped })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The variables for ssh to ask through this listener, with `askpass` (the
    /// `endeavor` binary, or the app acting as it) as its askpass program.
    /// `SSH_ASKPASS_REQUIRE=force`: ssh then asks it even with a terminal, and
    /// Windows' ssh uses an askpass only then.
    pub fn env(&self, askpass: &Path) -> Vec<(String, String)> {
        vec![
            ("SSH_ASKPASS".into(), askpass.display().to_string()),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (ADDRESS_ENV.into(), format!("127.0.0.1:{}", self.port)),
            (TOKEN_ENV.into(), self.token.clone()),
        ]
    }
}

impl Drop for Asker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Wakes the accept so the thread sees it.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

/// One askpass: the token, then its `Ask`, within `MOST_BYTES` and `READ_LIMIT`; a connection
/// without the token is closed unanswered.
fn serve(socket: TcpStream, token: &str, answer: &AnswerFn) {
    let Ok(mut writer) = socket.try_clone() else { return };
    let mut reader = BufReader::new(Until { socket, until: Instant::now() + READ_LIMIT }.take(MOST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || !same(line.trim_end_matches(['\r', '\n']).as_bytes(), token.as_bytes()) {
        return;
    }
    line.clear();
    let Ok(ask) = reader.read_line(&mut line).map_err(|_| ()).and_then(|_| serde_json::from_str::<Ask>(&line).map_err(|_| ())) else { return };
    let mut reply = serde_json::to_string(&Answer { text: answer(ask) }).expect("serializable");
    reply.push('\n');
    let _ = writer.write_all(reply.as_bytes());
}

/// Whether `a` and `b` are the same, in a time that doesn't depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// A socket whose reads all end by `until`, however slowly the other side sends.
struct Until {
    socket: TcpStream,
    until: Instant,
}

impl Read for Until {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.socket.set_read_timeout(Some(left))?;
        self.socket.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Mutex;
    use wire::askpass::Kind;

    fn ask(port: u16, lines: &str) -> String {
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        socket.write_all(lines.as_bytes()).unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn a_question_with_the_token_is_answered_and_one_without_it_is_not() {
        let asked: Arc<Mutex<Vec<Ask>>> = Arc::default();
        let asker = Asker::start({
            let asked = asked.clone();
            move |ask| {
                asked.lock().unwrap().push(ask.clone());
                (ask.kind == Kind::Secret).then(|| "hunter2".to_owned())
            }
        })
        .unwrap();
        let env = asker.env(Path::new("askpass"));
        let token = &env.iter().find(|(k, _)| k == TOKEN_ENV).unwrap().1;
        assert_eq!(env.iter().find(|(k, _)| k == ADDRESS_ENV).unwrap().1, format!("127.0.0.1:{}", asker.port()));
        assert!(env.contains(&("SSH_ASKPASS_REQUIRE".into(), "force".into())));
        let secret = serde_json::to_string(&Ask::from_ssh("jc@lab's password: ", None)).unwrap();
        assert_eq!(ask(asker.port(), &format!("{token}\n{secret}\n")), "{\"text\":\"hunter2\"}\n");
        let yes_no = serde_json::to_string(&Ask::from_ssh("Are you sure you want to continue connecting (yes/no/[fingerprint])? ", None)).unwrap();
        assert_eq!(ask(asker.port(), &format!("{token}\n{yes_no}\n")), "{\"text\":null}\n", "a cancel is an answer without text");
        assert_eq!(ask(asker.port(), &format!("not-the-token\n{secret}\n")), "", "the wrong token gets nothing");
        assert_eq!(ask(asker.port(), &format!("{secret}\n")), "", "no token gets nothing");
        assert_eq!(asked.lock().unwrap().len(), 2, "only the two with the token reached the user");
    }

    #[test]
    fn an_endless_first_line_is_cut_off_and_closed_unanswered() {
        let asked: Arc<AtomicUsize> = Arc::default();
        let asker = Asker::start({
            let asked = asked.clone();
            move |_| {
                asked.fetch_add(1, Ordering::SeqCst);
                Some("x".into())
            }
        })
        .unwrap();
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, asker.port())).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Twice what is read, with no line break; the write may fail once the listener has closed.
        let _ = socket.write_all(&vec![b'a'; 2 * MOST_BYTES as usize]);
        let mut reply = Vec::new();
        let read = socket.read_to_end(&mut reply);
        assert!(reply.is_empty(), "{read:?} {reply:?}");
        assert!(!matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut), "it was closed, not left open: {read:?}");
        assert_eq!(asked.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_token_comparison_needs_every_byte() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd") && !same(b"abc", b"ab") && !same(b"", b"a"));
    }
}

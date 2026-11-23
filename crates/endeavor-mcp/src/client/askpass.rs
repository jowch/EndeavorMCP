use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use wire::askpass::{ADDRESS_ENV, Answer, Ask, TOKEN_ENV};

const READ_LIMIT: Duration = Duration::from_secs(10);

const MOST_BYTES: u64 = 16 * 1024;

const MOST_AT_ONCE: usize = 8;

pub type AnswerFn = dyn Fn(Ask) -> Reply + Send + Sync;

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Answered(String),
    Refused,
    GaveUp,
}

pub struct Asker {
    port: u16,
    token: String,
    stopped: Arc<AtomicBool>,
}

impl Asker {
    pub fn start(answer: impl Fn(Ask) -> Option<String> + Send + Sync + 'static) -> Result<Asker, String> {
        Asker::replying(move |ask| answer(ask).map_or(Reply::Refused, Reply::Answered))
    }

    pub fn replying(answer: impl Fn(Ask) -> Reply + Send + Sync + 'static) -> Result<Asker, String> {
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
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

const DRAIN_LIMIT: Duration = Duration::from_millis(500);

fn serve(socket: TcpStream, token: &str, answer: &AnswerFn) {
    let socket = wire::Shared::new(socket);
    answer_one(socket.clone(), token, answer);
    close(&socket);
}

fn answer_one(socket: wire::Shared, token: &str, answer: &AnswerFn) {
    let mut writer = socket.clone();
    let mut reader = BufReader::new(Until { socket, until: Instant::now() + READ_LIMIT }.take(MOST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || !same(line.trim_end_matches(['\r', '\n']).as_bytes(), token.as_bytes()) {
        return;
    }
    line.clear();
    let Ok(ask) = reader.read_line(&mut line).map_err(|_| ()).and_then(|_| serde_json::from_str::<Ask>(&line).map_err(|_| ())) else { return };
    let answer = match answer(ask) {
        Reply::Answered(text) => Answer { text: Some(text), gave_up: false },
        Reply::Refused => Answer { text: None, gave_up: false },
        Reply::GaveUp => Answer { text: None, gave_up: true },
    };
    let mut reply = serde_json::to_string(&answer).expect("serializable");
    reply.push('\n');
    let _ = writer.write_all(reply.as_bytes());
}

fn close(socket: &wire::Shared) {
    let _ = socket.shutdown(std::net::Shutdown::Write);
    let mut rest = Until { socket: socket.clone(), until: Instant::now() + DRAIN_LIMIT }.take(MOST_BYTES);
    let _ = std::io::copy(&mut rest, &mut std::io::sink());
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

struct Until {
    socket: wire::Shared,
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
    fn a_refused_client_that_is_still_sending_reads_an_empty_reply_not_a_reset() {
        let asker = Asker::start(|_| Some("x".into())).unwrap();
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, asker.port())).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        socket.write_all(b"not-the-token\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let _ = socket.write_all(b"{\"prompt\":\"late\"}\n");
        std::thread::sleep(Duration::from_millis(100));
        let mut reply = String::new();
        assert_eq!(socket.read_to_string(&mut reply).map_err(|e| e.kind()), Ok(0));
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

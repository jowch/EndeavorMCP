//! The app's side of askpass over loopback TCP (`wire::askpass`): a port on
//! 127.0.0.1 and a token that a connection must send first. Each `Ask` that
//! comes with the token goes to the caller's `answer`, which may wait for the
//! user. It works on every platform, and is the only way on Windows. A caller
//! passes `Auth::Env(asker.env(askpass))` to the connect.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use wire::askpass::{ADDRESS_ENV, Answer, Ask, TOKEN_ENV};

/// How long a connection has to send the token and its question.
const READ_LIMIT: Duration = Duration::from_secs(10);

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
        std::thread::spawn({
            let (token, stopped) = (token.clone(), stopped.clone());
            move || {
                for socket in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(socket) = socket else { continue };
                    let (token, answer) = (token.clone(), answer.clone());
                    std::thread::spawn(move || serve(socket, &token, &*answer));
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

/// One askpass: the token, then its `Ask`; a connection without the token is closed unanswered.
fn serve(socket: TcpStream, token: &str, answer: &AnswerFn) {
    let _ = socket.set_read_timeout(Some(READ_LIMIT));
    let Ok(mut writer) = socket.try_clone() else { return };
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim_end_matches(['\r', '\n']) != token {
        return;
    }
    line.clear();
    let Ok(ask) = reader.read_line(&mut line).map_err(|_| ()).and_then(|_| serde_json::from_str::<Ask>(&line).map_err(|_| ())) else { return };
    let mut reply = serde_json::to_string(&Answer { text: answer(ask) }).expect("serializable");
    reply.push('\n');
    let _ = writer.write_all(reply.as_bytes());
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
}

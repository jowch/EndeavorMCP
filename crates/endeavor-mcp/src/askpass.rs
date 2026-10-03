//! Askpass mode: ssh runs the helper as `SSH_ASKPASS` with the prompt as its
//! only argument. Pass the prompt to the app over its socket and print the
//! answer for ssh; exit 1 if the user cancelled.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use wire::askpass::{Ask, Kind};
#[cfg(unix)]
use wire::askpass::{Answer, SOCKET_ENV};

pub fn run(prompt: &str) -> ! {
    let hint = std::env::var("SSH_ASKPASS_PROMPT").ok();
    // A notice ("touch your security key") that ssh takes down itself; nothing to answer.
    if hint.as_deref() == Some("none") {
        std::process::exit(0);
    }
    let ask = Ask::from_ssh(prompt, hint.as_deref());
    match ask_app(&ask) {
        Ok(Some(text)) => {
            if ask.kind != Kind::Confirm {
                println!("{text}");
            }
            std::process::exit(0);
        }
        Ok(None) => std::process::exit(1),
        Err(e) => {
            eprintln!("endeavor askpass: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn ask_app(ask: &Ask) -> Result<Option<String>, String> {
    let path = std::env::var(SOCKET_ENV).map_err(|_| format!("{SOCKET_ENV} isn't set"))?;
    let mut socket = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    let mut line = serde_json::to_string(ask).expect("serializable");
    line.push('\n');
    socket.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply).map_err(|e| e.to_string())?;
    let answer: Answer = serde_json::from_str(&reply).map_err(|e| format!("unreadable answer: {e}"))?;
    Ok(answer.text)
}

/// Not ported: the app's socket needs loopback TCP or a named pipe on Windows.
#[cfg(windows)]
fn ask_app(_ask: &Ask) -> Result<Option<String>, String> {
    Err("ssh prompts aren't supported on Windows yet".into())
}

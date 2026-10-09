//! Askpass mode: ssh runs the helper as `SSH_ASKPASS` with the prompt as its
//! only argument. Pass the prompt to the app (over loopback TCP, or its Unix
//! socket; see `wire::askpass`) and print the answer for ssh; exit 1 if the user
//! cancelled.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use wire::askpass::{ADDRESS_ENV, Answer, Ask, Kind, TOKEN_ENV};
#[cfg(unix)]
use wire::askpass::SOCKET_ENV;

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

fn ask_app(ask: &Ask) -> Result<Option<String>, String> {
    let mut line = serde_json::to_string(ask).expect("serializable");
    line.push('\n');
    match std::env::var(ADDRESS_ENV) {
        Ok(address) => {
            // Only to this computer: the prompt and the token go nowhere else.
            let to: SocketAddr = address.parse().ok().filter(|a: &SocketAddr| a.ip().is_loopback()).ok_or_else(|| format!("{ADDRESS_ENV} isn't a loopback address: {address}"))?;
            let token = std::env::var(TOKEN_ENV).map_err(|_| format!("{TOKEN_ENV} isn't set"))?;
            let socket = TcpStream::connect(to).map_err(|e| format!("{address}: {e}"))?;
            exchange(socket, &format!("{token}\n{line}"))
        }
        Err(_) => unix_socket(&line),
    }
}

/// Send `lines` and read the app's one-line `Answer`.
fn exchange(mut socket: impl Read + Write, lines: &str) -> Result<Option<String>, String> {
    socket.write_all(lines.as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply).map_err(|e| e.to_string())?;
    if reply.is_empty() {
        return Err("the app closed the connection without answering (over TCP: is the token right?)".into());
    }
    let answer: Answer = serde_json::from_str(&reply).map_err(|e| format!("unreadable answer: {e}"))?;
    Ok(answer.text)
}

#[cfg(unix)]
fn unix_socket(line: &str) -> Result<Option<String>, String> {
    let path = std::env::var(SOCKET_ENV).map_err(|_| format!("neither {ADDRESS_ENV} nor {SOCKET_ENV} is set"))?;
    let socket = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
    exchange(socket, line)
}

/// Windows has no Unix sockets in Rust's std: the app listens on loopback TCP instead.
#[cfg(windows)]
fn unix_socket(_line: &str) -> Result<Option<String>, String> {
    Err(format!("{ADDRESS_ENV} isn't set"))
}

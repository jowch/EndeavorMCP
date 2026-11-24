use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::os::fd::AsRawFd;

const MAX_HEAD: usize = 64 * 1024;

pub struct Head {
    pub line: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn read(reader: &mut impl BufRead) -> io::Result<Option<Head>> {
        let mut size = 0;
        let mut next_line = |reader: &mut dyn BufRead| -> io::Result<Option<String>> {
            let mut line = Vec::new();
            let n = reader.take((MAX_HEAD - size) as u64 + 1).read_until(b'\n', &mut line)?;
            size += n;
            if size > MAX_HEAD {
                return Err(invalid("head too long"));
            }
            if n == 0 {
                return Ok(None);
            }
            if !line.ends_with(b"\n") {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let text = String::from_utf8(line).map_err(|_| invalid("head isn't UTF-8"))?;
            Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned()))
        };
        let Some(line) = next_line(reader)? else { return Ok(None) };
        let mut headers = Vec::new();
        loop {
            let header = next_line(reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').ok_or_else(|| invalid("malformed header"))?;
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        Ok(Some(Head { line, headers }))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn method(&self) -> &str {
        self.line.split(' ').next().unwrap_or_default()
    }

    pub fn target(&self) -> &str {
        self.line.split(' ').nth(1).unwrap_or_default()
    }

    pub fn path(&self) -> &str {
        self.target().split('?').next().unwrap_or_default()
    }

    pub fn set_target(&mut self, target: &str) {
        self.line = format!("{} {target} {}", self.method(), self.version());
    }

    pub fn status(&self) -> u16 {
        self.line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    pub fn version(&self) -> &str {
        if self.line.starts_with("HTTP/") { self.line.split(' ').next() } else { self.line.rsplit(' ').next() }.unwrap_or_default()
    }

    pub fn keeps_alive(&self) -> bool {
        let close = self.header("Connection").is_some_and(|c| c.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")));
        self.version() == "HTTP/1.1" && !close
    }

    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let mut text = format!("{}\r\n", self.line);
        for (name, value) in &self.headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        out.write_all(text.as_bytes())
    }

    pub fn request_body(&self) -> io::Result<Framing> {
        match self.framing()? {
            Framing::UntilClose => Ok(Framing::Length(0)),
            framing => Ok(framing),
        }
    }

    pub fn response_body(&self, method: &str) -> io::Result<Framing> {
        let status = self.status();
        if method == "HEAD" || (100..200).contains(&status) || status == 204 || status == 304 {
            return Ok(Framing::Length(0));
        }
        self.framing()
    }

    fn framing(&self) -> io::Result<Framing> {
        if self.header("Transfer-Encoding").is_some_and(|t| t.to_ascii_lowercase().ends_with("chunked")) {
            return Ok(Framing::Chunked(Chunk::Size { size: 0 }));
        }
        match self.header("Content-Length") {
            Some(length) => length.parse().map(Framing::Length).map_err(|_| invalid("bad Content-Length")),
            None => Ok(Framing::UntilClose),
        }
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

#[derive(Debug, PartialEq)]
pub enum Framing {
    Length(u64),
    Chunked(Chunk),
    UntilClose,
}

#[derive(Debug, PartialEq)]
pub enum Chunk {
    Size { size: u64 },
    Extension { size: u64 },
    Data(u64),
    DataEnd,
    Trailer { blank: bool },
    Done,
}

impl Framing {
    pub fn done(&self) -> bool {
        matches!(self, Framing::Length(0) | Framing::Chunked(Chunk::Done))
    }

    pub fn take(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Framing::UntilClose => Ok(bytes.len()),
            Framing::Length(left) => {
                let n = (*left).min(bytes.len() as u64);
                *left -= n;
                Ok(n as usize)
            }
            Framing::Chunked(chunk) => {
                let mut used = 0;
                while used < bytes.len() && *chunk != Chunk::Done {
                    used += chunk.take(&bytes[used..])?;
                }
                Ok(used)
            }
        }
    }
}

impl Chunk {
    fn take(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let byte = bytes[0];
        *self = match *self {
            Chunk::Data(left) => {
                let n = left.min(bytes.len() as u64);
                *self = if n == left { Chunk::DataEnd } else { Chunk::Data(left - n) };
                return Ok(n as usize);
            }
            Chunk::Size { size } => match byte {
                b'\n' if size == 0 => Chunk::Trailer { blank: true },
                b'\n' => Chunk::Data(size),
                b';' | b'\r' | b' ' | b'\t' => Chunk::Extension { size },
                _ => {
                    let digit = (byte as char).to_digit(16).ok_or_else(|| invalid("bad chunk size"))?;
                    let size = size.checked_mul(16).ok_or_else(|| invalid("chunk too large"))? + digit as u64;
                    Chunk::Size { size }
                }
            },
            Chunk::Extension { size } => match byte {
                b'\n' if size == 0 => Chunk::Trailer { blank: true },
                b'\n' => Chunk::Data(size),
                _ => Chunk::Extension { size },
            },
            Chunk::DataEnd => match byte {
                b'\n' => Chunk::Size { size: 0 },
                _ => Chunk::DataEnd,
            },
            Chunk::Trailer { blank } => match byte {
                b'\n' if blank => Chunk::Done,
                b'\n' => Chunk::Trailer { blank: true },
                b'\r' => Chunk::Trailer { blank },
                _ => Chunk::Trailer { blank: false },
            },
            Chunk::Done => Chunk::Done,
        };
        Ok(1)
    }
}

pub fn copy_body(from: &mut impl BufRead, to: &mut impl Write, framing: &mut Framing) -> io::Result<()> {
    while !framing.done() {
        let bytes = from.fill_buf()?;
        if bytes.is_empty() {
            return match framing {
                Framing::UntilClose => Ok(()),
                _ => Err(io::ErrorKind::UnexpectedEof.into()),
            };
        }
        let n = framing.take(bytes)?;
        to.write_all(&bytes[..n])?;
        from.consume(n);
    }
    Ok(())
}

pub fn read_body(from: &mut impl BufRead, framing: Framing) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    match framing {
        Framing::Length(length) => {
            from.take(length).read_to_end(&mut body)?;
            if body.len() as u64 != length {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Framing::UntilClose => {
            from.read_to_end(&mut body)?;
        }
        Framing::Chunked(_) => loop {
            let mut line = String::new();
            from.take(1024).read_line(&mut line)?;
            let digits = line.split([';', '\r', '\n']).next().unwrap_or_default().trim();
            let size = u64::from_str_radix(digits, 16).map_err(|_| invalid("bad chunk size"))?;
            if size == 0 {
                while !matches!(line.as_str(), "\r\n" | "\n") {
                    line.clear();
                    if from.take(MAX_HEAD as u64).read_line(&mut line)? == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                }
                break;
            }
            let start = body.len() as u64;
            from.take(size).read_to_end(&mut body)?;
            if body.len() as u64 - start != size {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            line.clear();
            from.take(2).read_line(&mut line)?;
        },
    }
    Ok(body)
}

pub fn respond(out: &mut impl Write, status: &str, content_type: Option<&str>, body: &[u8], keep_alive: bool) -> io::Result<()> {
    respond_with(out, status, content_type, &[], body, keep_alive)
}

pub fn respond_with(out: &mut impl Write, status: &str, content_type: Option<&str>, headers: &[(&str, &str)], body: &[u8], keep_alive: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    if let Some(content_type) = content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if !keep_alive {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(body)
}

pub fn write_chunk(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write!(out, "{:x}\r\n", bytes.len())?;
    out.write_all(bytes)?;
    out.write_all(b"\r\n")
}

pub fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    post_by(port, path, headers, body, None)
}

struct Timed<'a> {
    socket: &'a TcpStream,
    deadline: Option<Instant>,
}

impl Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.socket.set_read_timeout(Some(left))?;
        }
        self.socket.read(buf)
    }
}

pub fn post_by(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8], deadline: Option<Instant>) -> io::Result<(u16, Vec<u8>)> {
    exchange(port, path, headers, body, deadline, true).map(|(status, body)| (status, body.unwrap_or_default()))
}

pub fn post_status_by(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8], deadline: Instant) -> io::Result<u16> {
    exchange(port, path, headers, body, Some(deadline), false).map(|(status, _)| status)
}

fn exchange(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8], deadline: Option<Instant>, want_body: bool) -> io::Result<(u16, Option<Vec<u8>>)> {
    let connect = deadline.map_or(Duration::from_secs(5), |deadline| deadline.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_secs(5)));
    let upstream = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), connect)?;
    let _ = upstream.set_nodelay(true);
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = &upstream;
    out.write_all(head.as_bytes())?;
    out.write_all(body)?;
    let mut reader = BufReader::new(Timed { socket: &upstream, deadline });
    let response = loop {
        let response = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    if !want_body {
        return Ok((response.status(), None));
    }
    let body = read_body(&mut reader, response.response_body("POST")?)?;
    Ok((response.status(), Some(body)))
}

pub fn get_by(port: u16, path: &str, deadline: Instant) -> io::Result<(u16, Option<String>, Vec<u8>)> {
    let connect = deadline.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_secs(5));
    let upstream = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), connect)?;
    let _ = upstream.set_nodelay(true);
    let mut out = &upstream;
    out.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes())?;
    let mut reader = BufReader::new(Timed { socket: &upstream, deadline: Some(deadline) });
    let response = loop {
        let response = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    let body = read_body(&mut reader, response.response_body("GET")?)?;
    Ok((response.status(), response.header("content-type").map(str::to_owned), body))
}

pub fn relay_body(upstream: &mut BufReader<wire::Shared>, client: &mut wire::Shared, framing: &mut Framing) -> io::Result<()> {
    let mut watch_client = true;
    while !framing.done() {
        if upstream.buffer().is_empty() {
            match wait_readable(upstream.get_ref(), client, watch_client)? {
                Readable::Upstream => {}
                Readable::ClientClosed => return Err(io::ErrorKind::ConnectionAborted.into()),
                Readable::ClientSent => {
                    watch_client = false;
                    continue;
                }
            }
        }
        let bytes = upstream.fill_buf()?;
        if bytes.is_empty() {
            return match framing {
                Framing::UntilClose => Ok(()),
                _ => Err(io::ErrorKind::UnexpectedEof.into()),
            };
        }
        let n = framing.take(bytes)?;
        client.write_all(&bytes[..n])?;
        upstream.consume(n);
    }
    Ok(())
}

pub fn relay_response(
    request: &Head,
    client: &mut BufReader<wire::Shared>,
    upstream: &mut BufReader<wire::Shared>,
    edit: &dyn Fn(&mut Head),
) -> io::Result<bool> {
    let mut to_client = client.get_ref().clone();
    let response = loop {
        let mut response = Head::read(upstream)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        edit(&mut response);
        response.write_to(&mut to_client)?;
        if response.status() == 101 {
            tunnel(client, upstream)?;
            return Ok(false);
        }
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    let mut framing = response.response_body(request.method())?;
    relay_body(upstream, &mut to_client, &mut framing)?;
    Ok(request.keeps_alive() && response.keeps_alive() && framing != Framing::UntilClose)
}

pub fn tunnel(client: &mut BufReader<wire::Shared>, upstream: &mut BufReader<wire::Shared>) -> io::Result<()> {
    let (mut to_client, mut to_upstream) = (client.get_ref().clone(), upstream.get_ref().clone());
    to_client.write_all(upstream.buffer())?;
    upstream.consume(upstream.buffer().len());
    to_upstream.write_all(client.buffer())?;
    client.consume(client.buffer().len());
    let (mut from_client, mut from_upstream) = (client.get_ref().clone(), upstream.get_ref().clone());
    let end = |a: &TcpStream, b: &TcpStream| {
        let _ = a.shutdown(std::net::Shutdown::Both);
        let _ = b.shutdown(std::net::Shutdown::Both);
    };
    let upward = std::thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_upstream);
        end(&from_client, &to_upstream);
    });
    let _ = io::copy(&mut from_upstream, &mut to_client);
    end(&from_upstream, &to_client);
    let _ = upward.join();
    Ok(())
}

enum Readable {
    Upstream,
    ClientClosed,
    ClientSent,
}

#[cfg(windows)]
pub(crate) mod winsock {
    use std::io;
    use std::net::TcpStream;
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{MSG_PEEK, POLLERR, POLLHUP, POLLRDNORM, SOCKET, WSAPOLLFD, WSAPoll, recv};

    pub(crate) enum Peeked {
        Quiet,
        Sent,
        Closed,
    }

    pub(crate) fn wait(sockets: &[&TcpStream], timeout_ms: i32) -> io::Result<Vec<i16>> {
        let mut fds: Vec<WSAPOLLFD> = sockets.iter().map(|socket| WSAPOLLFD { fd: socket.as_raw_socket() as SOCKET, events: POLLRDNORM, revents: 0 }).collect();
        // SAFETY: `fds` holds one valid WSAPOLLFD per socket we own, and its length is passed with it.
        if unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fds.iter().map(|fd| fd.revents).collect())
    }

    pub(crate) fn peek(socket: &TcpStream, revents: i16) -> Peeked {
        if revents == 0 {
            return Peeked::Quiet;
        }
        if revents & POLLRDNORM == 0 {
            return if revents & (POLLHUP | POLLERR) != 0 { Peeked::Closed } else { Peeked::Quiet };
        }
        let mut byte = 0u8;
        // SAFETY: peeks at most one byte into `byte`; the poll said the socket is readable, so recv doesn't block.
        let n = unsafe { recv(socket.as_raw_socket() as SOCKET, &raw mut byte, 1, MSG_PEEK) };
        match n {
            1.. => Peeked::Sent,
            0 => Peeked::Closed,
            _ if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock => Peeked::Quiet,
            _ => Peeked::Closed,
        }
    }
}

#[cfg(windows)]
fn wait_readable(upstream: &TcpStream, client: &TcpStream, watch_client: bool) -> io::Result<Readable> {
    let watched: &[&TcpStream] = if watch_client { &[upstream, client] } else { &[upstream] };
    let revents = winsock::wait(watched, -1)?;
    if watch_client {
        match winsock::peek(client, revents[1]) {
            winsock::Peeked::Sent => return Ok(Readable::ClientSent),
            winsock::Peeked::Closed => return Ok(Readable::ClientClosed),
            winsock::Peeked::Quiet => {}
        }
    }
    Ok(Readable::Upstream)
}

#[cfg(unix)]
fn wait_readable(upstream: &TcpStream, client: &TcpStream, watch_client: bool) -> io::Result<Readable> {
    let mut fds = [
        libc::pollfd { fd: upstream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: client.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    let count = if watch_client { 2 } else { 1 };
    loop {
        // SAFETY: `fds` holds `count` valid pollfds for sockets we own.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), count, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        break;
    }
    if watch_client && fds[1].revents != 0 {
        let mut byte = 0u8;
        // SAFETY: peeks at most one byte into `byte`.
        let n = unsafe { libc::recv(client.as_raw_fd(), (&raw mut byte).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        if n > 0 {
            return Ok(Readable::ClientSent);
        }
        if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
            return Ok(Readable::ClientClosed);
        }
    }
    Ok(Readable::Upstream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> Head {
        Head::read(&mut text.as_bytes()).unwrap().unwrap()
    }

    #[test]
    fn reads_heads_and_their_framing() {
        let request = head("POST /call HTTP/1.1\r\nHost: 127.0.0.1:9\r\ncontent-length: 12\r\nX-Endeavor-Host: This Mac\r\n\r\n");
        assert_eq!((request.method(), request.header("Content-Length")), ("POST", Some("12")));
        assert_eq!(request.header("x-endeavor-host"), Some("This Mac"));
        assert_eq!(request.request_body().unwrap(), Framing::Length(12));
        assert!(request.keeps_alive());
        assert_eq!(head("GET /health HTTP/1.1\r\n\r\n").request_body().unwrap(), Framing::Length(0));
        assert!(!head("POST /call HTTP/1.0\r\n\r\n").keeps_alive());
        assert!(!head("GET / HTTP/1.1\r\nConnection: close\r\n\r\n").keeps_alive());

        let sse = head("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        assert_eq!((sse.status(), sse.response_body("GET").unwrap()), (200, Framing::Chunked(Chunk::Size { size: 0 })));
        assert_eq!(head("HTTP/1.1 200 OK\r\n\r\n").response_body("GET").unwrap(), Framing::UntilClose);
        assert_eq!(head("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n").response_body("HEAD").unwrap(), Framing::Length(0));
        assert_eq!(head("HTTP/1.1 100 Continue\r\n\r\n").response_body("POST").unwrap(), Framing::Length(0));
        assert!(Head::read(&mut &b""[..]).unwrap().is_none());
    }

    #[test]
    fn finds_the_end_of_chunked_bodies_split_anywhere() {
        let body = b"5;x=y\r\nhello\r\n1A\r\nabcdefghijklmnopqrstuvwxyz\r\n0\r\nTrailer: t\r\n\r\nNEXT";
        let end = body.len() - 4;
        for split in 1..body.len() {
            let mut framing = Framing::Chunked(Chunk::Size { size: 0 });
            let first = framing.take(&body[..split]).unwrap();
            let second = if framing.done() { 0 } else { framing.take(&body[split..]).unwrap() };
            assert_eq!(first + second, end, "split at {split}");
            assert!(framing.done());
        }
        let mut copied = Vec::new();
        let mut from = &body[..];
        copy_body(&mut from, &mut copied, &mut Framing::Chunked(Chunk::Size { size: 0 })).unwrap();
        assert_eq!((&copied[..], from), (&body[..end], &b"NEXT"[..]));
        assert!(Framing::Chunked(Chunk::Size { size: 0 }).take(b"zz\r\n").is_err());
    }
}

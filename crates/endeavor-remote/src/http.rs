//! Just enough HTTP/1.1 for the core to pass requests through to Julia's bridge
//! one at a time: request and response heads, and bodies relayed as they arrive
//! in whatever framing the sender used (a length, chunks, or until the
//! connection closes), so a stream of events reaches the client as it's written;
//! and whole requests and responses, for what the core answers itself and asks Julia.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;

/// Longest request or response head accepted.
const MAX_HEAD: usize = 64 * 1024;

/// A request or status line and its headers, in the order they came.
pub struct Head {
    pub line: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The next head on `reader`, or `None` if the connection closed before one began.
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

    /// Replace the value of `name` where it appears.
    pub fn replace(&mut self, name: &str, value: &str) {
        for (n, v) in &mut self.headers {
            if n.eq_ignore_ascii_case(name) {
                *v = value.to_owned();
            }
        }
    }

    pub fn method(&self) -> &str {
        self.line.split(' ').next().unwrap_or_default()
    }

    /// A request's target: its path and query.
    pub fn target(&self) -> &str {
        self.line.split(' ').nth(1).unwrap_or_default()
    }

    /// A response's status code.
    pub fn status(&self) -> u16 {
        self.line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    fn version(&self) -> &str {
        if self.line.starts_with("HTTP/") { self.line.split(' ').next() } else { self.line.rsplit(' ').next() }.unwrap_or_default()
    }

    /// Whether the connection may carry another request after this message.
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

    /// How a request's body is framed.
    pub fn request_body(&self) -> io::Result<Framing> {
        match self.framing()? {
            Framing::UntilClose => Ok(Framing::Length(0)),
            framing => Ok(framing),
        }
    }

    /// How the response to a `method` request is framed.
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

/// Where a body ends, tracked as its bytes pass through unchanged.
#[derive(Debug, PartialEq)]
pub enum Framing {
    Length(u64),
    Chunked(Chunk),
    UntilClose,
}

#[derive(Debug, PartialEq)]
pub enum Chunk {
    /// In a chunk's size line (hex digits, maybe `;extensions`).
    Size { size: u64 },
    /// Past the digits: skipping extensions to the end of the line.
    Extension { size: u64 },
    Data(u64),
    /// The line break after a chunk's data.
    DataEnd,
    /// After the last chunk: trailer lines up to a blank one; `blank` while the
    /// current line is still empty.
    Trailer { blank: bool },
    Done,
}

impl Framing {
    pub fn done(&self) -> bool {
        matches!(self, Framing::Length(0) | Framing::Chunked(Chunk::Done))
    }

    /// How many of `bytes` belong to this body, advancing past them.
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

/// Copy a body from `from` to `to` as it arrives, leaving anything after it
/// (a pipelined request) in `from`. An error if `from` closes before its end.
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

/// Read a whole body, undoing chunked framing.
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
                // Trailers, up to a blank line.
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

/// Write a whole response with a body of known length.
pub fn respond(out: &mut impl Write, status: &str, content_type: Option<&str>, body: &[u8], keep_alive: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    if let Some(content_type) = content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if !keep_alive {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(body)
}

/// POST `body` to `path` on the loopback server at `port`: the response's
/// status and whole body.
pub fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let upstream = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), std::time::Duration::from_secs(5))?;
    let _ = upstream.set_nodelay(true);
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (name, value) in headers {
        // A value can't end the header early.
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut out = &upstream;
    out.write_all(head.as_bytes())?;
    out.write_all(body)?;
    let mut reader = BufReader::new(&upstream);
    let response = loop {
        let response = Head::read(&mut reader)?.ok_or(io::ErrorKind::UnexpectedEof)?;
        if !(100..200).contains(&response.status()) {
            break response;
        }
    };
    let body = read_body(&mut reader, response.response_body("POST")?)?;
    Ok((response.status(), body))
}

/// Relay a response body from `upstream` to `client` as it arrives, like
/// `copy_body`, but give up as soon as the client hangs up: a stream of events
/// may write nothing for a long time, and the upstream end should close with it.
pub fn relay_body(upstream: &mut BufReader<TcpStream>, client: &mut TcpStream, framing: &mut Framing) -> io::Result<()> {
    let mut watch_client = true;
    while !framing.done() {
        if upstream.buffer().is_empty() {
            match wait_readable(upstream.get_ref(), client, watch_client)? {
                Readable::Upstream => {}
                Readable::ClientClosed => return Err(io::ErrorKind::ConnectionAborted.into()),
                // The client's next request; it waits in the socket for this response to end.
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

enum Readable {
    Upstream,
    ClientClosed,
    ClientSent,
}

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

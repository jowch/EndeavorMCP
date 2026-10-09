//! Many TCP streams over one frame channel, the same on both ends: the app
//! attaches the connections it accepts, the helper the ones it dials.
//!
//! Each stream has a reader thread (socket → `Data` frames) and a writer thread
//! fed by a bounded queue (frames → socket), so one slow socket never blocks the
//! channel for the others. A queue that stays full for `stall` means its socket
//! stopped reading: that stream is closed and the channel moves on, so memory
//! stays bounded and other streams wait at most `stall`.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::Frame;

/// The most a `Data` frame carries.
pub const CHUNK: usize = 64 * 1024;
/// Bytes waiting for one socket before the channel waits on it.
pub const QUEUE_BYTES: usize = 4 << 20;
/// How long the channel waits on one full queue before closing that stream.
pub const STALL: Duration = Duration::from_secs(5);

pub struct Mux {
    out: Mutex<Box<dyn Write + Send>>,
    streams: Mutex<HashMap<u32, Arc<Queue>>>,
    /// The channel ended (`close_all`), so no stream is registered from now on.
    /// Set and read under `streams`' lock.
    ended: AtomicBool,
    next_id: AtomicU32,
    stall: Duration,
}

impl Mux {
    pub fn new(out: impl Write + Send + 'static) -> Arc<Mux> {
        Mux::with_stall(out, STALL)
    }

    pub fn with_stall(out: impl Write + Send + 'static, stall: Duration) -> Arc<Mux> {
        Arc::new(Mux { out: Mutex::new(Box::new(out)), streams: Mutex::default(), ended: AtomicBool::new(false), next_id: AtomicU32::new(0), stall })
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        frame.write_to(&mut *self.out.lock().unwrap())
    }

    /// Relay a connection the app accepted to the runtime's port on the other
    /// end. Refused once the channel has ended: nobody would answer or close it.
    pub fn open(self: &Arc<Self>, socket: TcpStream) -> io::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        // Registered before `Open` goes out so the first reply finds it; the
        // reader starts after, so no `Data` precedes the `Open`.
        let queue = self.register(id)?;
        if let Err(e) = self.send(&Frame::Open { id }) {
            self.closed_here(id, false);
            return Err(e);
        }
        self.spawn(id, socket, queue)
    }

    /// Relay a connection the helper dialed for the other end's `Open { id }`.
    pub fn attach(self: &Arc<Self>, id: u32, socket: TcpStream) -> io::Result<()> {
        let queue = self.register(id)?;
        self.spawn(id, socket, queue)
    }

    /// Read frames from the other end until it closes, routing stream data and
    /// handing `Open` and `Control` frames to the callbacks. Closes every
    /// stream when the channel ends.
    pub fn run(
        self: &Arc<Self>,
        mut input: impl Read,
        mut on_open: impl FnMut(&Arc<Mux>, u32),
        mut on_control: impl FnMut(&[u8]),
    ) -> io::Result<()> {
        let result = loop {
            match Frame::read_from(&mut input) {
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
                Ok(Some(Frame::Open { id })) => on_open(self, id),
                Ok(Some(Frame::Data { id, bytes })) => self.deliver(id, bytes),
                Ok(Some(Frame::Close { id })) => self.closed_there(id),
                Ok(Some(Frame::Control(json))) => on_control(&json),
            }
        };
        self.close_all();
        result
    }

    /// Take a `Data` or `Close` frame for one of this end's streams; any other
    /// frame comes back, for a caller that relays some streams elsewhere.
    pub fn take(&self, frame: Frame) -> Option<Frame> {
        match frame {
            Frame::Data { id, bytes } if self.streams.lock().unwrap().contains_key(&id) => self.deliver(id, bytes),
            Frame::Close { id } if self.streams.lock().unwrap().contains_key(&id) => self.closed_there(id),
            other => return Some(other),
        }
        None
    }

    /// End every stream (their sockets get what was already queued, then EOF),
    /// and take no new one.
    pub fn close_all(&self) {
        let streams: Vec<_> = {
            let mut streams = self.streams.lock().unwrap();
            self.ended.store(true, Ordering::SeqCst);
            streams.drain().collect()
        };
        for (_, queue) in streams {
            queue.end(End::Finish);
        }
    }

    pub fn open_streams(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    fn register(&self, id: u32) -> io::Result<Arc<Queue>> {
        let mut streams = self.streams.lock().unwrap();
        if self.ended.load(Ordering::SeqCst) {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "the channel has ended"));
        }
        let queue = Arc::new(Queue::default());
        streams.insert(id, queue.clone());
        Ok(queue)
    }

    fn spawn(self: &Arc<Self>, id: u32, socket: TcpStream, queue: Arc<Queue>) -> io::Result<()> {
        let clones = socket.try_clone().and_then(|a| Ok((a, socket.try_clone()?)));
        let (reader, stopper) = match clones {
            Ok(clones) => clones,
            Err(e) => {
                self.closed_here(id, false);
                return Err(e);
            }
        };
        queue.state.lock().unwrap().socket = Some(stopper);
        let mux = self.clone();
        std::thread::spawn(move || mux.write_socket(id, socket, &queue));
        let mux = self.clone();
        std::thread::spawn(move || mux.read_socket(id, reader));
        Ok(())
    }

    fn write_socket(&self, id: u32, mut socket: TcpStream, queue: &Queue) {
        while let Some(chunk) = queue.pop() {
            if socket.write_all(&chunk).is_err() {
                self.closed_here(id, false);
                break;
            }
        }
        // Also wakes this stream's reader, which then finds the stream gone.
        let _ = socket.shutdown(Shutdown::Both);
    }

    fn read_socket(&self, id: u32, mut socket: TcpStream) {
        let mut buf = vec![0; CHUNK];
        loop {
            let n = match socket.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if !self.streams.lock().unwrap().contains_key(&id) {
                return;
            }
            if self.send(&Frame::Data { id, bytes: buf[..n].to_vec() }).is_err() {
                break;
            }
        }
        self.closed_here(id, true);
    }

    fn deliver(&self, id: u32, bytes: Vec<u8>) {
        let Some(queue) = self.streams.lock().unwrap().get(&id).cloned() else { return };
        if !queue.push(bytes, self.stall) {
            self.closed_here(id, false);
        }
    }

    /// The other end closed `id`.
    fn closed_there(&self, id: u32) {
        let queue = self.streams.lock().unwrap().remove(&id);
        if let Some(queue) = queue {
            queue.end(End::Finish);
        }
    }

    /// `id` ended on this end: tell the other, and flush or drop what's queued for the socket.
    fn closed_here(&self, id: u32, flush: bool) {
        let queue = self.streams.lock().unwrap().remove(&id);
        if let Some(queue) = queue {
            let _ = self.send(&Frame::Close { id });
            queue.end(if flush { End::Finish } else { End::Abort });
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum End {
    /// Write what's queued, then close.
    Finish,
    /// Close now.
    Abort,
}

#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Default)]
struct QueueState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    end: Option<End>,
    /// For an abort to unblock a writer stuck on a socket that stopped reading.
    socket: Option<TcpStream>,
}

impl Queue {
    /// Queue `chunk`, waiting up to `stall` for room; false if there was none.
    fn push(&self, chunk: Vec<u8>, stall: Duration) -> bool {
        let deadline = Instant::now() + stall;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.end.is_some() {
                return true;
            }
            if state.bytes == 0 || state.bytes + chunk.len() <= QUEUE_BYTES {
                state.bytes += chunk.len();
                state.chunks.push_back(chunk);
                self.changed.notify_all();
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    /// The next chunk for the socket; None once the stream is over.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.end == Some(End::Abort) {
                return None;
            }
            if let Some(chunk) = state.chunks.pop_front() {
                state.bytes -= chunk.len();
                self.changed.notify_all();
                return Some(chunk);
            }
            if state.end == Some(End::Finish) {
                return None;
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    fn end(&self, end: End) {
        let mut state = self.state.lock().unwrap();
        let end = *state.end.get_or_insert(end);
        if end == End::Abort
            && let Some(socket) = &state.socket
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.changed.notify_all();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;

    /// An app end and a helper end joined like the helper's stdin/stdout; the
    /// helper dials `port` for each `Open`.
    fn channel(port: u16, stall: Duration) -> (Arc<Mux>, Arc<Mux>) {
        let (a, b) = UnixStream::pair().unwrap();
        let app = Mux::with_stall(a.try_clone().unwrap(), stall);
        let helper = Mux::with_stall(b.try_clone().unwrap(), stall);
        let h = helper.clone();
        std::thread::spawn(move || {
            h.run(
                b,
                |mux, id| {
                    match TcpStream::connect(("127.0.0.1", port)) {
                        Ok(socket) => mux.attach(id, socket).unwrap(),
                        Err(_) => mux.send(&Frame::Close { id }).unwrap(),
                    }
                },
                |_| {},
            )
        });
        let a2 = app.clone();
        std::thread::spawn(move || a2.run(a, |_, _| {}, |_| {}));
        (app, helper)
    }

    /// A client socket whose other end the app relays to the helper's port.
    fn connect(app: &Arc<Mux>) -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        app.open(listener.accept().unwrap().0).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        client
    }

    fn server(serve: fn(TcpStream)) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let socket = socket.unwrap();
                std::thread::spawn(move || serve(socket));
            }
        });
        port
    }

    fn echo(mut socket: TcpStream) {
        let mut reader = socket.try_clone().unwrap();
        let _ = io::copy(&mut reader, &mut socket);
    }

    /// 40 lines, 25 ms apart, then closes.
    fn slow(mut socket: TcpStream) {
        for i in 0..40 {
            let _ = writeln!(socket, "tick {i}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn flood(mut socket: TcpStream) {
        let chunk = vec![7u8; CHUNK];
        while socket.write_all(&chunk).is_ok() {}
    }

    const SLOW: u8 = 1;
    const FLOOD: u8 = 2;

    /// What the runtime's one port does in these tests: `slow` or `flood` when
    /// the first byte asks for it, else `echo`.
    fn mixed(mut socket: TcpStream) {
        let mut first = [0];
        if socket.read_exact(&mut first).is_err() {
            return;
        }
        match first[0] {
            SLOW => slow(socket),
            FLOOD => flood(socket),
            byte => {
                let _ = socket.write_all(&[byte]);
                echo(socket);
            }
        }
    }

    fn connect_to(app: &Arc<Mux>, what: u8) -> TcpStream {
        let mut client = connect(app);
        client.write_all(&[what]).unwrap();
        client
    }

    fn round_trip(client: &mut TcpStream, message: &[u8]) -> Duration {
        let start = Instant::now();
        client.write_all(message).unwrap();
        let mut back = vec![0; message.len()];
        client.read_exact(&mut back).unwrap();
        assert_eq!(back, message);
        start.elapsed()
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn streams_carry_bytes_both_ways_at_once() {
        let (app, helper) = channel(server(echo), STALL);
        let clients: Vec<_> = (0..8u8)
            .map(|n| {
                let mut client = connect(&app);
                std::thread::spawn(move || {
                    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i as u8).wrapping_mul(n + 1)).collect();
                    let mut writer = client.try_clone().unwrap();
                    let sent = data.clone();
                    let w = std::thread::spawn(move || writer.write_all(&sent).unwrap());
                    let mut back = vec![0; data.len()];
                    client.read_exact(&mut back).unwrap();
                    w.join().unwrap();
                    assert!(back == data, "stream {n} came back changed");
                })
            })
            .collect();
        for c in clients {
            c.join().unwrap();
        }
        eventually("all streams closed", || app.open_streams() == 0 && helper.open_streams() == 0);
    }

    #[test]
    fn a_slow_stream_holds_up_no_other_and_ends_after_its_last_byte() {
        let (app, _helper) = channel(server(mixed), STALL);
        let slow = connect_to(&app, SLOW);
        let mut quick = connect(&app);
        for _ in 0..20 {
            assert!(round_trip(&mut quick, b"ping") < Duration::from_millis(500));
        }
        let lines: Vec<String> = io::BufRead::lines(io::BufReader::new(slow)).map(Result::unwrap).collect();
        assert_eq!(lines.len(), 40, "every line, then EOF");
        assert_eq!(lines[39], "tick 39");
    }

    #[test]
    fn a_socket_that_stops_reading_is_closed_and_the_rest_carry_on() {
        let stall = Duration::from_millis(300);
        let (app, _helper) = channel(server(mixed), stall);
        let mut stuck = connect_to(&app, FLOOD);
        let mut quick = connect(&app);
        eventually("the stuck stream closed", || app.open_streams() == 1);
        for _ in 0..20 {
            assert!(round_trip(&mut quick, b"still here") < stall + Duration::from_millis(500));
        }
        // What was delivered before the close, then an end, not a hang.
        let mut sink = Vec::new();
        let result = stuck.read_to_end(&mut sink);
        assert!(result.is_ok() || result.unwrap_err().kind() == ErrorKind::ConnectionReset);
        assert!(sink.iter().all(|&b| b == 7));
    }

    #[test]
    fn a_target_that_refuses_ends_the_stream() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (app, _helper) = channel(closed, STALL);
        let mut client = connect(&app);
        let mut buf = [0; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
        eventually("the refused stream closed", || app.open_streams() == 0);
    }

    #[test]
    fn a_channel_that_ends_closes_its_streams() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let app = Mux::new(a.try_clone().unwrap());
        let a2 = app.clone();
        let run = std::thread::spawn(move || a2.run(a, |_, _| {}, |_| {}));
        let mut client = connect(&app);
        // Linux resets a Unix socket whose peer closes with bytes unread, so the
        // other end reads what it was sent before it goes, as a helper does.
        assert_eq!(Frame::read_from(&mut b).unwrap(), Some(Frame::Open { id: 1 }));
        drop(b);
        run.join().unwrap().unwrap();
        let mut buf = [0; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
    }

    /// A connection relayed just after the channel ended, as the app's listener
    /// can still do before it hears of the end, is refused, not left open with
    /// nobody to answer or close it.
    #[test]
    fn a_channel_that_ended_takes_no_new_stream() {
        let (a, b) = UnixStream::pair().unwrap();
        // The helper's input stays writable, as it does after a detach.
        let app = Mux::new(Vec::new());
        drop(b);
        app.run(a, |_, _| {}, |_| {}).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert!(app.open(listener.accept().unwrap().0).is_err());
        assert_eq!(app.open_streams(), 0);
        let mut buf = [0; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
    }
}

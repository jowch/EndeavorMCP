//! What the app and `endeavor-remote` say to each other over the helper's
//! stdin/stdout: TCP streams to the runtime's two ports, multiplexed byte for
//! byte, plus JSON control messages.
//!
//! # Frame layout
//!
//! Every frame is a 4-byte big-endian length `n`, then `n` bytes: a kind byte
//! and its body.
//!
//! | kind | frame     | body                                          |
//! |------|-----------|-----------------------------------------------|
//! | 0    | `Open`    | stream id (u32 BE), target (0 Pluto, 1 Bridge) |
//! | 1    | `Data`    | stream id (u32 BE), the bytes                 |
//! | 2    | `Close`   | stream id (u32 BE)                            |
//! | 3    | `Control` | one JSON message ([`ToApp`] or [`ToHelper`])  |
//!
//! Only the app opens streams, so it picks the ids. `Close` ends a stream in
//! both directions; either side may send it, and frames for an id that's
//! already closed are dropped.

pub mod askpass;
pub mod files;
pub mod notebooks;
pub mod relay;
pub mod slurm;

use std::io::{self, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

/// A frame larger than this is corrupt input, not a message.
pub const MAX_FRAME: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Pluto,
    Bridge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Open { id: u32, target: Target },
    Data { id: u32, bytes: Vec<u8> },
    Close { id: u32 },
    Control(Vec<u8>),
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0; 4];
        match self {
            Frame::Open { id, target } => {
                out.push(0);
                out.extend(id.to_be_bytes());
                out.push(match target {
                    Target::Pluto => 0,
                    Target::Bridge => 1,
                });
            }
            Frame::Data { id, bytes } => {
                out.push(1);
                out.extend(id.to_be_bytes());
                out.extend(bytes);
            }
            Frame::Close { id } => {
                out.push(2);
                out.extend(id.to_be_bytes());
            }
            Frame::Control(json) => {
                out.push(3);
                out.extend(json);
            }
        }
        let len = (out.len() - 4) as u32;
        out[..4].copy_from_slice(&len.to_be_bytes());
        out
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())?;
        w.flush()
    }

    /// The next frame, or None at a clean end of input (between frames).
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut len = [0; 4];
        let mut got = 0;
        while got < 4 {
            match r.read(&mut len[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
                Ok(n) => got += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(invalid(format!("frame length {len}")));
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body)?;
        let id = || -> io::Result<u32> {
            let bytes = body.get(1..5).ok_or_else(|| invalid("frame too short for its stream id".into()))?;
            Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
        };
        let frame = match body[0] {
            0 => Frame::Open {
                id: id()?,
                target: match body.get(5) {
                    Some(0) => Target::Pluto,
                    Some(1) => Target::Bridge,
                    other => return Err(invalid(format!("unknown target {other:?}"))),
                },
            },
            1 => Frame::Data { id: id()?, bytes: body[5..].to_vec() },
            2 => Frame::Close { id: id()? },
            3 => Frame::Control(body[1..].to_vec()),
            kind => return Err(invalid(format!("unknown frame kind {kind}"))),
        };
        Ok(Some(frame))
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

/// Helper → app. The helper says `Hello` as soon as it runs; the runtime starts
/// (or is attached to) only when the app sends `StartRuntime`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToApp {
    /// The helper is up. File requests work from now on.
    Hello {
        version: String,
        /// The machine the helper runs on.
        node: String,
        home: String,
        /// Slurm's commands are here: probably a cluster's login node.
        #[serde(default)]
        slurm: bool,
        /// It takes `files::Request::Place` and `Write`; an older helper
        /// doesn't say so and would ignore them.
        #[serde(default)]
        uploads: bool,
    },
    /// A line of the runtime's log while it starts, or of Julia's download.
    Progress { line: String },
    /// The julia the helper starts the runtime with (sent only when it starts one).
    FoundJulia { path: String, version: String },
    /// A cluster job for the runtime was submitted (`summary`: "8 CPUs · 32 GB · 8 h").
    Submitted { job: String, summary: String },
    /// The job waits in the queue: its state (PENDING, CONFIGURING) and Slurm's
    /// reason; then once, state RUNNING and (as `reason`) the node it got.
    Queued { job: String, state: String, reason: String },
    /// The runtime is up and streams can open.
    Ready {
        /// How the runtime was started: "process" or "slurm".
        launcher: String,
        /// The machine the runtime runs on.
        node: String,
        pid: u32,
        /// The bridge's bearer token.
        token: String,
        pluto_secret: String,
        /// The runtime was already running; this connect didn't start it.
        reattached: bool,
        /// The cluster job it runs in.
        #[serde(default)]
        job: Option<slurm::Job>,
    },
    /// The runtime couldn't start (no Julia, running on another node, …); the
    /// helper stays connected, so `StartRuntime` can try again.
    StartFailed { message: String },
    /// The runtime exited; the helper stays connected.
    Died { status: String, log_tail: Vec<String> },
    /// The runtime stopped as the app asked; the helper stays connected.
    Stopped,
    /// Another client took over this runtime; the helper exits.
    Replaced,
    /// The answer to `ToHelper::Files` with the same id.
    Files { id: u32, reply: files::Reply },
    /// The helper couldn't go on; it exits.
    Error { message: String },
}

/// App → helper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToHelper {
    /// Attach to the runtime, starting it if it isn't running. On a cluster,
    /// `job` says what to submit.
    StartRuntime {
        #[serde(default)]
        job: Option<slurm::JobRequest>,
    },
    /// Stop the runtime and stay connected: the attached one, else the one
    /// recorded in the state folder, or on a cluster the job waiting for it.
    Stop,
    /// Exit and leave the runtime running.
    Detach,
    Files { id: u32, request: files::Request },
}

impl ToApp {
    pub fn frame(&self) -> Frame {
        Frame::Control(serde_json::to_vec(self).expect("serializable"))
    }
}

impl ToHelper {
    pub fn frame(&self) -> Frame {
        Frame::Control(serde_json::to_vec(self).expect("serializable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out at most `step` bytes per read, like a pipe under load.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn samples() -> Vec<Frame> {
        vec![
            Frame::Open { id: 1, target: Target::Pluto },
            Frame::Open { id: u32::MAX, target: Target::Bridge },
            Frame::Data { id: 7, bytes: b"GET / HTTP/1.1\r\n\r\n".to_vec() },
            Frame::Data { id: 7, bytes: Vec::new() },
            Frame::Data { id: 8, bytes: (0..=255).cycle().take(70_000).collect() },
            Frame::Close { id: 7 },
            ToHelper::Stop.frame(),
            ToApp::Died { status: "signal: 9".into(), log_tail: vec!["a".into(), "b".into()] }.frame(),
        ]
    }

    #[test]
    fn frames_round_trip_through_any_read_sizes() {
        let bytes: Vec<u8> = samples().iter().flat_map(Frame::encode).collect();
        for step in [1, 3, 4, 5, 1000, usize::MAX] {
            let mut r = Trickle { data: &bytes, step };
            let mut got = Vec::new();
            while let Some(frame) = Frame::read_from(&mut r).unwrap() {
                got.push(frame);
            }
            assert_eq!(got, samples(), "reads of {step} bytes");
        }
    }

    #[test]
    fn a_truncated_or_corrupt_frame_is_an_error_not_an_end() {
        let bytes = Frame::Data { id: 1, bytes: b"hello".to_vec() }.encode();
        for cut in 1..bytes.len() {
            let err = Frame::read_from(&mut &bytes[..cut]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "cut at {cut}");
        }
        assert!(Frame::read_from(&mut &[][..]).unwrap().is_none());
        let bad = |b: &[u8]| Frame::read_from(&mut &b[..]).unwrap_err().kind();
        assert_eq!(bad(&[0, 0, 0, 1, 9]), ErrorKind::InvalidData, "unknown kind");
        assert_eq!(bad(&[0, 0, 0, 2, 1, 0]), ErrorKind::InvalidData, "no stream id");
        assert_eq!(bad(&[0, 0, 0, 6, 0, 0, 0, 0, 1, 5]), ErrorKind::InvalidData, "unknown target");
        assert_eq!(bad(&[0xff, 0, 0, 0]), ErrorKind::InvalidData, "too long");
    }

    #[test]
    fn control_messages_are_tagged_json() {
        let ready = ToApp::Ready {
            launcher: "process".into(),
            node: "labbox3".into(),
            pid: 81234,
            token: "t".into(),
            pluto_secret: "s".into(),
            reattached: true,
            job: None,
        };
        let Frame::Control(json) = ready.frame() else { panic!() };
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["type"], "Ready");
        assert_eq!(value["launcher"], "process");
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), ready);
        let files = ToHelper::Files { id: 3, request: files::Request::List { path: "~".into() } };
        let Frame::Control(json) = files.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<ToHelper>(&json).unwrap(), files);
        assert_eq!(serde_json::from_str::<ToHelper>(r#"{"type":"Detach"}"#).unwrap(), ToHelper::Detach);
        assert_eq!(serde_json::from_str::<ToApp>(r#"{"type":"Replaced"}"#).unwrap(), ToApp::Replaced);
        assert_eq!(serde_json::from_str::<ToHelper>(r#"{"type":"StartRuntime"}"#).unwrap(), ToHelper::StartRuntime { job: None });
        let old_hello = r#"{"type":"Hello","version":"0.1.0","node":"labbox3","home":"/home/ada"}"#;
        assert!(matches!(serde_json::from_str::<ToApp>(old_hello).unwrap(), ToApp::Hello { slurm: false, uploads: false, .. }), "a helper from before uploads");
    }
}

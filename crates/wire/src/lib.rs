//! What the app and its helper (`endeavor`) say to each other over the helper's
//! stdin/stdout: TCP streams to the runtime's port, multiplexed byte for byte,
//! plus JSON control messages.
//!
//! # Frame layout
//!
//! Every frame is a 4-byte big-endian length `n`, then `n` bytes: a kind byte
//! and its body.
//!
//! | kind | frame     | body                                          |
//! |------|-----------|-----------------------------------------------|
//! | 0    | `Open`    | stream id (u32 BE)                            |
//! | 1    | `Data`    | stream id (u32 BE), the bytes                 |
//! | 2    | `Close`   | stream id (u32 BE)                            |
//! | 3    | `Control` | one JSON message ([`ToApp`] or [`ToHelper`])  |
//!
//! Only the app opens streams, so it picks the ids. `Close` ends a stream in
//! both directions; either side may send it, and frames for an id that's
//! already closed are dropped.

pub mod askpass;
pub mod backend;
pub mod files;
pub mod notebooks;
pub mod relay;
pub mod slurm;
pub mod tree;

use std::io::{self, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

/// A frame larger than this is corrupt input, not a message.
pub const MAX_FRAME: usize = 16 << 20;

/// The number of this protocol: the frames, [`ToApp`] and [`ToHelper`]. The
/// helper says it in [`ToApp::Hello`], and a client refuses a helper whose number
/// differs from its own. Raise it when a client and a helper of the previous
/// number can no longer work together: a message or field one of them needs and
/// the other would not understand, or one whose meaning changed. Adding an
/// optional field or a message that a peer may ignore does not raise it. A
/// helper that says no number (`Hello` without `protocol`) is 0.
pub const PROTOCOL: u32 = 1;

/// `StartRuntime::engine` for Pluto notebooks, the only engine so far.
pub const ENGINE_PLUTO: &str = "pluto";

/// [`Item::kind`] of a language runtime, such as Julia.
pub const KIND_RUNTIME: &str = "runtime";
/// [`Item::kind`] of Endeavor's own files on a machine.
pub const KIND_HELPER: &str = "helper";

/// Something that has to be installed on a machine before a start can go on.
/// `kind` is a plain string, so that a kind a peer doesn't know still reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub kind: String,
    /// What a person calls it, with its version: "Julia 1.12.6".
    pub name: String,
    /// About how much would be downloaded or copied, in MB, when known.
    pub size_mb: Option<u64>,
    /// The folder it would go in, when known.
    pub place: Option<String>,
}

impl Item {
    /// "about 289 MB, into /home/ada/.cache/endeavor/julia-1.12.6", or nothing of what isn't known.
    fn details(&self) -> String {
        self.size_mb.map(|mb| format!("about {mb} MB")).into_iter().chain(self.place.as_ref().map(|place| format!("into {place}"))).collect::<Vec<_>>().join(", ")
    }
}

impl std::fmt::Display for Item {
    /// "Julia 1.12.6 (about 289 MB, into /home/ada/.cache/endeavor/julia-1.12.6)".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let details = self.details();
        if details.is_empty() { write!(f, "{}", self.name) } else { write!(f, "{} ({details})", self.name) }
    }
}

/// The items as a sentence part: "Julia 1.12.6 (about 289 MB, into /x) and Endeavor's helper (about 20 MB)".
pub fn items_text(items: &[Item]) -> String {
    match items {
        [] => String::new(),
        [only] => only.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.iter().map(Item::to_string).collect::<Vec<_>>().join(", ")),
    }
}

/// What is missing on `machine`, a sentence for each item. A runtime that wasn't found: "Julia 1.12.6
/// wasn't found on lab. Endeavor can download its own copy (about 289 MB, into /x)." Any other:
/// "Endeavor needs to install R 4.5.1 (about 120 MB) on lab."
pub fn needs_text(items: &[Item], machine: &str) -> String {
    let sentence = |item: &Item| match (item.kind == KIND_RUNTIME, item.details()) {
        (true, details) if details.is_empty() => format!("{} wasn't found on {machine}. Endeavor can download its own copy.", item.name),
        (true, details) => format!("{} wasn't found on {machine}. Endeavor can download its own copy ({details}).", item.name),
        (false, _) => format!("Endeavor needs to install {item} on {machine}."),
    };
    items.iter().map(sentence).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Open { id: u32 },
    Data { id: u32, bytes: Vec<u8> },
    Close { id: u32 },
    Control(Vec<u8>),
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0; 4];
        match self {
            Frame::Open { id } => {
                out.push(0);
                out.extend(id.to_be_bytes());
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
            0 => Frame::Open { id: id()? },
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
        /// The helper's [`PROTOCOL`]; absent from a helper that predates it, so 0.
        #[serde(default)]
        protocol: u32,
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
        /// How this helper runs the runtime, for the whole connection: "process" or "slurm" (what
        /// `--launcher auto` became). Empty from a helper that predates it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        launcher: String,
    },
    /// A line of the runtime's log while it starts, or of an install. A
    /// start's progress (this, `Found`, `Submitted`, `Queued`) names no
    /// id: only one start is under way at a time, and the answer that ends it names it.
    Progress { line: String },
    /// What the helper starts the runtime with, such as Julia: its `name`
    /// ("Julia"), `version` and `path` (sent only when it starts one).
    Found { name: String, version: String, path: String },
    /// A cluster job for the runtime was submitted (`summary`: "8 CPUs · 32 GB · 8 h").
    Submitted { job: String, summary: String },
    /// The job waits in the queue: its state (PENDING, CONFIGURING) and Slurm's
    /// reason; then once, state RUNNING and (as `reason`) the node it got.
    Queued { job: String, state: String, reason: String },
    /// The runtime is up and streams can open: the answer to the `StartRuntime`
    /// with this id. (The relay on a job's node, which has no request, says it with id 0.)
    Ready {
        id: u32,
        /// How the runtime was started: "process" or "slurm".
        launcher: String,
        /// The machine the runtime runs on.
        node: String,
        pid: u32,
        /// The runtime's token, for every path on its port.
        token: String,
        /// The runtime was already running; this connect didn't start it.
        reattached: bool,
        /// The cluster job it runs in.
        #[serde(default)]
        job: Option<slurm::Job>,
        /// The runtime's own port on `node`. A helper that doesn't say leaves it unknown.
        #[serde(default)]
        port: Option<u16>,
        /// The build that started the runtime, as its record says; none when it doesn't, or the helper doesn't say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build: Option<String>,
        /// The number for what the runtime's core offers its callers (`endeavor_mcp::CORE_INTERFACE`), as its
        /// record says; none when it doesn't, or the helper doesn't say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interface: Option<u32>,
    },
    /// The runtime couldn't start (running on another node, …): the answer to the
    /// `StartRuntime` with this id. The helper stays connected, so
    /// `StartRuntime` can try again.
    StartFailed { id: u32, message: String },
    /// The `StartRuntime` with this id needs `items` installed and didn't
    /// allow installing: nothing was started or installed. The helper stays
    /// connected, so `StartRuntime` can try again with `install` true.
    NeedsInstall { id: u32, items: Vec<Item> },
    /// The runtime exited while the `StartRuntime` with this id waited for it:
    /// that start's answer. A runtime that exits later is `Died`.
    StartDied { id: u32, status: String, log_tail: Vec<String> },
    /// A `Stop` ended the `StartRuntime` with this id before it was ready: that
    /// start's answer, sent before the `Stopped` that answers the `Stop`.
    StartCancelled { id: u32 },
    /// The `StartRuntime` with this id had `attach_only`, and no runtime runs or starts: nothing was started.
    NotRunning { id: u32 },
    /// The runtime exited, with no request waiting for it; the helper stays connected.
    Died { status: String, log_tail: Vec<String> },
    /// The runtime stopped as the `Stop` with this id asked; the helper stays connected.
    Stopped { id: u32 },
    /// The runtime did not stop, and why; it is as it was and the helper stays
    /// connected. The helper answers each `Stop` with one `Stopped` or one
    /// `NotStopped`, naming its id.
    NotStopped { id: u32, message: String },
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
    ///
    /// `id` is the client's own; the answer names it (see `ToApp`), and the helper
    /// answers each request once. Ids are the client's to keep apart: one in use
    /// by a request still waiting must not be used again.
    ///
    /// `engine` names the notebook system to start ([`ENGINE_PLUTO`]). `install`
    /// says the helper may install whatever this start needs (such as a
    /// language runtime); without it, a start that needs something is answered
    /// `NeedsInstall` and installs nothing. `attach_only` says never to start
    /// one: the helper attaches to the runtime that runs, waits for a start
    /// under way and attaches to what it records, and answers `NotRunning` when
    /// there is none or the start ended without one. Absent, as from a client
    /// that predates it, it is false.
    StartRuntime {
        id: u32,
        #[serde(default)]
        job: Option<slurm::JobRequest>,
        engine: String,
        install: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        attach_only: bool,
    },
    /// Stop the runtime and stay connected: the attached one, else the one
    /// recorded in the state folder, or on a cluster the job waiting for it.
    /// A start under way ends with its own answer, then this is answered;
    /// that includes this connection's wait for a start another connection
    /// began. Without `force`, that other start is left to finish and the
    /// answer is `NotStopped`; with it, it is cancelled as
    /// `endeavor stop --force` cancels it. Absent, as from a client that
    /// predates it, it is false; a helper that predates it ignores it.
    Stop {
        id: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        force: bool,
    },
    /// Exit and leave the runtime running.
    Detach,
    Files { id: u32, request: files::Request },
}

impl ToApp {
    /// The id of the request this answers, if it answers one.
    pub fn answers(&self) -> Option<u32> {
        match self {
            ToApp::Ready { id, .. } | ToApp::StartFailed { id, .. } | ToApp::NeedsInstall { id, .. } | ToApp::StartDied { id, .. } | ToApp::StartCancelled { id } | ToApp::NotRunning { id } | ToApp::Stopped { id } | ToApp::NotStopped { id, .. } => Some(*id),
            _ => None,
        }
    }

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
            Frame::Open { id: 1 },
            Frame::Open { id: u32::MAX },
            Frame::Data { id: 7, bytes: b"GET / HTTP/1.1\r\n\r\n".to_vec() },
            Frame::Data { id: 7, bytes: Vec::new() },
            Frame::Data { id: 8, bytes: (0..=255).cycle().take(70_000).collect() },
            Frame::Close { id: 7 },
            ToHelper::Stop { id: 1, force: false }.frame(),
            ToHelper::Stop { id: 2, force: true }.frame(),
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
        assert_eq!(bad(&[0xff, 0, 0, 0]), ErrorKind::InvalidData, "too long");
    }

    #[test]
    fn control_messages_are_tagged_json() {
        let ready = ToApp::Ready {
            id: 4,
            launcher: "process".into(),
            node: "labbox3".into(),
            pid: 81234,
            token: "t".into(),
            reattached: true,
            job: None,
            port: Some(41234),
            build: Some("abc123".into()),
            interface: Some(1),
        };
        let Frame::Control(json) = ready.frame() else { panic!() };
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["type"], "Ready");
        assert_eq!(value["launcher"], "process");
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), ready);
        let older = r#"{"type":"Ready","id":4,"launcher":"process","node":"labbox3","pid":81234,"token":"t","reattached":true}"#;
        assert!(matches!(serde_json::from_str::<ToApp>(older).unwrap(), ToApp::Ready { port: None, build: None, interface: None, .. }), "a helper that says no port, build or interface still reads");
        let files = ToHelper::Files { id: 3, request: files::Request::List { path: "~".into() } };
        let Frame::Control(json) = files.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<ToHelper>(&json).unwrap(), files);
        assert_eq!(serde_json::from_str::<ToHelper>(r#"{"type":"Detach"}"#).unwrap(), ToHelper::Detach);
        assert_eq!(serde_json::to_string(&ToHelper::Stop { id: 2, force: false }).unwrap(), r#"{"type":"Stop","id":2}"#, "an unforced stop reads as it did before `force`");
        assert_eq!(serde_json::from_str::<ToHelper>(r#"{"type":"Stop","id":2}"#).unwrap(), ToHelper::Stop { id: 2, force: false });
        assert_eq!(serde_json::to_string(&ToHelper::Stop { id: 2, force: true }).unwrap(), r#"{"type":"Stop","id":2,"force":true}"#);
        assert_eq!(serde_json::from_str::<ToApp>(r#"{"type":"Replaced"}"#).unwrap(), ToApp::Replaced);
        let not_stopped = ToApp::NotStopped { id: 2, message: "Julia was not stopped.".into() };
        let Frame::Control(json) = not_stopped.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&json).unwrap()["type"], "NotStopped");
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), not_stopped);
        let start = ToHelper::StartRuntime { id: 5, job: None, engine: ENGINE_PLUTO.into(), install: false, attach_only: false };
        assert_eq!(serde_json::to_string(&start).unwrap(), r#"{"type":"StartRuntime","id":5,"job":null,"engine":"pluto","install":false}"#);
        assert_eq!(serde_json::from_str::<ToHelper>(r#"{"type":"StartRuntime","id":5,"engine":"pluto","install":false}"#).unwrap(), start);
        let attach = ToHelper::StartRuntime { id: 6, job: None, engine: ENGINE_PLUTO.into(), install: false, attach_only: true };
        assert_eq!(serde_json::to_string(&attach).unwrap(), r#"{"type":"StartRuntime","id":6,"job":null,"engine":"pluto","install":false,"attach_only":true}"#);
        assert_eq!(serde_json::from_str::<ToHelper>(&serde_json::to_string(&attach).unwrap()).unwrap(), attach);
        assert_eq!(ToApp::NotRunning { id: 6 }.answers(), Some(6));
        let julia = Item { kind: KIND_RUNTIME.into(), name: "Julia 1.12.6".into(), size_mb: Some(289), place: Some("/home/ada/.cache/endeavor/julia-1.12.6".into()) };
        let needs = ToApp::NeedsInstall { id: 5, items: vec![julia.clone()] };
        let Frame::Control(json) = needs.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), needs);
        assert_eq!(needs.answers(), Some(5));
        let unseen: Item = serde_json::from_str(r#"{"kind":"kernel","name":"R 4.5.1","size_mb":null,"place":null}"#).unwrap();
        assert_eq!(unseen.kind, "kernel", "a kind nobody here knows still reads");
        assert_eq!(julia.to_string(), "Julia 1.12.6 (about 289 MB, into /home/ada/.cache/endeavor/julia-1.12.6)");
        assert_eq!(unseen.to_string(), "R 4.5.1");
        assert_eq!(items_text(&[julia.clone(), unseen.clone()]), "Julia 1.12.6 (about 289 MB, into /home/ada/.cache/endeavor/julia-1.12.6) and R 4.5.1");
        let found = ToApp::Found { name: "Julia".into(), version: "1.12.6".into(), path: "/opt/julia/bin/julia".into() };
        let Frame::Control(json) = found.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), found);
        assert_eq!(ToApp::Stopped { id: 3 }.answers(), Some(3));
        assert_eq!(ToApp::StartCancelled { id: 4 }.answers(), Some(4));
        assert_eq!(ToApp::Died { status: String::new(), log_tail: Vec::new() }.answers(), None);
        assert_eq!(ToApp::Progress { line: String::new() }.answers(), None);
        let hello = ToApp::Hello { protocol: PROTOCOL, version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: true, launcher: String::new() };
        let Frame::Control(json) = hello.frame() else { panic!() };
        assert_eq!(serde_json::from_slice::<ToApp>(&json).unwrap(), hello);
        let old_hello = r#"{"type":"Hello","version":"0.1.0","node":"labbox3","home":"/home/ada"}"#;
        assert!(matches!(serde_json::from_str::<ToApp>(old_hello).unwrap(), ToApp::Hello { protocol: 0, slurm: false, uploads: false, .. }), "a helper from before uploads and the protocol number");
    }
}

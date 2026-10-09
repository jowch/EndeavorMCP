//! The client half of a remote connection (docs/plugins-and-remote.md): run
//! `ssh` to a server with a bootstrap script that installs this build's helper
//! (when allowed),
//! speak the wire protocol to `endeavor connect` there, start or attach to the
//! runtime, and serve its port on a loopback port of this computer. Std threads
//! and blocking I/O; nothing here needs a window or a prompt.

mod channel;
mod listener;
mod machines;
mod session;
mod ssh;

pub use channel::{CLOSED, Channel, Hello, Notice, Runtime, StartError, StartOptions, died_reason};
pub use listener::{Listener, Messages, Refuse};
pub use session::{Config, FoundInfo, HelloInfo, HelperFor, InstallInfo, JobInfo, OnEvent, Outcome, QueueInfo, RuntimeInfo, Session, SessionEvent, State, Status, Want};
pub use machines::{Cluster, IdleStop, Launcher, MachinesFile, Server, ssh_config_hosts, valid_id};
pub(crate) use ssh::no_window;
pub use ssh::{Auth, Cancel, ConnectError, Event, NeedsInstall, Options, Running, Transport, bootstrap_script, connect, no_helper, start, test, this_platform, valid_host};

/// A folder of a unit test's own under `target/tmp`, emptied.
#[cfg(test)]
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("client-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

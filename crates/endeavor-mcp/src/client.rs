mod askpass;
mod channel;
mod listener;
mod machines;
mod session;
mod ssh;

pub use askpass::{AnswerFn, Asker, Reply};
pub use channel::{CLOSED, Channel, Hello, Notice, Runtime, StartError, StartOptions, died_reason};
pub use listener::{Listener, Messages, Refuse};
pub use session::{Config, FoundInfo, Gone, HelloInfo, HelperFor, InstallInfo, JobInfo, OnEvent, Outcome, QueueInfo, RuntimeInfo, Session, SessionEvent, State, Status, Want};
pub use machines::{Cluster, IdleStop, Launcher, MachinesFile, Server, ssh_config_hosts, valid_id};
pub use ssh::no_window;
pub use ssh::{Auth, Cancel, ConnectError, Event, NeedsInstall, Options, Running, SIGN_IN_GAVE_UP, Transport, bootstrap_script, connect, no_helper, start, test, this_platform, valid_host};

#[cfg(test)]
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("client-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

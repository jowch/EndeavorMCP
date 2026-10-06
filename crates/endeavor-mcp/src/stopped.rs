//! `DIR/stopped`, the note a stop leaves for the others still attached to the
//! runtime (or waiting for its job): what was stopped and how. The file says
//! "ID HOW": ID is the runtime's pid, or `job:` and the job's id for a job not
//! yet running; HOW is `stop` (`endeavor stop`) or `connection` (a client's
//! Stop). A note with the pid alone, which an older `endeavor stop` writes, is
//! `stop`. One note at a time: a later stop replaces it.

use std::path::Path;

/// What a stop ended.
#[derive(Clone, Copy)]
pub enum Of<'a> {
    Runtime(i32),
    Job(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum How {
    /// `endeavor stop`.
    Stop,
    /// A client sent Stop.
    Connection,
}

const FILE: &str = "stopped";

impl Of<'_> {
    fn key(&self) -> String {
        match self {
            Of::Runtime(pid) => pid.to_string(),
            Of::Job(job) => format!("job:{job}"),
        }
    }
}

/// Note that `of` is being stopped.
pub fn mark(dir: &Path, of: Of, how: How) {
    let how = match how {
        How::Stop => "stop",
        How::Connection => "connection",
    };
    let _ = std::fs::write(dir.join(FILE), format!("{} {how}", of.key()));
}

/// How `of` was stopped, if the note names it.
pub fn why(dir: &Path, of: Of) -> Option<How> {
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    let mut words = text.split_whitespace();
    (words.next()? == of.key()).then(|| if words.next() == Some("connection") { How::Connection } else { How::Stop })
}

/// Take the note back if it names `of`: the stop didn't happen.
pub fn unmark(dir: &Path, of: Of) {
    if why(dir, of).is_some() {
        let _ = std::fs::remove_file(dir.join(FILE));
    }
}

/// A runtime is starting: a note about an earlier one is no use, and its pid may come again.
/// A job's note stays: ids aren't reused, and a helper still waiting on that job may not have read it.
pub fn clear(dir: &Path) {
    let job = std::fs::read_to_string(dir.join(FILE)).is_ok_and(|text| text.starts_with("job:"));
    if !job {
        let _ = std::fs::remove_file(dir.join(FILE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_the_named_runtime_or_job_was_stopped() {
        let dir = crate::client::scratch("stopped-note");
        mark(&dir, Of::Runtime(7), How::Connection);
        assert_eq!(why(&dir, Of::Runtime(7)), Some(How::Connection));
        assert_eq!((why(&dir, Of::Runtime(70)), why(&dir, Of::Job("7"))), (None, None));
        std::fs::write(dir.join(FILE), "7").unwrap();
        assert_eq!(why(&dir, Of::Runtime(7)), Some(How::Stop), "an older build writes the pid alone");
        mark(&dir, Of::Job("42"), How::Connection);
        clear(&dir);
        assert_eq!(why(&dir, Of::Job("42")), Some(How::Connection), "a job's note stays");
        unmark(&dir, Of::Runtime(7));
        assert_eq!(why(&dir, Of::Job("42")), Some(How::Connection), "another's note stays");
        mark(&dir, Of::Runtime(7), How::Stop);
        clear(&dir);
        assert_eq!(why(&dir, Of::Runtime(7)), None);
    }
}

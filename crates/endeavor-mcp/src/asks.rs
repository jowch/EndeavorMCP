//! Calls waiting for the user: in Ask to run, an agent's call that runs code
//! waits here until the app answers (Endeavor's docs/other-agents.md, work
//! item 3); in
//! Manual, so does a call that changes the notebook. The app sees each
//! waiting call in its event stream (`asks`), shows it as a run or edit card,
//! and answers with `endeavor/answer_run`. The call stops waiting if the
//! agent cancels it or hangs up.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// How often a waiting call checks whether its agent is still there.
const CHECK: Duration = Duration::from_millis(250);

pub struct Asks {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    /// Ids go on from the runtime's start time, so an app that saw another
    /// runtime's asks doesn't take a new one for an old one.
    next: u64,
    waiting: Vec<Waiting>,
}

struct Waiting {
    id: u64,
    owner: String,
    /// The JSON-RPC id of the agent's request, which a cancellation names.
    request: Value,
    /// What the app sees: `{id, owner, call_id, tool, arguments, since}`.
    shown: Value,
    answer: Option<Answer>,
    cancelled: bool,
}

/// The user's answer to a run.
pub struct Answer {
    pub allow: bool,
    /// Cells the user's own run reached while the call waited, each with its
    /// `last_run` before that run (`[{cell_id, last_run}]`).
    pub user_ran: Value,
}

/// How a wait ended.
pub enum Outcome {
    Answered(Answer),
    /// The agent cancelled the call.
    Cancelled,
    /// The agent's connection closed.
    Gone,
}

/// One call to wait for an answer about.
pub struct Ask<'a> {
    pub owner: &'a str,
    pub call_id: Option<&'a str>,
    pub request: &'a Value,
    pub tool: &'a str,
    pub arguments: &'a Value,
    pub since: f64,
}

impl Asks {
    pub fn new(now: f64) -> Asks {
        Asks { state: Mutex::new(State { next: (now * 1000.0) as u64, waiting: Vec::new() }), changed: Condvar::new() }
    }

    /// The calls waiting, in the order they came.
    pub fn list(&self) -> Vec<Value> {
        self.state.lock().unwrap().waiting.iter().filter(|w| w.answer.is_none() && !w.cancelled).map(|w| w.shown.clone()).collect()
    }

    /// Start waiting on `ask`; its id.
    pub fn add(&self, ask: Ask) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let id = state.next;
        let shown = json!({ "id": id, "owner": ask.owner, "call_id": ask.call_id, "tool": ask.tool, "arguments": ask.arguments, "since": ask.since });
        state.waiting.push(Waiting { id, owner: ask.owner.to_owned(), request: ask.request.clone(), shown, answer: None, cancelled: false });
        id
    }

    /// `endeavor/answer_run`: the user's answer to ask `id`.
    pub fn answer(&self, id: u64, answer: Answer) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let waiting = state.waiting.iter_mut().find(|w| w.id == id && w.answer.is_none() && !w.cancelled);
        let Some(waiting) = waiting else { return Err(format!("ArgumentError: no_ask::No run waits for an answer as {id}")) };
        waiting.answer = Some(answer);
        self.changed.notify_all();
        Ok(())
    }

    /// The agent of session `owner` cancelled its request `request`.
    pub fn cancel(&self, owner: &str, request: &Value) -> bool {
        let mut state = self.state.lock().unwrap();
        let found = state.waiting.iter_mut().find(|w| w.owner == owner && w.request == *request && w.answer.is_none());
        let Some(waiting) = found else { return false };
        waiting.cancelled = true;
        self.changed.notify_all();
        true
    }

    /// Wait for ask `id` to be answered or given up (`gone`: whether the
    /// agent's connection closed); it's forgotten either way.
    pub fn wait(&self, id: u64, gone: &dyn Fn() -> bool) -> Outcome {
        let mut state = self.state.lock().unwrap();
        loop {
            let at = state.waiting.iter().position(|w| w.id == id).expect("waited on an ask it added");
            if state.waiting[at].answer.is_some() || state.waiting[at].cancelled {
                let waiting = state.waiting.remove(at);
                return waiting.answer.map_or(Outcome::Cancelled, Outcome::Answered);
            }
            let (after, timed_out) = self.changed.wait_timeout(state, CHECK).unwrap();
            state = after;
            if !timed_out.timed_out() {
                continue;
            }
            // Checked without the lock: it's a system call on the socket.
            drop(state);
            if gone() {
                let mut state = self.state.lock().unwrap();
                state.waiting.retain(|w| w.id != id);
                return Outcome::Gone;
            }
            state = self.state.lock().unwrap();
        }
    }
}

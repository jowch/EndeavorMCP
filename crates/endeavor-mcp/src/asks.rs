//! Calls waiting for the user: in Ask to run, an agent's call that runs code
//! waits here until the app answers (Endeavor's docs/other-agents.md, work
//! item 3); in
//! Manual, so does a call that changes the notebook. The app sees each
//! waiting call in its event stream (`asks`), shows it as a run or edit card,
//! and answers with `endeavor/answer_run`. The call stops waiting if the
//! agent cancels it or hangs up.
//!
//! A call waits for its answer only until its deadline: agents end a tool
//! call after about 60 seconds (Claude Code's default, progress or not), and
//! a user can be away longer. The ask then stays up, without a call, and the
//! same call made again by the same session, on the same code, waits on it
//! once more; an approval the user gave meanwhile is that call's at once. Any
//! other call by the session takes it down, and so does a refusal: the next
//! call asks afresh. The app sees which asks a call waits on (`waiting`), so
//! it can keep one left up past the end of the agent's turn: the agent is told
//! to stop calling after a while and let the user answer when they are back.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

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
    /// The JSON-RPC id of the agent's request, which a cancellation names;
    /// null while no call waits on the ask.
    request: Value,
    /// The call's tool and arguments, and the code of its notebook when it
    /// asked, which a later call must match to wait on it.
    tool: String,
    arguments: Value,
    code: Option<u64>,
    /// When it was first asked: the same call made again goes on counting from here.
    asked: Instant,
    /// What the app sees: `{id, owner, call_id, tool, arguments, since}`, and `waiting` from `list`.
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
    /// The deadline came first, this long after the ask was first made. The
    /// ask is still up, for the same call made again.
    Unanswered(Duration),
}

/// One call to wait for an answer about.
pub struct Ask<'a> {
    pub owner: &'a str,
    pub call_id: Option<&'a str>,
    pub request: &'a Value,
    pub tool: &'a str,
    pub arguments: &'a Value,
    /// The code of the notebook the call is about, as `Notebooks::code_print` gives it, if any.
    pub code: Option<u64>,
    pub since: f64,
}

impl Asks {
    pub fn new(now: f64) -> Asks {
        Asks { state: Mutex::new(State { next: (now * 1000.0) as u64, waiting: Vec::new() }), changed: Condvar::new() }
    }

    /// The asks up, in the order they came, each with `waiting`: whether a call waits on it now.
    pub fn list(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        let up = state.waiting.iter().filter(|w| w.answer.is_none() && !w.cancelled);
        up.map(|w| {
            let mut shown = w.shown.clone();
            shown["waiting"] = (!w.request.is_null()).into();
            shown
        })
        .collect()
    }

    /// Session `owner` makes a call to `tool` with `arguments`: its asks left
    /// up for any other call are taken down, since the agent went on to
    /// something else (with no `tool`, all of them). Whether any were.
    pub fn moved_on(&self, owner: &str, tool: &str, arguments: &Value) -> bool {
        let mut state = self.state.lock().unwrap();
        let before = state.waiting.len();
        state.waiting.retain(|w| !(w.owner == owner && w.request.is_null() && (w.tool != tool || w.arguments != *arguments)));
        state.waiting.len() != before
    }

    /// Start waiting on `ask`; its id. The same call made again on the same
    /// code after its wait ended unanswered waits on that ask instead; the
    /// session's other asks without a call are given up.
    pub fn add(&self, ask: Ask) -> u64 {
        let mut state = self.state.lock().unwrap();
        let left = |w: &Waiting| w.owner == ask.owner && w.request.is_null() && !w.cancelled;
        let same = |w: &Waiting| w.tool == ask.tool && w.arguments == *ask.arguments && w.code == ask.code;
        if let Some(waiting) = state.waiting.iter_mut().find(|w| left(w) && same(w)) {
            waiting.request = ask.request.clone();
            return waiting.id;
        }
        state.waiting.retain(|w| !left(w));
        state.next += 1;
        let id = state.next;
        let shown = json!({ "id": id, "owner": ask.owner, "call_id": ask.call_id, "tool": ask.tool, "arguments": ask.arguments, "since": ask.since });
        let (tool, arguments, code) = (ask.tool.to_owned(), ask.arguments.clone(), ask.code);
        state.waiting.push(Waiting { id, owner: ask.owner.to_owned(), request: ask.request.clone(), tool, arguments, code, asked: Instant::now(), shown, answer: None, cancelled: false });
        id
    }

    /// `endeavor/answer_run`: the user's answer to ask `id`. A refusal of an
    /// ask no call waits on takes it down instead of being kept: it may be the
    /// app's at the end of a turn, not the user's, and the next call asks afresh.
    pub fn answer(&self, id: u64, answer: Answer) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let at = state.waiting.iter().position(|w| w.id == id && w.answer.is_none() && !w.cancelled);
        let Some(at) = at else { return Err(format!("ArgumentError: no_ask::No run waits for an answer as {id}")) };
        if !answer.allow && state.waiting[at].request.is_null() {
            state.waiting.remove(at);
            return Ok(());
        }
        state.waiting[at].answer = Some(answer);
        self.changed.notify_all();
        Ok(())
    }

    /// The agent of session `owner` cancelled its request `request`.
    pub fn cancel(&self, owner: &str, request: &Value) -> bool {
        let mut state = self.state.lock().unwrap();
        let found = state.waiting.iter_mut().find(|w| w.owner == owner && !w.request.is_null() && w.request == *request && w.answer.is_none());
        let Some(waiting) = found else { return false };
        waiting.cancelled = true;
        self.changed.notify_all();
        true
    }

    /// Wait for ask `id` to be answered or given up (`gone`: whether the
    /// agent's connection closed), and it's forgotten; or until `deadline`,
    /// and it stays up without a call.
    pub fn wait(&self, id: u64, gone: &dyn Fn() -> bool, deadline: Instant) -> Outcome {
        let mut state = self.state.lock().unwrap();
        loop {
            let at = state.waiting.iter().position(|w| w.id == id).expect("waited on an ask it added");
            if state.waiting[at].answer.is_some() || state.waiting[at].cancelled {
                let waiting = state.waiting.remove(at);
                return waiting.answer.map_or(Outcome::Cancelled, Outcome::Answered);
            }
            if Instant::now() >= deadline {
                state.waiting[at].request = Value::Null;
                return Outcome::Unanswered(state.waiting[at].asked.elapsed());
            }
            let (after, timed_out) = self.changed.wait_timeout(state, CHECK.min(deadline.saturating_duration_since(Instant::now()))).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ask<'a>(owner: &'a str, request: &'a Value, tool: &'a str, arguments: &'a Value) -> Ask<'a> {
        Ask { owner, call_id: None, request, tool, arguments, code: Some(1), since: 0.0 }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(50)
    }

    #[test]
    fn a_wait_ends_at_its_deadline_and_the_ask_stays_up_for_the_same_call_again() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        assert_eq!(asks.list().len(), 1, "the user still sees it");
        // A cancellation of the call that stopped waiting names no ask now.
        assert!(!asks.cancel("1", &json!(7)));
        // The same call again waits on the same ask, and gets the answer given meanwhile.
        asks.answer(id, Answer { allow: true, user_ran: json!([]) }).unwrap();
        assert_eq!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Answered(Answer { allow: true, .. })));
        assert!(asks.list().is_empty());
    }

    #[test]
    fn the_app_sees_whether_a_call_waits_and_the_same_call_again_counts_from_the_first_ask() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert_eq!(asks.list()[0]["waiting"], true);
        let Outcome::Unanswered(first) = asks.wait(id, &|| false, soon()) else { panic!("answered") };
        assert_eq!(asks.list()[0]["waiting"], false, "left up: the app may keep its card past the turn");
        assert_eq!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert_eq!(asks.list()[0]["waiting"], true);
        let Outcome::Unanswered(second) = asks.wait(id, &|| false, soon()) else { panic!("answered") };
        assert!(second >= first + Duration::from_millis(50), "{first:?} then {second:?}");
    }

    #[test]
    fn the_same_call_again_waits_again_and_can_be_cancelled() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        assert_eq!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert!(asks.cancel("1", &json!(8)));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Cancelled));
        assert!(asks.list().is_empty());
    }

    #[test]
    fn another_call_or_session_does_not_take_an_ask_left_up_and_the_sessions_next_ask_ends_it() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let other_cell = json!({ "notebook_id": "n", "cell_id": "b" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        // Another session's same call is its own ask, and leaves this one up.
        let theirs = asks.add(ask("2", &json!(7), "execute_cell", &cell));
        assert_ne!(theirs, id);
        assert_eq!(asks.list().len(), 2);
        // This session's next call to something else: the agent went on, so the one left up goes.
        let next = asks.add(ask("1", &json!(9), "execute_cell", &other_cell));
        assert!(next != id && next != theirs);
        let shown: Vec<u64> = asks.list().iter().map(|a| a["id"].as_u64().unwrap()).collect();
        assert_eq!(shown, [theirs, next]);
        assert!(asks.answer(id, Answer { allow: true, user_ran: json!([]) }).is_err(), "nothing waits on it any more");
    }

    #[test]
    fn any_other_call_of_the_session_takes_down_an_ask_left_up_and_the_same_one_does_not() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        assert!(!asks.moved_on("1", "execute_cell", &cell), "the same call keeps it");
        assert!(!asks.moved_on("2", "read_cell", &cell), "another session's call doesn't touch it");
        assert_eq!(asks.list().len(), 1);
        // A read, or an edit that isn't held: the agent went on, so an approval given now can't reach a later run.
        assert!(asks.moved_on("1", "edit_cell", &json!({ "notebook_id": "n", "cell_id": "a", "code": "2" })));
        assert!(asks.list().is_empty());
        assert!(asks.answer(id, Answer { allow: true, user_ran: json!([]) }).is_err());
        assert_ne!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id, "the next run asks afresh");
    }

    #[test]
    fn the_same_call_on_changed_code_asks_afresh() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        // The user allows the card raised for the code as it was; the code changes (another session, say).
        asks.answer(id, Answer { allow: true, user_ran: json!([]) }).unwrap();
        let request = json!(8);
        let again = asks.add(Ask { code: Some(2), ..ask("1", &request, "execute_cell", &cell) });
        assert_ne!(again, id, "the approval was for other code");
        let shown: Vec<u64> = asks.list().iter().map(|a| a["id"].as_u64().unwrap()).collect();
        assert_eq!(shown, [again]);
    }

    #[test]
    fn a_refusal_of_an_ask_left_up_takes_it_down_and_the_same_call_asks_afresh() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered(_)));
        // As the app refuses what is still up when the turn ends.
        asks.answer(id, Answer { allow: false, user_ran: json!([]) }).unwrap();
        assert!(asks.list().is_empty());
        let again = asks.add(ask("1", &json!(8), "execute_cell", &cell));
        assert_ne!(again, id);
        assert!(matches!(asks.wait(again, &|| false, soon()), Outcome::Unanswered(_)), "no refusal carried over");
        // A refusal while a call waits is still that call's answer.
        assert_eq!(asks.add(ask("1", &json!(9), "execute_cell", &cell)), again);
        asks.answer(again, Answer { allow: false, user_ran: json!([]) }).unwrap();
        assert!(matches!(asks.wait(again, &|| false, soon()), Outcome::Answered(Answer { allow: false, .. })));
    }

    #[test]
    fn a_call_waiting_on_an_ask_is_not_taken_by_the_same_call_made_meanwhile() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        // Still waited on (its call is under way): the same call made now is another ask.
        assert_ne!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert_eq!(asks.list().len(), 2);
    }
}

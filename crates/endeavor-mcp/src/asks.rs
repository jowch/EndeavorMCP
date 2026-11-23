use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CHECK: Duration = Duration::from_millis(250);

pub struct Asks {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    next: u64,
    waiting: Vec<Waiting>,
}

struct Waiting {
    id: u64,
    owner: String,
    request: Value,
    tool: String,
    arguments: Value,
    code: Option<u64>,
    shown: Value,
    answer: Option<Answer>,
    cancelled: bool,
}

pub struct Answer {
    pub allow: bool,
    pub user_ran: Value,
    pub packages: Option<Value>,
}

pub enum Outcome {
    Answered(Answer),
    Cancelled,
    Gone,
    Unanswered,
}

pub struct Ask<'a> {
    pub owner: &'a str,
    pub call_id: Option<&'a str>,
    pub request: &'a Value,
    pub tool: &'a str,
    pub arguments: &'a Value,
    pub code: Option<u64>,
    pub since: f64,
}

impl Asks {
    pub fn new(now: f64) -> Asks {
        Asks { state: Mutex::new(State { next: (now * 1000.0) as u64, waiting: Vec::new() }), changed: Condvar::new() }
    }

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

    pub fn moved_on(&self, owner: &str, tool: &str, arguments: &Value) -> bool {
        let mut state = self.state.lock().unwrap();
        let before = state.waiting.len();
        state.waiting.retain(|w| !(w.owner == owner && w.request.is_null() && (w.tool != tool || w.arguments != *arguments)));
        state.waiting.len() != before
    }

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
        state.waiting.push(Waiting { id, owner: ask.owner.to_owned(), request: ask.request.clone(), tool, arguments, code, shown, answer: None, cancelled: false });
        id
    }

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

    pub fn cancel(&self, owner: &str, request: &Value) -> bool {
        let mut state = self.state.lock().unwrap();
        let found = state.waiting.iter_mut().find(|w| w.owner == owner && !w.request.is_null() && w.request == *request && w.answer.is_none());
        let Some(waiting) = found else { return false };
        waiting.cancelled = true;
        self.changed.notify_all();
        true
    }

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
                return Outcome::Unanswered;
            }
            let (after, timed_out) = self.changed.wait_timeout(state, CHECK.min(deadline.saturating_duration_since(Instant::now()))).unwrap();
            state = after;
            if !timed_out.timed_out() {
                continue;
            }
            drop(state);
            if crate::mcp::waiting_on(crate::mcp::Waits::ANSWER, gone) {
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
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        assert_eq!(asks.list().len(), 1, "the user still sees it");
        assert!(!asks.cancel("1", &json!(7)));
        asks.answer(id, Answer { allow: true, user_ran: json!([]), packages: None }).unwrap();
        assert_eq!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Answered(Answer { allow: true, .. })));
        assert!(asks.list().is_empty());
    }

    #[test]
    fn the_app_sees_whether_a_call_waits_on_an_ask() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert_eq!(asks.list()[0]["waiting"], true);
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        assert_eq!(asks.list()[0]["waiting"], false, "left up: the app may keep its card past the turn");
        assert_eq!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert_eq!(asks.list()[0]["waiting"], true);
    }

    #[test]
    fn the_same_call_again_waits_again_and_can_be_cancelled() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
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
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        let theirs = asks.add(ask("2", &json!(7), "execute_cell", &cell));
        assert_ne!(theirs, id);
        assert_eq!(asks.list().len(), 2);
        let next = asks.add(ask("1", &json!(9), "execute_cell", &other_cell));
        assert!(next != id && next != theirs);
        let shown: Vec<u64> = asks.list().iter().map(|a| a["id"].as_u64().unwrap()).collect();
        assert_eq!(shown, [theirs, next]);
        assert!(asks.answer(id, Answer { allow: true, user_ran: json!([]), packages: None }).is_err(), "nothing waits on it any more");
    }

    #[test]
    fn any_other_call_of_the_session_takes_down_an_ask_left_up_and_the_same_one_does_not() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        assert!(!asks.moved_on("1", "execute_cell", &cell), "the same call keeps it");
        assert!(!asks.moved_on("2", "read_cell", &cell), "another session's call doesn't touch it");
        assert_eq!(asks.list().len(), 1);
        assert!(asks.moved_on("1", "edit_cell", &json!({ "notebook_id": "n", "cell_id": "a", "code": "2" })));
        assert!(asks.list().is_empty());
        assert!(asks.answer(id, Answer { allow: true, user_ran: json!([]), packages: None }).is_err());
        assert_ne!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id, "the next run asks afresh");
    }

    #[test]
    fn the_same_call_on_changed_code_asks_afresh() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        asks.answer(id, Answer { allow: true, user_ran: json!([]), packages: None }).unwrap();
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
        assert!(matches!(asks.wait(id, &|| false, soon()), Outcome::Unanswered));
        asks.answer(id, Answer { allow: false, user_ran: json!([]), packages: None }).unwrap();
        assert!(asks.list().is_empty());
        let again = asks.add(ask("1", &json!(8), "execute_cell", &cell));
        assert_ne!(again, id);
        assert!(matches!(asks.wait(again, &|| false, soon()), Outcome::Unanswered), "no refusal carried over");
        assert_eq!(asks.add(ask("1", &json!(9), "execute_cell", &cell)), again);
        asks.answer(again, Answer { allow: false, user_ran: json!([]), packages: None }).unwrap();
        assert!(matches!(asks.wait(again, &|| false, soon()), Outcome::Answered(Answer { allow: false, .. })));
    }

    #[test]
    fn a_call_waiting_on_an_ask_is_not_taken_by_the_same_call_made_meanwhile() {
        let asks = Asks::new(0.0);
        let cell = json!({ "notebook_id": "n", "cell_id": "a" });
        let id = asks.add(ask("1", &json!(7), "execute_cell", &cell));
        assert_ne!(asks.add(ask("1", &json!(8), "execute_cell", &cell)), id);
        assert_eq!(asks.list().len(), 2);
    }
}

//! The checks a task's `checks.json` names. Each reads the proxy's log of tool
//! calls, the notebooks the checker read afterwards, or what the agent did and
//! said last. A check marked `"soft": true` is reported but doesn't fail the task.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::log::Call;
use crate::mcp::Notebook;

/// What the agent left, for the checks to read.
pub struct Evidence<'a> {
    pub calls: &'a [Call],
    pub notebooks: &'a [Notebook],
    /// The agent's own tools it used (for Claude, from its transcript), by name.
    pub agent_tools: &'a [String],
    pub final_message: &'a str,
}

pub struct Outcome {
    pub name: String,
    pub soft: bool,
    pub passed: bool,
    pub detail: String,
}

pub fn run(spec: &Value, ev: &Evidence) -> Outcome {
    let kind = spec["check"].as_str().unwrap_or_default();
    let soft = spec["soft"] == true;
    let (passed, detail) = match check(kind, spec, ev) {
        Ok(detail) => (true, detail),
        Err(detail) => (false, detail),
    };
    Outcome { name: describe(kind, spec), soft, passed, detail }
}

fn describe(kind: &str, spec: &Value) -> String {
    let mut fields: Vec<String> = spec.as_object().into_iter().flatten().filter(|(k, _)| *k != "check" && *k != "soft").map(|(k, v)| format!("{k}={v}")).collect();
    fields.sort();
    if fields.is_empty() { kind.to_owned() } else { format!("{kind}({})", fields.join(", ")) }
}

fn check(kind: &str, spec: &Value, ev: &Evidence) -> Result<String, String> {
    let str_list = |key: &str| -> Vec<String> { spec[key].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_owned).collect() };
    match kind {
        // A tool was called between `min` (default 1) and `max` (default any) times.
        "called" => {
            let tool = spec["tool"].as_str().unwrap_or_default();
            let n = ev.calls.iter().filter(|c| c.tool == tool).count() as u64;
            let (min, max) = (spec["min"].as_u64().unwrap_or(1), spec["max"].as_u64().unwrap_or(u64::MAX));
            if (min..=max).contains(&n) { Ok(format!("{n} calls")) } else { Err(format!("{n} calls")) }
        }
        "not_called" => {
            let tool = spec["tool"].as_str().unwrap_or_default();
            match ev.calls.iter().filter(|c| c.tool == tool).count() {
                0 => Ok(String::new()),
                n => Err(format!("{n} calls")),
            }
        }
        // `tool` was called after the last call that ran cells (a plot looked at
        // after it was drawn, before the agent reported).
        "called_after_last_run" => {
            let tool = spec["tool"].as_str().unwrap_or_default();
            let last_run = ev.calls.iter().rposition(runs);
            let after = last_run.map_or(0, |i| i + 1);
            if ev.calls[after.min(ev.calls.len())..].iter().any(|c| c.tool == tool) { Ok(String::new()) } else { Err(format!("no {tool} after the last run")) }
        }
        "notebooks" => {
            let want = spec["count"].as_u64().unwrap_or(1) as usize;
            if ev.notebooks.len() == want { Ok(String::new()) } else { Err(format!("{} open: {:?}", ev.notebooks.len(), ev.notebooks.iter().map(|n| &n.path).collect::<Vec<_>>())) }
        }
        "no_errored_cells" => {
            let errored: Vec<String> = ev.notebooks.iter().flat_map(|n| n.cells.iter().filter(|c| c.errored).map(move |c| format!("{}: {}", n.path, first_line(&c.code)))).collect();
            if errored.is_empty() { Ok(String::new()) } else { Err(errored.join("; ")) }
        }
        // Some cell's output contains every one of `texts`.
        "output_contains" => {
            let texts = str_list("texts");
            let found = ev.notebooks.iter().flat_map(|n| &n.cells).any(|c| texts.iter().all(|t| c.output.contains(t.as_str())));
            if found { Ok(String::new()) } else { Err("no cell's output has them".into()) }
        }
        "execution_allowed" => {
            let want = spec["value"] == true;
            let wrong: Vec<&str> = ev.notebooks.iter().filter(|n| n.execution_allowed != want).map(|n| n.path.as_str()).collect();
            if ev.notebooks.is_empty() {
                Err("no notebook open".into())
            } else if wrong.is_empty() {
                Ok(String::new())
            } else {
                Err(format!("not {want}: {wrong:?}"))
            }
        }
        // The agent's last message contains one of `texts`, ignoring case.
        "final_message_contains_any" => {
            let message = ev.final_message.to_lowercase();
            let texts = str_list("texts");
            match texts.iter().find(|t| message.contains(&t.to_lowercase())) {
                Some(t) => Ok(format!("has {t:?}")),
                None => Err(format!("last message: {:?}", clip(ev.final_message, 300))),
            }
        }
        // After a waited run came back with cells in `execution.still_running`,
        // no later call ran any of those cells again. With `require`, such a run
        // must also have happened (an agent may rightly not wait at all).
        "no_rerun_of_still_running" => {
            let mut still: HashSet<String> = HashSet::new();
            let mut saw = false;
            for call in ev.calls {
                if !still.is_empty() && runs(call) {
                    let ran = ran_cells(call);
                    if call.tool == "run_all_cells" || ran.iter().any(|c| still.contains(c)) {
                        return Err(format!("{} at {:.0}s ran {:?} again", call.tool, call.at, ran.iter().filter(|c| still.contains(*c)).collect::<Vec<_>>()));
                    }
                }
                for id in call.reply["execution"]["still_running"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    saw = true;
                    still.insert(id.to_owned());
                }
            }
            match (saw, spec["require"] == true) {
                (true, _) => Ok("a run outlasted the wait; nothing ran it again".into()),
                (false, false) => Ok("no run outlasted the wait".into()),
                (false, true) => Err("no run outlasted the wait".into()),
            }
        }
        // The agent never used these tools of its own (e.g. writing the notebook's file).
        "agent_tools_not_used" => {
            let tools = str_list("tools");
            let used: Vec<&String> = ev.agent_tools.iter().filter(|t| tools.contains(t)).collect();
            if used.is_empty() { Ok(String::new()) } else { Err(format!("used {used:?}")) }
        }
        _ => Err(format!("unknown check {kind:?}")),
    }
}

/// Whether a call runs cells.
fn runs(call: &Call) -> bool {
    match call.tool.as_str() {
        "execute_cell" | "submit_changes" | "run_all_cells" => true,
        "add_cell" => call.args["run_after"] == true,
        "allow_execution" => call.args["run_notebook"] != false,
        _ => false,
    }
}

/// The cells a running call ran: what its reply says it affected or still runs,
/// else what its arguments name.
fn ran_cells(call: &Call) -> Vec<String> {
    let mut cells: Vec<String> = Vec::new();
    cells.extend(call.reply["affected_cells"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    cells.extend(call.reply["execution"]["still_running"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    if let Some(id) = call.args["cell_id"].as_str() {
        cells.push(id.to_owned());
    }
    cells.extend(call.args["cell_ids"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
    cells
}

fn first_line(code: &str) -> &str {
    code.lines().next().unwrap_or_default()
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_owned() } else { format!("{}…", text.chars().take(max).collect::<String>()) }
}

pub fn to_json(outcomes: &[Outcome]) -> Value {
    json!(outcomes.iter().map(|o| json!({ "check": o.name, "soft": o.soft, "passed": o.passed, "detail": o.detail })).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Log;
    use std::time::Instant;

    /// A log of these (request, reply) pairs, read back as calls.
    fn calls(pairs: &[(&str, Value, Value)]) -> Vec<Call> {
        let dir = std::env::temp_dir().join(format!("smoke-checks-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut log = Log::create(&path, Instant::now());
        for (i, (tool, args, reply)) in pairs.iter().enumerate() {
            log.write("in", &json!({ "jsonrpc": "2.0", "id": i, "method": "tools/call", "params": { "name": tool, "arguments": args } }).to_string());
            log.write("out", &json!({ "jsonrpc": "2.0", "id": i, "result": { "content": [{ "type": "text", "text": reply.to_string() }] } }).to_string());
        }
        let calls = crate::log::calls(&path);
        let _ = std::fs::remove_dir_all(&dir);
        calls
    }

    fn judge(spec: Value, calls: &[Call]) -> Outcome {
        run(&spec, &Evidence { calls, notebooks: &[], agent_tools: &[], final_message: "" })
    }

    #[test]
    fn the_log_pairs_each_call_with_its_reply() {
        let calls = calls(&[("list_notebooks", json!({}), json!([])), ("read_cell", json!({ "cell_id": "a" }), json!({ "output": "2" }))]);
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[1].tool.as_str(), &calls[1].args["cell_id"], &calls[1].reply["output"]), ("read_cell", &json!("a"), &json!("2")));
    }

    #[test]
    fn running_a_still_running_cell_again_fails_and_reading_it_doesnt() {
        let waited = ("submit_changes", json!({ "wait_for_completion": true }), json!({ "affected_cells": ["a"], "execution": { "status": "running", "still_running": ["a"] } }));
        let read = ("read_cell", json!({ "cell_id": "a" }), json!({ "running": true }));
        let again = ("execute_cell", json!({ "cell_id": "a" }), json!({ "affected_cells": ["a"] }));
        let other = ("execute_cell", json!({ "cell_id": "b" }), json!({ "affected_cells": ["b"] }));
        let spec = json!({ "check": "no_rerun_of_still_running" });
        assert!(judge(spec.clone(), &calls(&[waited.clone(), read.clone(), other])).passed);
        assert!(!judge(spec.clone(), &calls(&[waited.clone(), read, again])).passed);
        assert!(!judge(spec, &calls(&[waited, ("run_all_cells", json!({}), json!({}))])).passed);
        let required = json!({ "check": "no_rerun_of_still_running", "require": true });
        assert!(!judge(required, &calls(&[("execute_cell", json!({ "cell_id": "a" }), json!({ "affected_cells": ["a"] }))])).passed, "required, and no run outlasted the wait");
    }

    #[test]
    fn called_counts_within_its_bounds() {
        let calls = calls(&[("new_notebook", json!({}), json!({})), ("new_notebook", json!({}), json!({}))]);
        assert!(!judge(json!({ "check": "called", "tool": "new_notebook", "max": 1 }), &calls).passed);
        assert!(judge(json!({ "check": "called", "tool": "new_notebook", "min": 2 }), &calls).passed);
        assert!(!judge(json!({ "check": "not_called", "tool": "new_notebook" }), &calls).passed);
        assert!(!judge(json!({ "check": "no_such_check" }), &calls).passed, "an unknown check fails rather than passing");
    }
}

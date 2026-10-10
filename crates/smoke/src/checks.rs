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
    /// The second person's steps (inject.rs).
    pub injections: &'a [Value],
    /// The notebooks run again from their files in a fresh runtime, when a check asked for it.
    pub rerun: Option<&'a Result<Vec<Notebook>, String>>,
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
        // A tool was called between `min` (default 1) and `max` (default any)
        // times; with `ok`, counting only the calls that succeeded.
        "called" => {
            let tool = spec["tool"].as_str().unwrap_or_default();
            let ok = spec["ok"] == true;
            // A call the runtime turned away while Julia started is the agent waiting as told, not a second call.
            let n = ev.calls.iter().filter(|c| c.tool == tool && !while_starting(c) && !(ok && c.is_error)).count() as u64;
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
        // No call ran a cell again while an earlier run of it was still going.
        // A cell is in flight from a reply that says it is running or queued
        // (a run returned early, a waited run stopped waiting with it in
        // `execution.still_running`, or a read shows it running) until a read
        // shows it done. With `require_still_running`, a waited run must also
        // have stopped waiting: an agent may rightly not wait at all.
        "no_rerun_of_running_cells" => {
            let start = ev.calls.first().map_or(0.0, |c| c.at);
            let mut in_flight: HashSet<String> = HashSet::new();
            let mut saw_still_running = false;
            for call in ev.calls {
                if runs(call) {
                    let mut again: Vec<String> = ran_cells(call).into_iter().filter(|c| in_flight.contains(c)).collect();
                    if call.tool == "run_all_cells" || call.tool == "allow_execution" {
                        again = in_flight.iter().cloned().collect();
                    }
                    again.sort();
                    again.dedup();
                    if !again.is_empty() {
                        return Err(format!("{} at {:.0}s ran {again:?} while still running", call.tool, call.at - start));
                    }
                }
                track(call, &mut in_flight, &mut saw_still_running);
            }
            match (saw_still_running, spec["require_still_running"] == true) {
                (true, _) => Ok("a waited run stopped waiting; nothing ran its cells again".into()),
                (false, false) => Ok(String::new()),
                (false, true) => Err("no waited run stopped waiting".into()),
            }
        }
        // Some tool reply contains every one of `texts`: a result the agent
        // reports was really computed, not guessed.
        "reply_contains" => {
            let texts = str_list("texts");
            let found = ev.calls.iter().any(|c| {
                let reply = c.reply.to_string();
                texts.iter().all(|t| reply.contains(t.as_str()))
            });
            if found { Ok(String::new()) } else { Err("no tool reply has them".into()) }
        }
        // Every check in `checks` passes, or at least one does.
        "all_of" | "any_of" => {
            let outcomes: Vec<Outcome> = spec["checks"].as_array().into_iter().flatten().map(|c| run(c, ev)).collect();
            let detail = outcomes.iter().map(|o| format!("{} {}{}", if o.passed { "✓" } else { "✗" }, o.name, if o.detail.is_empty() { String::new() } else { format!(": {}", o.detail) })).collect::<Vec<_>>().join("; ");
            let passed = if kind == "all_of" { outcomes.iter().all(|o| o.passed) } else { outcomes.iter().any(|o| o.passed) };
            if outcomes.is_empty() {
                Err("no checks given".into())
            } else if passed {
                Ok(detail)
            } else {
                Err(detail)
            }
        }
        // The second person did everything inject.json says, so the task tested what it is for.
        "injected" => {
            let failed: Vec<&Value> = ev.injections.iter().filter(|i| i["ok"] != true).collect();
            if ev.injections.is_empty() {
                Err("the moment never came: the agent made no call it waits for".into())
            } else if let Some(f) = failed.first() {
                Err(format!("{} failed: {}", f["tool"].as_str().unwrap_or("starting"), clip(&f["reply"].to_string(), 300)))
            } else {
                Ok(format!("{} steps", ev.injections.len()))
            }
        }
        // Some cell's code contains every one of `texts`; with `not`, also none of those.
        "code_contains" => {
            let (texts, not) = (str_list("texts"), str_list("not"));
            let found = ev.notebooks.iter().flat_map(|n| &n.cells).any(|c| texts.iter().all(|t| c.code.contains(t.as_str())) && !not.iter().any(|t| c.code.contains(t.as_str())));
            if found { Ok(String::new()) } else { Err("no cell's code has them".into()) }
        }
        // After a reply containing every one of `texts` (an error the agent had
        // to deal with), a later call ran cells and succeeded.
        "ran_after_reply" => {
            let texts = str_list("texts");
            let Some(at) = ev.calls.iter().position(|c| {
                let reply = c.reply.to_string();
                texts.iter().all(|t| reply.contains(t.as_str()))
            }) else {
                return Err("no tool reply has them".into());
            };
            if ev.calls[at + 1..].iter().any(|c| runs(c) && !c.is_error) { Ok(format!("{} calls later", ev.calls.len() - at - 1)) } else { Err(format!("nothing ran after {}", ev.calls[at].tool)) }
        }
        // Run again from its file in a fresh runtime and a fresh copy of the
        // project folder, each notebook gives what the agent left. Pluto's
        // reactivity already drops what a deleted cell defined; what this
        // catches is what lives outside the notebook's memory: a file a cell
        // wrote that a deleted cell made, and the code and order as saved.
        // Cells still running when the agent ended, and pictures, aren't compared.
        "reproducible" => {
            let rerun = match ev.rerun {
                None => return Err("the notebooks weren't run again".into()),
                Some(Err(e)) => return Err(format!("running them again: {e}")),
                Some(Ok(rerun)) => rerun,
            };
            if ev.notebooks.is_empty() {
                return Err("no notebook to run again".into());
            }
            let mut differ = Vec::new();
            let mut compared = 0;
            for nb in ev.notebooks {
                let Some(again) = rerun.iter().find(|r| r.path == nb.path) else {
                    differ.push(format!("{} wasn't opened again", nb.path));
                    continue;
                };
                for cell in nb.cells.iter().filter(|c| !c.busy && !is_picture(&c.output)) {
                    compared += 1;
                    match again.cell(&cell.id) {
                        None => differ.push(format!("{}: gone", first_line(&cell.code))),
                        Some(a) if a.errored != cell.errored => differ.push(format!("{}: errored {} then {}", first_line(&cell.code), cell.errored, a.errored)),
                        Some(a) if a.output.trim_end() != cell.output.trim_end() => {
                            differ.push(format!("{}: {:?} then {:?}", first_line(&cell.code), clip(&cell.output, 120), clip(&a.output, 120)))
                        }
                        Some(_) => {}
                    }
                }
            }
            if !differ.is_empty() {
                Err(differ.join("; "))
            } else if compared == 0 {
                // Every cell was a picture or still running: the check would pass on nothing.
                Err("no cell to compare".into())
            } else {
                Ok(format!("{compared} cells the same"))
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

/// An output that is a picture, which a second run needn't draw byte for byte the same.
fn is_picture(output: &str) -> bool {
    output.contains("data:image") || output.contains("<img") || output.contains("<svg") || output.len() > 20_000
}

/// Whether the runtime turned the call away because Julia was still starting.
fn while_starting(call: &Call) -> bool {
    call.is_error && call.reply.as_str().is_some_and(|t| t.contains("is starting"))
}

/// Whether a call runs cells (each tool's own default for its run argument).
fn runs(call: &Call) -> bool {
    match call.tool.as_str() {
        "execute_cell" | "submit_changes" | "run_all_cells" => true,
        "add_cell" | "edit_cell" | "edit_cells" => call.args["run_after"] == true,
        "open_notebook" => call.args["run_notebook"] == true,
        "allow_execution" => call.args["run_notebook"] != false,
        _ => false,
    }
}

/// What a call's reply says about which cells are still going.
fn track(call: &Call, in_flight: &mut HashSet<String>, saw_still_running: &mut bool) {
    let r = &call.reply;
    let ids = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect() };
    let still = ids(&r["execution"]["still_running"]);
    if !still.is_empty() {
        *saw_still_running = true;
    }
    if runs(call) {
        let status = r["execution"]["status"].as_str().unwrap_or_default();
        // A run that ended is done with its cells; one that returned early isn't.
        if matches!(status, "completed" | "errored") && still.is_empty() {
            for c in ids(&r["affected_cells"]) {
                in_flight.remove(&c);
            }
        } else {
            in_flight.extend(ids(&r["affected_cells"]));
        }
        in_flight.extend(still);
    }
    // A cell's own state, from a read.
    if call.tool == "read_cell" && let Some(id) = r["cell_id"].as_str().or(call.args["cell_id"].as_str()) {
        if r["running"] == true || r["queued"] == true {
            in_flight.insert(id.to_owned());
        } else if r["running"] == false && r["queued"] == false {
            in_flight.remove(id);
        }
    }
    // list_notebooks names each notebook's running cells.
    if call.tool == "list_notebooks" {
        let running: HashSet<String> = r.as_array().into_iter().flatten().flat_map(|nb| ids(&nb["running"])).collect();
        in_flight.retain(|c| running.contains(c));
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

    /// A log of these (request, reply) pairs, read back as calls.
    fn calls(pairs: &[(&str, Value, Value)]) -> Vec<Call> {
        let dir = std::env::temp_dir().join(format!("smoke-checks-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut log = Log::create(&path);
        for (i, (tool, args, reply)) in pairs.iter().enumerate() {
            log.write("in", &json!({ "jsonrpc": "2.0", "id": i, "method": "tools/call", "params": { "name": tool, "arguments": args } }).to_string());
            log.write("out", &json!({ "jsonrpc": "2.0", "id": i, "result": { "content": [{ "type": "text", "text": reply.to_string() }] } }).to_string());
        }
        let calls = crate::log::calls(&path);
        let _ = std::fs::remove_dir_all(&dir);
        calls
    }

    fn judge(spec: Value, calls: &[Call]) -> Outcome {
        run(&spec, &Evidence { calls, notebooks: &[], agent_tools: &[], final_message: "", injections: &[], rerun: None })
    }

    #[test]
    fn the_log_pairs_each_call_with_its_reply() {
        let calls = calls(&[("list_notebooks", json!({}), json!([])), ("read_cell", json!({ "cell_id": "a" }), json!({ "output": "2" }))]);
        assert_eq!(calls.len(), 2);
        assert_eq!((calls[1].tool.as_str(), &calls[1].args["cell_id"], &calls[1].reply["output"]), ("read_cell", &json!("a"), &json!("2")));
    }

    #[test]
    fn running_a_running_cell_again_fails_and_reading_it_doesnt() {
        let spec = json!({ "check": "no_rerun_of_running_cells" });
        let waited = ("submit_changes", json!({ "wait_for_completion": true }), json!({ "affected_cells": ["a"], "execution": { "status": "running", "still_running": ["a"] } }));
        let early = ("submit_changes", json!({}), json!({ "affected_cells": ["a"], "execution": { "status": "running" } }));
        let running = ("read_cell", json!({ "cell_id": "a" }), json!({ "cell_id": "a", "running": true, "queued": false }));
        let done = ("read_cell", json!({ "cell_id": "a" }), json!({ "cell_id": "a", "running": false, "queued": false, "output": "5050" }));
        let again = ("execute_cell", json!({ "cell_id": "a" }), json!({ "affected_cells": ["a"], "execution": { "status": "completed" } }));
        let other = ("execute_cell", json!({ "cell_id": "b" }), json!({ "affected_cells": ["b"], "execution": { "status": "completed" } }));
        assert!(judge(spec.clone(), &calls(&[waited.clone(), running.clone(), other])).passed);
        assert!(!judge(spec.clone(), &calls(&[waited.clone(), running.clone(), again.clone()])).passed, "after the wait stopped");
        assert!(!judge(spec.clone(), &calls(&[early.clone(), again.clone()])).passed, "after a run that returned at once");
        assert!(!judge(spec.clone(), &calls(&[waited.clone(), ("run_all_cells", json!({}), json!({}))])).passed);
        assert!(judge(spec.clone(), &calls(&[early.clone(), running, done, again])).passed, "a run after it finished isn't a rerun of a running cell");
        let required = json!({ "check": "no_rerun_of_running_cells", "require_still_running": true });
        assert!(!judge(required.clone(), &calls(&[early])).passed, "required, and no waited run stopped waiting");
        assert!(judge(required, &calls(&[waited])).passed);
    }

    #[test]
    fn a_result_must_be_in_a_tool_reply_and_any_of_takes_one_branch() {
        let calls = calls(&[("read_cell", json!({ "cell_id": "a" }), json!({ "output": "5050" }))]);
        assert!(judge(json!({ "check": "reply_contains", "texts": ["5050"] }), &calls).passed);
        assert!(!judge(json!({ "check": "reply_contains", "texts": ["4950"] }), &calls).passed);
        let either = json!({ "check": "any_of", "checks": [{ "check": "reply_contains", "texts": ["4950"] }, { "check": "called", "tool": "read_cell" }] });
        assert!(judge(either, &calls).passed);
        let both = json!({ "check": "all_of", "checks": [{ "check": "reply_contains", "texts": ["4950"] }, { "check": "called", "tool": "read_cell" }] });
        assert!(!judge(both, &calls).passed);
        assert!(!judge(json!({ "check": "any_of", "checks": [] }), &calls).passed);
    }

    #[test]
    fn called_counts_within_its_bounds() {
        let calls = calls(&[("new_notebook", json!({}), json!({})), ("new_notebook", json!({}), json!({}))]);
        assert!(!judge(json!({ "check": "called", "tool": "new_notebook", "max": 1 }), &calls).passed);
        assert!(judge(json!({ "check": "called", "tool": "new_notebook", "min": 2 }), &calls).passed);
        assert!(!judge(json!({ "check": "not_called", "tool": "new_notebook" }), &calls).passed);
        assert!(!judge(json!({ "check": "no_such_check" }), &calls).passed, "an unknown check fails rather than passing");

        let mut waited = calls.clone();
        waited[0].is_error = true;
        waited[0].reply = json!("Julia is starting on this computer. To wait, call the notebook tool you want again.");
        assert!(judge(json!({ "check": "called", "tool": "new_notebook", "max": 1 }), &waited).passed, "a call turned away while Julia starts isn't counted");
    }

    fn notebook(cells: &[(&str, &str, bool)]) -> Notebook {
        let cells = cells.iter().map(|(id, output, busy)| crate::mcp::Cell { id: id.to_string(), code: format!("{id} = 1"), output: output.to_string(), busy: *busy, ..Default::default() }).collect();
        Notebook { path: "/p/a.jl".into(), execution_allowed: true, cells }
    }

    #[test]
    fn a_rerun_must_give_the_same_outputs_except_running_cells_and_pictures() {
        let left = [notebook(&[("a", "1", false), ("b", "2", false), ("c", "", true), ("d", "<img src=\"data:image/png;base64,AAA\">", false)])];
        let judge = |rerun: Result<Vec<Notebook>, String>| {
            let ev = Evidence { calls: &[], notebooks: &left, agent_tools: &[], final_message: "", injections: &[], rerun: Some(&rerun) };
            run(&json!({ "check": "reproducible" }), &ev)
        };
        assert!(judge(Ok(vec![notebook(&[("a", "1", false), ("b", "2\n", false), ("c", "3", false), ("d", "<img src=\"data:image/png;base64,BBB\">", false)])])).passed);
        let changed = judge(Ok(vec![notebook(&[("a", "1", false), ("b", "5", false), ("c", "3", false), ("d", "", false)])]));
        assert!(!changed.passed && changed.detail.contains("b = 1"), "{}", changed.detail);
        assert!(!judge(Ok(vec![notebook(&[("a", "1", false)])])).passed, "a cell missing from the rerun fails");
        assert!(!judge(Err("Julia didn't start".into())).passed);
        let busy = [notebook(&[("c", "", true)])];
        let ev = Evidence { calls: &[], notebooks: &busy, agent_tools: &[], final_message: "", injections: &[], rerun: Some(&Ok(vec![notebook(&[("c", "3", false)])])) };
        assert!(!run(&json!({ "check": "reproducible" }), &ev).passed, "nothing compared fails");
        let ev = Evidence { calls: &[], notebooks: &left, agent_tools: &[], final_message: "", injections: &[], rerun: None };
        assert!(!run(&json!({ "check": "reproducible" }), &ev).passed, "no rerun at all fails");
    }

    #[test]
    fn a_conflict_must_be_followed_by_a_run_that_worked_and_the_second_person_must_have_acted() {
        let conflict = json!({ "warnings": ["run_conflict::Another Endeavor session changed t. The edit is staged, not run."] });
        let calls = calls(&[("edit_cell", json!({ "run_after": true }), conflict.clone()), ("read_cell", json!({}), json!({})), ("submit_changes", json!({}), json!({ "execution": { "status": "completed" } }))]);
        assert!(judge(json!({ "check": "ran_after_reply", "texts": ["run_conflict"] }), &calls).passed);
        let only_read = calls[..2].to_vec();
        assert!(!judge(json!({ "check": "ran_after_reply", "texts": ["run_conflict"] }), &only_read).passed);

        let injected = |steps: &[Value]| run(&json!({ "check": "injected" }), &Evidence { calls: &[], notebooks: &[], agent_tools: &[], final_message: "", injections: steps, rerun: None }).passed;
        assert!(injected(&[json!({ "tool": "read_cell", "ok": true }), json!({ "tool": "edit_cell", "ok": true })]));
        assert!(!injected(&[json!({ "tool": "read_cell", "ok": true }), json!({ "tool": "edit_cell", "ok": false })]));
        assert!(!injected(&[]), "a moment that never came fails");
    }
}

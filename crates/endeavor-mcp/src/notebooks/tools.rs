//! The notebook tools: every rule the agent's tools follow, applied here and
//! carried out through the engine's adapter. Results and errors are what the
//! Julia runtime gave before, to the byte, but for the order of lists Julia
//! kept in hash tables (now notebook order) and errors for argument types
//! Julia reported with its own stack of candidate methods.
//!
//! An error is the text of the exception Julia raised: `ArgumentError: kind::message`
//! names its kind (see `mcp::tool_error`).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{Change, Folder, GraphQuery, Notebooks, Snapshot, absolute_path, canonical_path, uuid_value};
use crate::host_tools::julia_repr;
use crate::mcp::julia_string;

/// How long the app's report of a user's run counts. (Also sent as the
/// `timeout` of runs the engine is not asked to wait for, which it ignores.)
pub(super) const TIMEOUT_SECONDS: f64 = 60.0;
/// The most a tool call waits for the user's answer and then a run, from the
/// start of the call: under the 60 seconds that Claude Code (by default, and
/// whatever progress it hears) and Codex give a tool call. The wait for the
/// user's own run of the same cells (`ran_after_user`) comes out of it.
pub const WAIT_SECONDS: f64 = 45.0;
/// The least a run is waited for, however much of `WAIT_SECONDS` went before.
const WAIT_FLOOR_SECONDS: f64 = 5.0;
/// Claude accepts images up to about 5 MB; plots are typically tens of KB.
const MAX_IMAGE_BYTES: usize = 4_000_000;
/// How long a package step can take before the agent is told it is taking
/// longer than usual and to stop waiting unless the package log moves.
const INSTALL_USUAL_SECONDS: u64 = 600;
/// How `read_notebook_code` starts each cell, as Pluto's file does.
const CELL_MARKER: &str = "# ╔═╡ ";
/// How much of each output's text a change's receipt carries; `read_cell` has
/// the rest.
const RECEIPT_TEXT_MAX: usize = 2_000;

/// A tool's result: JSON, or a PNG with JSON describing it.
pub enum Reply {
    Json(Value),
    Image { meta: Value, png_base64: String },
}

impl Notebooks {
    /// A notebook tool call by `owner` ("" for the app): its result, or the
    /// error Julia raised for it. `folder` is the session's working folder,
    /// where `new_notebook` puts notebooks and relative paths start. `began` is
    /// when the call arrived, which a waited run's cap counts from.
    pub fn tool(&self, owner: &str, name: &str, args: &Value, folder: &Folder, began: Instant) -> Result<Reply, String> {
        self.tool_watched(owner, name, args, folder, began, &|| false)
    }

    /// `tool` for a caller that can hang up: `gone` says it has, which ends a
    /// wait in the call early.
    pub fn tool_watched(&self, owner: &str, name: &str, args: &Value, folder: &Folder, began: Instant, gone: &dyn Fn() -> bool) -> Result<Reply, String> {
        let t = Call { nbs: self, owner, args, began, gone };
        let result = match name {
            "list_notebooks" => t.list_notebooks(),
            "read_cell" => t.read_cell(),
            "view_cell_output" => return t.view_cell_output(),
            "edit_cell" => t.edit_cell(),
            "edit_cells" => t.edit_cells(),
            "add_cell" => t.add_cell(),
            "delete_cell" => t.delete_cell(),
            "execute_cell" => t.execute_cell(),
            "submit_changes" => t.submit_changes(),
            "run_all_cells" => t.run_all_cells(),
            "move_cell" => t.move_cell(),
            "fold_cell" => t.fold_cell(),
            "read_notebook_code" => t.read_notebook_code(),
            "get_cell_order" => t.get_cell_order(),
            "get_execution_order" => t.get_execution_order(),
            "get_cell_dependencies" => t.get_cell_dependencies(),
            "get_cell_dependents" => t.get_cell_dependents(),
            "find_symbol_definitions" => t.find_symbol("definitions"),
            "find_symbol_references" => t.find_symbol("references"),
            "validate_cell" => t.validate_cell(),
            "search_code" => t.search_code(),
            "session_status" => self.session_status(),
            "open_notebook" => t.open_notebook(folder),
            "new_notebook" => t.new_notebook(folder),
            "allow_execution" => t.allow_execution(),
            _ => Err(argument_error(&format!("unknown_tool::Unknown tool: '{name}'"))),
        }?;
        Ok(Reply::Json(result))
    }

    /// The engine's status, with the idle limit now in force (0: never) and whether the runtime ends itself when idle.
    fn session_status(&self) -> Result<Value, String> {
        let mut status = self.call("status", json!({}))?;
        if let Value::Object(fields) = &mut status {
            let hours = self.idle_limit_hours();
            fields.insert("idle_stop_hours".into(), if hours.is_finite() && hours > 0.0 { hours } else { 0.0 }.into());
            fields.insert("exits_when_idle".into(), self.exits_when_idle.into());
        }
        Ok(status)
    }

    /// An edit that was to run after (`run_after`), when the user chose not
    /// to run it: the edit is made, staged and not run, and its receipt says why.
    pub fn tool_unrun(&self, owner: &str, name: &str, args: &Value, folder: &Folder, began: Instant) -> Result<Reply, String> {
        let mut args = args.clone();
        args["run_after"] = false.into();
        let mut reply = self.tool(owner, name, &args, folder, began)?;
        if let Reply::Json(Value::Object(receipt)) = &mut reply
            && let Some(Value::Array(warnings)) = receipt.get_mut("warnings")
        {
            warnings.push("not_approved::The user chose not to run this yet. The edit is kept, staged and not run.".into());
        }
        Ok(reply)
    }

    /// `endeavor/run_preview`, for the app's approval card: what a run tool
    /// call would run. `cells` are the cells it targets (named by what they
    /// define), `all` means the whole notebook, `needed_ids` the cells they
    /// depend on that never ran and so run first, `dependents` counts the
    /// other cells that re-run with them, and `packages` are those a
    /// whole-notebook run loads, in notebook order. For `allow_execution`,
    /// `count` is the notebook's size whether or not it runs.
    pub fn run_preview(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let t = Call { nbs: self, owner: "", args, began: Instant::now(), gone: &|| false };
        let nb = t.notebook()?;
        let graph = self.graph(&nb.id, GraphQuery { refresh: true, edges: true, ..Default::default() })?;
        let targets: Vec<String> = if tool == "submit_changes" {
            let ids = match args.get("cell_ids") {
                Some(ids) => julia_iterate(ids)?.iter().map(|c| uuid_value(c).ok_or_else(|| malformed_uuid(c))).collect::<Result<Vec<_>, _>>()?,
                None => self.with_state(&nb.id, |state| state.pending_run(&nb)),
            };
            ids.into_iter().filter(|id| nb.cells.contains_key(id)).collect()
        } else if args.get("cell_id").is_some() {
            vec![t.cell(&nb)?]
        } else {
            Vec::new()
        };
        // allow_execution with run_notebook=false only lifts safe preview.
        let all = tool == "run_all_cells" || (tool == "allow_execution" && args.get("run_notebook").is_none_or(|run| *run != false));
        let whole = all || tool == "allow_execution";
        let needed = if all { Vec::new() } else { self.never_run_upstream(&nb, &targets)? };
        let downstream = graph.downstream_of(&targets);
        let down: Vec<String> = nb.order.iter().filter(|id| downstream.contains(*id) && !targets.contains(id)).cloned().collect();
        let cells: Vec<Value> = targets.iter().map(|id| json!({ "id": id, "name": graph.name(id), "code": nb.cells[id].code })).collect();
        let mut packages: Vec<String> = Vec::new();
        if whole {
            // The cells as they are now: in safe preview some were never analysed.
            let fresh = self.graph(&nb.id, GraphQuery { fresh: true, packages: true, ..Default::default() })?;
            for package in nb.order.iter().filter_map(|id| fresh.node(id)).flat_map(|node| &node.packages) {
                if !packages.contains(package) {
                    packages.push(package.clone());
                }
            }
        }
        Ok(json!({
            "all": all,
            "count": if whole { nb.order.len() } else { targets.len() },
            "cells": cells,
            "needed_ids": needed,
            "dependents": if all { 0 } else { down.len() },
            "dependent_ids": if all { Vec::new() } else { down },
            "packages": packages,
        }))
    }

    /// The cells `targets` depend on, directly or not, that have never run
    /// (as after leaving safe preview without a run), in notebook order. A
    /// run of the targets runs these too, so they don't fail on names those
    /// cells define. None while the notebook isn't running code.
    fn never_run_upstream(&self, nb: &Snapshot, targets: &[String]) -> Result<Vec<String>, String> {
        if targets.is_empty() || !nb.execution_allowed {
            return Ok(Vec::new());
        }
        let upstream = self.graph(&nb.id, GraphQuery { fresh: true, edges: true, ..Default::default() })?.upstream_of(targets);
        Ok(nb
            .order
            .iter()
            .filter(|id| upstream.contains(*id) && !targets.contains(id))
            .filter(|id| nb.cells.get(*id).is_some_and(|c| c.last_run == 0.0 && !nb.is_running(c)))
            .cloned()
            .collect())
    }
}

fn other_owner(change: &Change, owner: &str) -> bool {
    !change.owner.is_empty() && change.owner != owner
}

/// What a run did, for its receipt.
struct Ran {
    warnings: Vec<String>,
    /// The cells it ran.
    cells: Vec<String>,
    /// A waited run whose wait ended with it unfinished: its cells and their
    /// dependents, which may still be running. Empty otherwise.
    going: Vec<String>,
}

/// One tool call: who makes it and its arguments.
struct Call<'a> {
    nbs: &'a Notebooks,
    owner: &'a str,
    args: &'a Value,
    began: Instant,
    /// Whether the caller has hung up.
    gone: &'a dyn Fn() -> bool,
}

impl Call<'_> {
    /// What is left of `WAIT_SECONDS`.
    fn wait_left(&self) -> Duration {
        Duration::from_secs_f64(WAIT_SECONDS).saturating_sub(self.began.elapsed())
    }

    /// How long a run is waited for: what is left of `WAIT_SECONDS`, but not
    /// less than `WAIT_FLOOR_SECONDS`.
    fn run_wait(&self) -> Duration {
        self.wait_left().max(Duration::from_secs_f64(WAIT_FLOOR_SECONDS))
    }

    fn arg(&self, name: &str) -> Result<&Value, String> {
        self.args.get(name).ok_or_else(|| key_error(name))
    }

    /// The notebook `notebook_id` names, as it is now.
    fn notebook(&self) -> Result<Snapshot, String> {
        let value = self.arg("notebook_id")?;
        let shown = julia_string(value);
        let id = uuid_value(value).ok_or_else(|| argument_error(&format!("invalid_notebook_id::Invalid notebook ID: '{shown}'")))?;
        self.nbs.snapshot(&id).map_err(|e| if e.contains("notebook_not_found::") { notebook_not_found(&shown) } else { e })
    }

    /// The cell `cell_id` names.
    fn cell(&self, nb: &Snapshot) -> Result<String, String> {
        cell_named(nb, self.arg("cell_id")?)
    }

    fn now(&self) -> f64 {
        (self.nbs.clock)()
    }

    /// The owner reads a cell's code (it counts as reading it before an edit).
    fn record_read(&self, id: &str, cell: &str, code: &str) {
        let mut state = self.nbs.state.lock().unwrap();
        state.seq += 1;
        let seq = state.seq;
        let notebook = state.notebooks.entry(id.to_owned()).or_default();
        notebook.reads.insert((self.owner.to_owned(), cell.to_owned()), (code.to_owned(), seq));
    }

    /// The owner changed a cell through the tools.
    fn note_changed(&self, id: &str, cell: &str) {
        let mut state = self.nbs.state.lock().unwrap();
        state.seq += 1;
        let seq = state.seq;
        let notebook = state.notebooks.entry(id.to_owned()).or_default();
        notebook.changes.insert(cell.to_owned(), Change { owner: self.owner.to_owned(), seq });
    }

    /// An edit needs the owner to have read the cell's current code.
    fn require_fresh_read(&self, nb: &Snapshot, cell: &str) -> Result<(), String> {
        let state = self.nbs.state.lock().unwrap();
        let read = state.notebooks.get(&nb.id).and_then(|n| n.reads.get(&(self.owner.to_owned(), cell.to_owned())));
        match read {
            None => Err(argument_error(&format!("read_required::Call read_cell or read_notebook_code before editing cell {cell}"))),
            Some((code, _)) if *code != nb.cells[cell].code => Err(argument_error(&format!("stale_read::Cell {cell} changed since last read; call read_cell again"))),
            Some(_) => Ok(()),
        }
    }

    fn mark_pending(&self, id: &str, cells: &[String]) {
        let now = self.now();
        self.nbs.with_state(id, |state| {
            for cell in cells {
                state.pending.insert(cell.clone(), now);
            }
        });
    }

    fn pending_run(&self, nb: &Snapshot) -> Vec<String> {
        self.nbs.with_state(&nb.id, |state| state.pending_run(nb))
    }

    /// Carry out `ops` in the engine: its reply, with `inserted` cells and its `seq` after.
    fn apply(&self, id: &str, ops: Vec<Value>) -> Result<Value, String> {
        self.nbs.call("apply", json!({ "notebook_id": id, "ops": ops }))
    }

    /// The cells another session changed since this owner last read them that
    /// `targets` depend on (the targets included), in notebook order, as a
    /// `run_conflict::` message.
    fn run_conflict(&self, nb: &Snapshot, targets: &[String]) -> Result<Option<String>, String> {
        if self.owner.is_empty() {
            return Ok(None);
        }
        let unread: HashSet<String> = {
            let state = self.nbs.state.lock().unwrap();
            let Some(notebook) = state.notebooks.get(&nb.id) else { return Ok(None) };
            let read_seq = |cell: &String| notebook.reads.get(&(self.owner.to_owned(), cell.clone())).map_or(0, |(_, seq)| *seq);
            notebook.changes.iter().filter(|(cell, ch)| other_owner(ch, self.owner) && ch.seq > read_seq(cell)).map(|(cell, _)| cell.clone()).collect()
        };
        if unread.is_empty() {
            return Ok(None);
        }
        let graph = self.nbs.graph(&nb.id, GraphQuery { fresh: true, edges: true, ..Default::default() })?;
        let mut upstream = graph.upstream_of(targets);
        upstream.extend(targets.iter().cloned());
        let conflicted: Vec<&str> = nb.order.iter().filter(|c| unread.contains(*c) && upstream.contains(*c)).map(String::as_str).collect();
        if conflicted.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "run_conflict::Another Endeavor session changed {} since you last read them, and the cells you're running depend on them. \
             Read them (read_cell or read_notebook_code), then run again.",
            conflicted.join(", ")
        )))
    }

    fn require_no_run_conflict(&self, nb: &Snapshot, targets: &[String]) -> Result<(), String> {
        match self.run_conflict(nb, targets)? {
            Some(conflict) => Err(argument_error(&conflict)),
            None => Ok(()),
        }
    }

    /// Run cells, and the cells they need that never ran, waiting for them
    /// or not.
    fn run(&self, nb: &Snapshot, targets: &[String], wait: bool) -> Result<Ran, String> {
        self.nbs.with_state(&nb.id, |state| state.prune(nb));
        let needed = self.nbs.never_run_upstream(nb, targets)?;
        let cells: Vec<String> = needed.iter().chain(targets).cloned().collect();
        let timeout = if wait { self.run_wait().as_secs_f64() } else { TIMEOUT_SECONDS };
        let reply = self.nbs.call("run", json!({ "notebook_id": nb.id, "cells": cells, "wait": wait, "timeout": timeout }))?;
        let ids = |key: &str| reply[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        if reply.get("exited").is_some() {
            let graph = self.nbs.graph(&nb.id, GraphQuery::default()).ok();
            let cell = ids("exited").first().map(|id| graph.and_then(|g| g.name(id)).unwrap_or_else(|| id.clone()));
            return Err(argument_error(&format!("process_exited::{}", exited_message(cell.as_deref()))));
        }
        let mut warnings = Vec::new();
        let mut going = Vec::new();
        if reply["accepted"] != true {
            let status = reply["process_status"].as_str().unwrap_or_default();
            let mut warning = format!("execution_blocked::notebook is not running code (process_status={status}); pending_run kept");
            if status == "waiting_for_permission" {
                warning.push_str("; nothing ran. allow_execution is for when the user asked you to run the notebook");
            }
            warnings.push(warning);
        } else if wait {
            // The run's own cells, which the receipt looks at after the wait: its
            // cells and the cells that depend on them.
            if !ids("timed_out").is_empty() {
                let graph = self.nbs.graph(&nb.id, GraphQuery { edges: true, ..Default::default() })?;
                let dependents = graph.downstream_of(&cells);
                going = nb.order.iter().filter(|id| cells.contains(*id) || dependents.contains(*id)).cloned().collect();
            }
        } else {
            warnings.push("async_execution::cells running; read them for the result".into());
        }
        // A run of these cells was accepted: they are no longer staged, whether or not they have finished.
        if reply["accepted"] == true {
            self.nbs.with_state(&nb.id, |state| {
                for cell in &cells {
                    state.pending.remove(cell);
                    state.tool_edits.remove(cell);
                    state.user_runs.remove(cell);
                }
            });
        }
        if reply["accepted"] == true && !needed.is_empty() {
            warnings.push(format!("also_ran::Also ran {}: cells this run needs that had never run.", needed.join(", ")));
        }
        Ok(Ran { warnings, cells, going })
    }

    /// An edit's run_after: the edit stands either way, but a run that
    /// conflicts with another session's unread changes is left staged.
    fn run_or_stage(&self, nb: &Snapshot, cell: &str, run_after: bool) -> Result<Ran, String> {
        let cells = [cell.to_owned()];
        let conflict = if run_after { self.run_conflict(nb, &cells)? } else { None };
        if run_after && conflict.is_none() {
            // Not waited for: a long run would hold up the agent's other calls.
            return self.run(nb, &cells, false);
        }
        self.mark_pending(&nb.id, &cells);
        Ok(Ran { warnings: conflict.map(|c| vec![format!("{c} The edit is staged, not run.")]).unwrap_or_default(), cells: Vec::new(), going: Vec::new() })
    }

    /// Whether `targets` have all run since the user's run reached them
    /// while the agent's approval card waited: cells the tools changed and
    /// haven't run since that have run since anyway, or cells the app just
    /// said the user ran anyway (`endeavor/run_anyway`). A run of them still
    /// under way is waited for, within the call's `WAIT_SECONDS`.
    fn ran_after_user(&self, nb: &Snapshot, targets: &[String]) -> Result<bool, String> {
        let now = self.now();
        let marks: Option<Vec<(Option<f64>, Option<f64>)>> = self.nbs.with_state(&nb.id, |state| {
            state.user_runs.retain(|_, (_, at)| now - *at <= TIMEOUT_SECONDS);
            let marks = targets.iter().map(|c| {
                let edited = state.tool_edits.get(c).copied();
                let before = state.user_runs.remove(c).map(|(last_run, _)| last_run);
                (edited.is_some() || before.is_some()).then_some((edited, before))
            });
            marks.collect()
        });
        let Some(marks) = marks.filter(|_| !targets.is_empty()) else { return Ok(false) };
        let ran = |cell: &super::Cell, (edited, before): &(Option<f64>, Option<f64>)| {
            edited.is_some_and(|edited| cell.ran_since(edited)) || before.is_some_and(|before| !(cell.running || cell.queued) && cell.last_run > before)
        };
        // Leaves the run its floor.
        let deadline = Instant::now() + self.wait_left().saturating_sub(Duration::from_secs_f64(WAIT_FLOOR_SECONDS));
        loop {
            let now = self.nbs.snapshot(&nb.id)?;
            let Some(cells) = targets.iter().map(|c| now.cells.get(c)).collect::<Option<Vec<_>>>() else { return Ok(false) };
            if cells.iter().zip(&marks).all(|(cell, mark)| ran(cell, mark)) {
                return Ok(true);
            }
            if !cells.iter().any(|c| c.running || c.queued) || Instant::now() > deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The cells the tools changed that have run since, though not by the
    /// tools, in notebook order.
    fn ran_since_tool_edit(&self, nb: &Snapshot) -> Vec<String> {
        self.nbs.with_state(&nb.id, |state| nb.order.iter().filter(|id| state.tool_edits.get(*id).is_some_and(|edited| nb.cells[*id].ran_since(*edited))).cloned().collect())
    }

    /// A run left out because its cells already ran after the user's change:
    /// the usual receipt, for those cells, saying so.
    fn already_ran(&self, id: &str, mutation: Value, cells: &[String]) -> Result<Map<String, Value>, String> {
        self.nbs.with_state(id, |state| {
            for cell in cells {
                state.tool_edits.remove(cell);
            }
        });
        let them = if cells.len() == 1 { "it" } else { "them" };
        let warning = format!("already_ran::{} already ran after the user's change; not run again, so {them} ran once.", cells.join(", "));
        Ok(self.receipt(id, mutation, cells, vec![warning], None)?.0)
    }

    /// What every change reports: the notebook's order and run state after it.
    fn receipt(&self, id: &str, mutation: Value, cells_run: &[String], warnings: Vec<String>, status: Option<&str>) -> Result<(Map<String, Value>, Snapshot), String> {
        self.receipt_of_run(id, mutation, Ran { warnings, cells: cells_run.to_vec(), going: Vec::new() }, status)
    }

    /// `receipt` for a run.
    fn receipt_of_run(&self, id: &str, mutation: Value, ran: Ran, status: Option<&str>) -> Result<(Map<String, Value>, Snapshot), String> {
        let Ran { mut warnings, cells: cells_run, going } = ran;
        let cells_run = &cells_run[..];
        let nb = self.nbs.snapshot(id)?;
        let graph = self.nbs.graph(id, GraphQuery::default())?;
        // The cells of a run that ran out of its wait that are running now, read
        // after the wait: some may have finished meanwhile.
        let still_running: Vec<String> = nb.order.iter().filter(|id| going.contains(id) && nb.cells.get(*id).is_some_and(|c| c.running || c.queued)).cloned().collect();
        if !still_running.is_empty() {
            warnings.push(format!("execution_timeout::The run was still going when the wait ended after at most {WAIT_SECONDS}s"));
        }
        let status = match status {
            Some(status) => status.to_owned(),
            None => match execution_status(&nb, cells_run, &warnings).as_str() {
                "completed" if !still_running.is_empty() => "running".to_owned(),
                derived => derived.to_owned(),
            },
        };
        // A blocked run touched no cell; existing outputs are not this run's result.
        // A cell still running has the output of its last run.
        let ran: &[String] = if status == "blocked" { &[] } else { cells_run };
        let changed: Vec<Value> = ran
            .iter()
            .filter(|id| !still_running.contains(id))
            .filter_map(|id| Some((id, nb.cells.get(id)?)))
            .filter(|(_, cell)| !cell.output.is_empty())
            .map(|(id, cell)| {
                let mut entry = json!({ "cell_id": id, "output_summary": cell.output });
                if let Some(error) = &cell.error {
                    entry["error"] = error.clone();
                }
                if let Some(text) = self.output_text(&nb, id)? {
                    entry["output_text"] = json!(cut(&text, RECEIPT_TEXT_MAX));
                }
                Ok(entry)
            })
            .collect::<Result<_, String>>()?;
        let pending = self.pending_run(&nb);
        let receipt = json!({
            "applied": true,
            "mutation": mutation,
            "cell_order": nb.order,
            "execution_order": graph.execution_order(),
            "affected_cells": cells_run,
            "execution": { "status": status },
            "outputs": { "changed": changed },
            "pending_run": pending,
            "warnings": warnings,
        });
        let Value::Object(mut receipt) = receipt else { unreachable!() };
        if !still_running.is_empty() {
            receipt["execution"]["still_running"] = json!(still_running);
        }
        if nb.installing() {
            receipt.insert("packages".into(), nb.packages.clone().unwrap_or_default());
            let mut message = installing_message(&nb);
            if !still_running.is_empty() {
                message.push_str(" Don't run the cells in `execution.still_running` again: they run once the packages are ready.");
            }
            receipt.insert("message".into(), json!(message));
        } else if !still_running.is_empty() {
            receipt.insert(
                "message".into(),
                json!(format!(
                    "The run is still going and continues: a waited run returns after at most {WAIT_SECONDS} seconds. \
                     Call list_notebooks (`running` lists the cells running) or read_cell to see when it ends. Don't run the cells in `execution.still_running` again."
                )),
            );
        }
        Ok((receipt, nb))
    }

    /// The text form of a cell's output when Pluto shows it as something else
    /// (HTML, a table, a tree), rendered by the runtime; none for text, errors
    /// and empty outputs.
    fn output_text(&self, nb: &Snapshot, id: &str) -> Result<Option<String>, String> {
        let cell = &nb.cells[id];
        if cell.errored || cell.output.is_empty() {
            return Ok(None);
        }
        let rendered = self.nbs.call("render_text", json!({ "notebook_id": nb.id, "cell_id": id }))?;
        Ok(rendered["text"].as_str().map(str::to_owned))
    }

    /// A cell as `read_cell` shows it.
    fn cell_json(&self, nb: &Snapshot, id: &str) -> Map<String, Value> {
        let cell = &nb.cells[id];
        let stale = cell.stale || self.pending_run(nb).iter().any(|p| p == id);
        let mut out = json!({
            "cell_id": id, "code": cell.code, "output": cell.output, "errored": cell.errored,
            "running": cell.running, "queued": cell.queued, "code_folded": cell.folded, "stale": stale,
        });
        if let Some(error) = &cell.error {
            out["error"] = error.clone();
        }
        if cell.not_run {
            out["not_run"] = json!(true);
        }
        let Value::Object(out) = out else { unreachable!() };
        out
    }

    fn list_notebooks(&self) -> Result<Value, String> {
        let snapshots = self.nbs.snapshots()?;
        let bound = self.nbs.bound(self.owner);
        let paths: Vec<String> = snapshots.iter().map(|nb| canonical_path(&nb.path).unwrap_or_else(|_| nb.path.clone())).collect();
        let own = |at: usize| bound.as_ref().is_some_and(|bound| paths[at] == *bound);
        if let Some(bound) = bound.as_ref().filter(|bound| !paths.contains(bound)) {
            self.nbs.unbind(self.owner, bound);
        }
        let mut state = self.nbs.state.lock().unwrap();
        Ok(Value::Array(snapshots.iter().enumerate().map(|(at, nb)| nb.summary(&state.notebooks.entry(nb.id.clone()).or_default().pending_run(nb), own(at))).collect()))
    }

    fn read_cell(&self) -> Result<Value, String> {
        let mut nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        // A cell waiting on a package step is waited for, within the call's
        // `WAIT_SECONDS`: an agent that reads it at once sees no change and
        // gives up long before a first install ends.
        let deadline = Instant::now() + self.wait_left().saturating_sub(Duration::from_secs(1));
        while nb.installing() && nb.cells.get(&cell).is_some_and(|c| c.queued) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            // Nobody will see this read, so it doesn't count as one.
            if (self.gone)() {
                return Err(argument_error("client_gone::The caller hung up during the wait"));
            }
            nb = self.nbs.snapshot(&nb.id)?;
        }
        let Some(code) = nb.cells.get(&cell).map(|c| c.code.clone()) else { return Err(key_error(&format!("cell_not_found::No cell with id '{cell}' in notebook"))) };
        self.record_read(&nb.id, &cell, &code);
        let mut out = self.cell_json(&nb, &cell);
        if let Some(text) = self.output_text(&nb, &cell)? {
            out.insert("output_text".into(), json!(text));
        }
        if nb.installing() {
            out.insert("packages".into(), nb.packages.clone().unwrap_or_default());
            out.insert("message".into(), json!(installing_message(&nb)));
        }
        Ok(Value::Object(out))
    }

    fn view_cell_output(&self) -> Result<Reply, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        if nb.cells[&cell].errored {
            return Err(argument_error(&format!("no_image::Cell {cell} errored; read_cell shows the error")));
        }
        let rendered = self.nbs.call("render_png", json!({ "notebook_id": nb.id, "cell_id": cell }))?;
        let mime = rendered["mime"].as_str().unwrap_or_default();
        let Some(png) = rendered["png"].as_str() else {
            return Err(argument_error(&format!("no_image::Cell {cell}: its output ({mime}) has no PNG rendering; read_cell shows it as text")));
        };
        let bytes = base64_len(png);
        if bytes > MAX_IMAGE_BYTES {
            return Err(argument_error(&format!("image_too_large::Cell {cell} renders to {bytes} bytes (max {MAX_IMAGE_BYTES})")));
        }
        Ok(Reply::Image { meta: json!({ "cell_id": cell, "shown_as": mime, "png_bytes": bytes }), png_base64: png.to_owned() })
    }

    fn edit_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let code = self.arg("code")?;
        self.require_fresh_read(&nb, &cell)?;
        let run_after = bool_arg(self.args, "run_after", false)?;
        let before = nb.cells[&cell].code.clone();
        let applied = self.apply(&nb.id, vec![json!({ "op": "set_code", "cell_id": cell, "code": code, "expected": before })])?;
        let code = code.as_str().unwrap_or_default();
        self.edited(&nb.id, &applied, &cell, &before, code);
        let ran = self.run_or_stage(&nb, &cell, run_after)?;
        let (mut receipt, after) = self.receipt_of_run(&nb.id, json!({ "type": "edit_cell", "cell_id": cell }), ran, None)?;
        receipt.extend(self.cell_json(&after, &cell));
        Ok(Value::Object(receipt))
    }

    /// What the core keeps of an edit the tools made (`applied`, the
    /// engine's reply): who changed the cell, that the owner knows its new
    /// code, and the code it replaced.
    fn edited(&self, id: &str, applied: &Value, cell: &str, before: &str, code: &str) {
        self.note_changed(id, cell);
        self.record_read(id, cell, code);
        let now = self.now();
        self.nbs.with_state(id, |state| state.tool_edits.insert(cell.to_owned(), now));
        self.nbs.with_state(id, |state| state.agent_edited(cell, before, code, applied["seq"].as_u64()));
    }

    fn edit_cells(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let edits = match self.arg("cells")? {
            Value::Array(edits) => edits.clone(),
            Value::Object(edits) if edits.is_empty() => Vec::new(),
            _ => return Err(argument_error("invalid_argument::cells must be a list of {cell_id, code}")),
        };
        let mut cells = Vec::new();
        for edit in &edits {
            if !edit.is_object() {
                return Err(argument_error("invalid_argument::cells must be a list of {cell_id, code}"));
            }
            let cell = cell_named(&nb, edit.get("cell_id").ok_or_else(|| key_error("cell_id"))?)?;
            self.require_fresh_read(&nb, &cell)?;
            cells.push(cell);
        }
        let mut ops = Vec::new();
        for (edit, cell) in edits.iter().zip(&cells) {
            let code = edit.get("code").ok_or_else(|| key_error("code"))?;
            ops.push(json!({ "op": "set_code", "cell_id": cell, "code": code, "expected": nb.cells[cell].code }));
        }
        let applied = self.apply(&nb.id, ops)?;
        for (edit, cell) in edits.iter().zip(&cells) {
            self.edited(&nb.id, &applied, cell, &nb.cells[cell].code, edit["code"].as_str().unwrap_or_default());
        }
        self.mark_pending(&nb.id, &cells);
        let mutation = json!({ "type": "edit_cells", "cell_ids": cells });
        Ok(Value::Object(self.receipt(&nb.id, mutation, &[], Vec::new(), None)?.0))
    }

    fn add_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let code = self.args.get("code").cloned().unwrap_or_else(|| json!(""));
        let after = self.args.get("after_cell_id").filter(|a| !a.is_null());
        let run_after = self.args.get("run_after").cloned().unwrap_or(json!(false));
        let folded = match self.args.get("folded").unwrap_or(&json!(false)) {
            Value::Bool(folded) => *folded,
            _ => return Err(argument_error("invalid_argument::folded must be a boolean")),
        };
        let unplaced = after.is_none_or(|a| *a == "");
        if !nb.order.is_empty() && unplaced {
            return Err(argument_error("placement_required::after_cell_id is required when the notebook is not empty"));
        }
        if !nb.order.is_empty() {
            let anchor = cell_named(&nb, after.unwrap())?;
            self.require_fresh_read(&nb, &anchor)?;
        }
        let index = match after.filter(|_| !unplaced) {
            None => nb.order.len(),
            Some(after) => {
                let target = uuid_value(after).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{}'", julia_string(after))))?;
                let at = nb.order.iter().position(|c| *c == target).ok_or_else(|| key_error(&format!("cell_not_found::Cell '{target}' not found in notebook")))?;
                at + 1
            }
        };
        let Value::Bool(run_after) = run_after else { return Err(argument_error("invalid_argument::run_after must be a boolean")) };
        let applied = self.apply(&nb.id, vec![json!({ "op": "insert", "code": code, "folded": folded, "index": index })])?;
        let cell = applied["inserted"][0].as_str().ok_or("the engine didn't say which cell it added")?.to_owned();
        let added = self.nbs.snapshot(&nb.id)?;
        let code = added.cells.get(&cell).map_or(String::new(), |c| c.code.clone());
        self.edited(&nb.id, &applied, &cell, "", &code);
        let ran = self.run_or_stage(&added, &cell, run_after)?;
        let (mut receipt, after) = self.receipt_of_run(&nb.id, json!({ "type": "add_cell", "cell_id": cell }), ran, None)?;
        receipt.extend(self.cell_json(&after, &cell));
        Ok(Value::Object(receipt))
    }

    fn delete_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        self.apply(&nb.id, vec![json!({ "op": "delete", "cell_id": cell })])?;
        self.nbs.with_state(&nb.id, |state| {
            state.pending.remove(&cell);
            state.reads.retain(|(_, read), _| *read != cell);
        });
        self.note_changed(&nb.id, &cell);
        // Pluto's reactive cleanup of what the cell defined, not waited for.
        self.nbs.call("run", json!({ "notebook_id": nb.id, "cells": [], "wait": false, "timeout": TIMEOUT_SECONDS }))?;
        let warnings = vec!["async_execution::cell deletion cleanup queued".to_owned()];
        Ok(Value::Object(self.receipt(&nb.id, json!({ "type": "delete_cell", "cell_id": cell }), &[], warnings, Some("completed"))?.0))
    }

    fn execute_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let cells = [cell.clone()];
        self.require_no_run_conflict(&nb, &cells)?;
        let wait = bool_arg(self.args, "wait_for_completion", false)?;
        let mutation = json!({ "type": "execute_cell", "cell_id": cell });
        if self.ran_after_user(&nb, &cells)? {
            return Ok(Value::Object(self.already_ran(&nb.id, mutation, &cells)?));
        }
        let ran = self.run(&nb, &cells, wait)?;
        Ok(Value::Object(self.receipt_of_run(&nb.id, mutation, ran, None)?.0))
    }

    fn submit_changes(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let wait = self.args.get("wait_for_completion").cloned().unwrap_or(json!(false));
        self.nbs.with_state(&nb.id, |state| state.prune(&nb));
        let mutation = json!({ "type": "submit_changes" });
        let targets = match self.args.get("cell_ids") {
            Some(ids) => {
                let ids = julia_iterate(ids)?
                    .iter()
                    .map(|c| uuid_value(c).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{}'", julia_string(c)))))
                    .collect::<Result<Vec<_>, _>>()?;
                let force = match self.args.get("force").unwrap_or(&json!(false)) {
                    Value::Bool(force) => *force,
                    other => return Err(no_method_not(other)),
                };
                if self.ran_after_user(&nb, &ids)? {
                    return Ok(Value::Object(self.already_ran(&nb.id, mutation, &ids)?));
                }
                if !force {
                    let pending = self.pending_run(&nb);
                    if let Some(id) = ids.iter().find(|id| !pending.contains(id)) {
                        return Err(argument_error(&format!("not_staged::Cell {id} is not in pending_run; stage first or pass force=true")));
                    }
                }
                ids
            }
            None => self.pending_run(&nb),
        };
        if self.args.get("cell_ids").is_none() {
            let ran = if targets.is_empty() { self.ran_since_tool_edit(&nb) } else { targets.clone() };
            if !ran.is_empty() && self.ran_after_user(&nb, &ran)? {
                return Ok(Value::Object(self.already_ran(&nb.id, mutation, &ran)?));
            }
        }
        if targets.is_empty() {
            return Ok(Value::Object(self.receipt(&nb.id, mutation, &[], Vec::new(), Some("completed"))?.0));
        }
        if let Some(missing) = targets.iter().find(|id| !nb.cells.contains_key(*id)) {
            return Err(key_error(&format!("cell_not_found::No cell with id '{missing}' in notebook")));
        }
        self.require_no_run_conflict(&nb, &targets)?;
        let Value::Bool(wait) = wait else { return Err(argument_error("invalid_argument::wait_for_completion must be a boolean")) };
        let ran = self.run(&nb, &targets, wait)?;
        Ok(Value::Object(self.receipt_of_run(&nb.id, mutation, ran, None)?.0))
    }

    fn run_all_cells(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let wait = self.args.get("wait_for_completion").cloned().unwrap_or(json!(false));
        self.require_no_run_conflict(&nb, &nb.order)?;
        let Value::Bool(wait) = wait else { return Err(argument_error("invalid_argument::wait_for_completion must be a boolean")) };
        let ran = Ran { cells: nb.order.clone(), ..self.run(&nb, &nb.order, wait)? };
        Ok(Value::Object(self.receipt_of_run(&nb.id, json!({ "type": "run_all_cells" }), ran, None)?.0))
    }

    fn fold_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let Value::Bool(folded) = self.arg("folded")? else { return Err(argument_error("invalid_argument::folded must be a boolean")) };
        self.apply(&nb.id, vec![json!({ "op": "fold", "cell_id": cell, "folded": folded })])?;
        let mutation = json!({ "type": "fold_cell", "cell_id": cell, "folded": folded });
        Ok(Value::Object(self.receipt(&nb.id, mutation, &[], Vec::new(), Some("completed"))?.0))
    }

    fn move_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let after = self.arg("after_cell_id")?;
        let old_index = nb.order.iter().position(|c| *c == cell).unwrap_or_default();
        let rest: Vec<&String> = nb.order.iter().filter(|c| **c != cell).collect();
        let index = if *after == "" {
            0
        } else {
            let shown = julia_string(after);
            let target = uuid_value(after).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{shown}'")))?;
            rest.iter().position(|c| **c == target).ok_or_else(|| key_error(&format!("cell_not_found::Target cell '{shown}' not found")))? + 1
        };
        self.apply(&nb.id, vec![json!({ "op": "move", "cell_id": cell, "index": index })])?;
        let mutation = json!({ "type": "move_cell", "cell_id": cell, "old_index": old_index + 1, "new_index": index + 1 });
        Ok(Value::Object(self.receipt(&nb.id, mutation, &[], Vec::new(), Some("completed"))?.0))
    }

    fn read_notebook_code(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let order = self.args.get("order").cloned().unwrap_or(json!("execution"));
        let markdown = self.args.get("include_markdown").cloned().unwrap_or(json!(false));
        if !order.is_string() {
            return Err(format!("TypeError: in keyword argument order, expected AbstractString, got a value of type {}", julia_type(&order)));
        }
        let Value::Bool(markdown) = markdown else { return Err(non_boolean(&markdown)) };
        let cells: Vec<String> = match order.as_str() {
            Some("visual") => nb.order.clone(),
            Some("execution") => self.nbs.graph(&nb.id, GraphQuery::default())?.order,
            _ => return Err(argument_error(&format!("invalid_order::order must be 'execution' or 'visual', got '{}'", julia_string(&order)))),
        };
        let mut ids = Vec::new();
        let mut blocks = Vec::new();
        let mut left_out = 0;
        for id in cells {
            let Some(cell) = nb.cells.get(&id) else { continue };
            if cell.hidden {
                continue;
            }
            if cell.markdown && !markdown {
                left_out += 1;
                continue;
            }
            let body = if cell.markdown {
                format!("# md:\n{}", cell.code)
            } else if julia_strip(&cell.code).is_empty() {
                "# (empty)".to_owned()
            } else {
                cell.code.clone()
            };
            blocks.push(format!("{CELL_MARKER}{id}\n{body}"));
            ids.push(id);
        }
        for id in &ids {
            self.record_read(&nb.id, id, &nb.cells[id].code);
        }
        let pending = self.pending_run(&nb);
        let stale: Vec<&String> = nb.order.iter().filter(|id| pending.contains(id) || nb.cells[*id].stale).collect();
        let mut result = json!({
            "notebook_id": nb.id,
            "path": nb.path,
            "order": order,
            "cell_ids": ids,
            "stale_cell_ids": stale,
            "pending_run": pending,
            "code": blocks.join("\n\n"),
        });
        if left_out > 0 {
            result["markdown_cells_left_out"] = left_out.into();
        }
        Ok(result)
    }

    fn get_cell_order(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        Ok(json!({ "notebook_id": nb.id, "cell_ids": nb.order }))
    }

    fn get_execution_order(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let graph = self.nbs.graph(&nb.id, GraphQuery::default())?;
        Ok(json!({ "notebook_id": nb.id, "cell_ids": graph.order }))
    }

    fn get_cell_dependencies(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let graph = self.nbs.graph(&nb.id, GraphQuery { refresh: true, edges: true, ..Default::default() })?;
        let upstream = graph.upstream_of(std::slice::from_ref(&cell));
        let upstream: Vec<&String> = graph.cells.iter().map(|n| &n.id).filter(|id| **id != cell && upstream.contains(*id)).collect();
        let symbols = graph.node(&cell).map(|n| n.references.clone()).unwrap_or_default();
        Ok(json!({ "upstream": upstream, "symbols": symbols }))
    }

    fn get_cell_dependents(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let graph = self.nbs.graph(&nb.id, GraphQuery { refresh: true, edges: true, ..Default::default() })?;
        let downstream = graph.downstream_of(std::slice::from_ref(&cell));
        let downstream: Vec<&String> = graph.cells.iter().map(|n| &n.id).filter(|id| **id != cell && downstream.contains(*id)).collect();
        Ok(json!({ "downstream": downstream }))
    }

    /// `find_symbol_definitions` or `find_symbol_references`: the cells whose
    /// `field` has the symbol, each with the first line mentioning it.
    fn find_symbol(&self, field: &str) -> Result<Value, String> {
        let nb = self.notebook()?;
        let symbol = julia_string(self.arg("symbol")?);
        let graph = self.nbs.graph(&nb.id, GraphQuery { refresh: true, ..Default::default() })?;
        let found: Vec<Value> = graph
            .cells
            .iter()
            .filter(|n| if field == "definitions" { &n.definitions } else { &n.references }.contains(&symbol))
            .map(|n| {
                let code = nb.cells.get(&n.id).map_or("", |c| c.code.as_str());
                let hint = code.split('\n').position(|line| line.contains(symbol.as_str())).map(|i| i + 1);
                json!({ "cell_id": n.id, "line_hint": hint })
            })
            .collect();
        Ok(Value::Array(found))
    }

    fn validate_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let code = self.arg("code")?;
        let reply = self.nbs.call("validate", json!({ "notebook_id": nb.id, "cell_id": cell, "code": code }))?;
        let errors = reply["errors"].clone();
        Ok(json!({ "valid": errors.as_array().is_some_and(Vec::is_empty), "errors": errors }))
    }

    fn search_code(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let Value::String(query) = self.arg("query")? else { return Err(argument_error("invalid_argument::query must be a string")) };
        let mut found = Vec::new();
        for id in &nb.order {
            if let Some(snippet) = snippet_around(&nb.cells[id].code, query)? {
                found.push(json!({ "cell_id": id, "snippet": snippet }));
            }
        }
        Ok(Value::Array(found))
    }

    fn open_notebook(&self, folder: &Folder) -> Result<Value, String> {
        let path = match self.args.get("path") {
            None | Some(Value::Null) => return Err(argument_error("invalid_path::path is required")),
            Some(Value::String(path)) => path,
            Some(_) => return Err(argument_error("invalid_path::path must be a string")),
        };
        let path = &super::requested_path(path, folder)?;
        if !std::path::Path::new(path).exists() {
            return Err(argument_error(&format!("file_not_found::No file at '{path}'")));
        }
        if wire::backend::Backend::of_file(std::path::Path::new(path)) == Some(wire::backend::Backend::Ember) {
            r_notebooks(&format!("'{path}' is an Ember notebook (R)"))?;
        }
        let run = self.args.get("run_notebook").cloned().unwrap_or(json!(false));
        let Value::Bool(run) = run else { return Err(non_boolean(&run)) };
        let opened = match self.nbs.call("open", json!({ "path": path, "run": run })) {
            Ok(opened) => opened,
            Err(error) if already_open(&error) => return self.join(path).unwrap_or(Err(error)),
            Err(error) => return Err(error),
        };
        let mut result = json!({
            "notebook_id": opened["notebook_id"], "path": opened["path"], "execution_allowed": run, "ran": run,
            "process_status": opened["process_status"],
        });
        let mut warnings = self.nbs.take_notices();
        if run {
            warnings.push("async_execution::open queued non-blocking notebook run; poll read_cell for completion".into());
        }
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        self.nbs.opened_by(self.owner, opened["path"].as_str().unwrap_or_default());
        Ok(result)
    }

    /// `open_notebook` on a notebook that is already open: the session
    /// works in it as it is. Nothing runs and its safe preview is as it was.
    /// None if no open notebook has this path after all.
    fn join(&self, path: &str) -> Option<Result<Value, String>> {
        let wanted = canonical_path(path).unwrap_or_else(|_| path.to_owned());
        // The list of open notebooks has ids and paths, where a snapshot of each would hold every cell.
        let open = match self.nbs.call("status", json!({})) {
            Ok(status) => status,
            Err(error) => return Some(Err(error)),
        };
        let found = open["notebooks"].as_array()?.iter().find(|nb| nb["path"].as_str().is_some_and(|open| canonical_path(open).unwrap_or_else(|_| open.to_owned()) == wanted))?;
        let nb = match self.nbs.snapshot(found["notebook_id"].as_str()?) {
            Ok(nb) => nb,
            Err(error) => return Some(Err(error)),
        };
        self.nbs.opened_by(self.owner, &nb.path);
        Some(Ok(json!({
            "notebook_id": nb.id, "path": nb.path, "execution_allowed": nb.execution_allowed, "ran": false,
            "process_status": nb.process_status, "already_open": true,
        })))
    }

    fn new_notebook(&self, folder: &Folder) -> Result<Value, String> {
        let params = match self.args.get("path").filter(|p| !p.is_null()) {
            None => {
                let mut params = match folder {
                    Folder::Unknown => return Err(super::folder_unknown()),
                    // The engine's own naming, like Pluto's "Create a new notebook", in the session's folder.
                    Folder::In(folder) if std::path::Path::new(folder).is_dir() => json!({ "folder": folder }),
                    _ => json!({}),
                };
                // In the session's kind, when its client said it.
                if let Some(kind @ wire::backend::Backend::Ember) = self.nbs.kind(self.owner) {
                    r_notebooks("This session's notebooks are R notebooks")?;
                    params["engine"] = json!(kind);
                }
                params
            }
            Some(Value::String(requested)) => {
                let path = absolute_path(&super::requested_path(requested, folder)?)?;
                match super::engines::of_path(&path) {
                    wire::backend::Backend::Ember => r_notebooks(&format!("'{path}' would be an R notebook"))?,
                    _ if !path.ends_with(".jl") => {
                        let ends = if cfg!(windows) { ".jl" } else { ".jl (Julia) or .R (R)" };
                        return Err(argument_error(&format!("invalid_path::Notebook path must end in {ends}: '{path}'")));
                    }
                    _ => {}
                }
                if std::path::Path::new(&path).exists() {
                    return Err(argument_error(&format!("file_exists::'{path}' already exists; use open_notebook to load it")));
                }
                let dir = super::parent_dir(&path);
                if !std::path::Path::new(&dir).is_dir() {
                    return Err(argument_error(&format!("invalid_path::Directory does not exist: '{dir}'")));
                }
                json!({ "path": path })
            }
            Some(_) => return Err(argument_error("invalid_path::path must be a string")),
        };
        let made = self.nbs.call("new", params)?;
        let id = made["notebook_id"].as_str().unwrap_or_default();
        let mut cell_ids = Vec::new();
        // The caller knows these cells are empty; without a read first, the first edit would be refused.
        for cell in made["cells"].as_array().into_iter().flatten() {
            let cell_id = cell["cell_id"].as_str().unwrap_or_default();
            self.record_read(id, cell_id, cell["code"].as_str().unwrap_or_default());
            cell_ids.push(cell_id);
        }
        self.nbs.opened_by(self.owner, made["path"].as_str().unwrap_or_default());
        let mut result = json!({
            "notebook_id": id, "path": made["path"], "execution_allowed": true, "ran": true,
            "process_status": made["process_status"], "cell_ids": cell_ids, "created": true,
        });
        let warnings = self.nbs.take_notices();
        if !warnings.is_empty() {
            result["warnings"] = json!(warnings);
        }
        Ok(result)
    }

    fn allow_execution(&self) -> Result<Value, String> {
        if self.args.get("notebook_id").is_none_or(Value::is_null) {
            return Err(argument_error("invalid_notebook_id::notebook_id is required"));
        }
        if !self.args["notebook_id"].is_string() {
            return Err(argument_error(&format!("invalid_notebook_id::Invalid notebook ID: '{}'", julia_string(&self.args["notebook_id"]))));
        }
        let nb = self.notebook()?;
        let run = self.args.get("run_notebook").cloned().unwrap_or(json!(true));
        let Value::Bool(run) = run else { return Err(non_boolean(&run)) };
        let allowed = self.nbs.call("allow_execution", json!({ "notebook_id": nb.id, "run": run, "timeout": TIMEOUT_SECONDS }))?;
        let ran = allowed["ran"] == true;
        let mut result = json!({
            "notebook_id": nb.id, "execution_allowed": true, "already_allowed": allowed["already_allowed"], "ran": ran,
            "process_status": allowed["process_status"],
        });
        if ran {
            self.nbs.with_state(&nb.id, |state| state.prune(&nb));
            result["run_warnings"] = json!(["async_execution::allow_execution queued non-blocking notebook run; poll read_cell for completion"]);
        }
        Ok(result)
    }
}

/// The cell a `cell_id` argument names, refused as Julia's lookup refused it.
fn cell_named(nb: &Snapshot, value: &Value) -> Result<String, String> {
    let shown = julia_string(value);
    let id = uuid_value(value).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{shown}'")))?;
    if !nb.cells.contains_key(&id) {
        return Err(key_error(&format!("cell_not_found::No cell with id '{shown}' in notebook")));
    }
    Ok(id)
}

/// Whether a run touched its cells, and how it went.
fn execution_status(nb: &Snapshot, cells_run: &[String], warnings: &[String]) -> String {
    let warned = |kind: &str| warnings.iter().any(|w| w.starts_with(kind));
    let status = if warned("execution_blocked::") {
        "blocked"
    } else if warned("async_execution::") {
        "running"
    } else if cells_run.is_empty() {
        "staged"
    } else {
        let cells: Vec<_> = cells_run.iter().filter_map(|id| nb.cells.get(id)).collect();
        if cells.iter().any(|c| c.errored) {
            "errored"
        } else if cells.iter().any(|c| c.running || c.queued) {
            "running"
        } else {
            "completed"
        }
    };
    status.to_owned()
}

/// What the agent hears while a notebook's cells wait on a package step, in
/// plain words: what is going on, that it is slow only the first time, and
/// how to wait; past `INSTALL_USUAL_SECONDS`, to wait only while the log moves.
fn installing_message(nb: &Snapshot) -> String {
    let packages = nb.packages.as_ref().cloned().unwrap_or_default();
    let names: Vec<&str> = packages["packages"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let which = if names.is_empty() { "the packages it uses".to_owned() } else { names.join(", ") };
    let step = packages["step"].as_str().unwrap_or("installing");
    let seconds = packages["seconds"].as_u64().unwrap_or(0);
    let so_far = if seconds >= 90 { format!("about {} minutes", (seconds + 30) / 60) } else { format!("{seconds} seconds") };
    let last = packages["last_line"].as_str().map(|line| format!(" Last line of the package log: {line}")).unwrap_or_default();
    let language = super::engines::language(super::engines::of_path(&nb.path));
    let what = format!("{language} is getting {which} ready for this notebook ({step}, {so_far} so far). The cells are queued and run when that is done.");
    if seconds >= INSTALL_USUAL_SECONDS {
        format!(
            "{what} This is taking longer than usual. Tell the user, with the last line of the package log. \
             Call read_cell again only if that line has changed since your last read; if it hasn't, stop waiting and let the user decide.{last}"
        )
    } else {
        format!(
            "{what} This is normal the first time a package is used, and can take several minutes; later notebooks reuse it. \
             Tell the user it is installing, then wait: call read_cell on a cell that is queued, and while packages install each call waits up to {WAIT_SECONDS} seconds.{last}"
        )
    }
}

/// What Claude hears when a notebook's own Julia ends during a run it waits
/// for, in the words of the app's crash page.
fn exited_message(cell: Option<&str>) -> String {
    let stopped = match cell {
        Some(cell) => format!("Julia stopped unexpectedly while running `{cell}`."),
        None => "Julia stopped unexpectedly.".to_owned(),
    };
    format!("{stopped} The notebook file is saved; its outputs are gone until the cells run again.")
}

/// A flag argument: a Bool, or refused.
fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, String> {
    match args.get(name) {
        None => Ok(default),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(argument_error(&format!("invalid_argument::{name} must be a boolean"))),
    }
}

/// The items Julia's `for x in value` goes through: an array's items, a
/// string's characters (each a number to `UUID`), a number itself.
fn julia_iterate(value: &Value) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(items) => Ok(items.clone()),
        Value::String(text) => Ok(text.chars().map(|c| json!(c as u32)).collect()),
        Value::Number(_) | Value::Bool(_) => Ok(vec![value.clone()]),
        _ => Err(argument_error("invalid_argument::cell_ids must be a list of cell IDs")),
    }
}

/// Whether the adapter refused to open a path because it is already open:
/// its error's kind, as for every adapter error (`ArgumentError: kind::message`).
pub(super) fn already_open(error: &str) -> bool {
    error.strip_prefix("ArgumentError: ").is_some_and(|rest| rest.starts_with("notebook_already_open::"))
}

/// Whether R notebooks can open here: not on Windows, where Ember doesn't run yet.
fn r_notebooks(what: &str) -> Result<(), String> {
    if cfg!(windows) {
        return Err(argument_error(&format!("unsupported::{what}. R notebooks don't run on Windows yet; they need macOS or Linux")));
    }
    Ok(())
}

pub fn argument_error(message: &str) -> String {
    format!("ArgumentError: {message}")
}

/// Julia's `KeyError` for a missing key, as it shows it.
pub fn key_error(key: &str) -> String {
    format!("KeyError: key {} not found", julia_repr(key))
}

/// No notebook with `shown`'s id: the error `notebook()` and `notebook_arg`
/// both raise. `mcp::tool_error` unwraps this back to `notebook_not_found`
/// with this message.
pub fn notebook_not_found(shown: &str) -> String {
    key_error(&format!("notebook_not_found::No notebook with id '{shown}' in the current session. Run list_notebooks to see what's open."))
}

/// Julia's error for `UUID(s)` on a string that isn't one.
fn malformed_uuid(value: &Value) -> String {
    match value {
        Value::String(text) => argument_error(&format!("Malformed UUID string: {}", julia_repr(text))),
        other => argument_error(&format!("invalid_cell_id::Invalid cell ID: '{}'", julia_string(other))),
    }
}

/// The name of the type Julia parses a JSON value into.
fn julia_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "Nothing",
        Value::Bool(_) => "Bool",
        Value::Number(n) if n.is_f64() => "Float64",
        Value::Number(_) => "Int64",
        Value::String(_) => "String",
        Value::Array(_) => "Vector{Any}",
        Value::Object(_) => "JSON.Object{String, Any}",
    }
}

/// Julia's error for a value that isn't a Bool where one must be.
fn non_boolean(value: &Value) -> String {
    format!("TypeError: non-boolean ({}) used in boolean context", julia_type(value))
}

/// Julia's error for `!value` on anything but a Bool.
fn no_method_not(value: &Value) -> String {
    format!(
        "MethodError: no method matching !(::{})\nThe function `!` exists, but no method is defined for this combination of argument types.\n\n\
         Closest candidates are:\n  !(!Matched::Missing)\n   @ Base missing.jl:101\n  !(!Matched::Bool)\n   @ Base bool.jl:37\n  \
         !(!Matched::ComposedFunction{{typeof(!)}})\n   @ Base operators.jl:1154\n  ...\n",
        julia_type(value)
    )
}

/// Julia's `isspace`.
fn julia_space(c: char) -> bool {
    matches!(c, ' ' | '\t'..='\r' | '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn julia_strip(text: &str) -> &str {
    text.trim_matches(julia_space)
}

/// The code around the first match of `query`, 40 bytes each side, as Julia
/// cut it: by byte index, failing as Julia fails when an end lands inside a character.
pub(super) fn snippet_around(code: &str, query: &str) -> Result<Option<String>, String> {
    let Some(at) = code.find(query) else { return Ok(None) };
    // Julia's match range: its first byte, and the first byte of its last character (1-based).
    let first = at + 1;
    let last = at + query.len() + 1 - query.chars().next_back().map_or(1, char::len_utf8);
    let start = first.saturating_sub(40).max(1);
    let stop = code.len().min(last + 40);
    if start > stop {
        return Ok(None);
    }
    for index in [start, stop] {
        if !code.is_char_boundary(index - 1) {
            return Err(string_index_error(code, index));
        }
    }
    let end = stop - 1 + code[stop - 1..].chars().next().map_or(1, char::len_utf8);
    let snippet = julia_strip(&code[start - 1..end]);
    Ok((!snippet.is_empty()).then(|| snippet.to_owned()))
}

/// Julia's `StringIndexError` for byte `index` (1-based) of `text`.
fn string_index_error(text: &str, index: usize) -> String {
    let prev = (0..index).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let char_at = |i: usize| text[i..].chars().next().map(|c| c.escape_debug().to_string().replace("\\'", "'")).unwrap_or_default();
    let next = prev + text[prev..].chars().next().map_or(1, char::len_utf8);
    if next < text.len() {
        format!("StringIndexError: invalid index [{index}], valid nearby indices [{}]=>'{}', [{}]=>'{}'", prev + 1, char_at(prev), next + 1, char_at(next))
    } else {
        format!("StringIndexError: invalid index [{index}], valid nearby index [{}]=>'{}'", prev + 1, char_at(prev))
    }
}

/// The length of what a base64 text decodes to.
/// `text` cut to at most `max` bytes at a character boundary, saying so.
pub(super) fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let end = (0..=max).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    format!("{}\n… (cut; read_cell shows more)", &text[..end])
}

fn base64_len(text: &str) -> usize {
    let padding = text.bytes().rev().take_while(|b| *b == b'=').count();
    text.len() / 4 * 3 - padding
}

#[cfg(test)]
pub(super) fn tool_json(reply: Reply) -> Value {
    match reply {
        Reply::Json(value) => value,
        Reply::Image { meta, .. } => meta,
    }
}

//! The notebook tools: every rule the agent's tools follow, applied here and
//! carried out through the engine's adapter. Results and errors are what the
//! Julia runtime gave before, to the byte, but for the order of lists Julia
//! kept in hash tables (now notebook order) and errors for argument types
//! Julia reported with its own stack of candidate methods.
//!
//! An error is the text of the exception Julia raised: `ArgumentError: kind::message`
//! names its kind (see `mcp::tool_error`).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{Cell, Change, Folder, GraphQuery, Notebooks, Snapshot, Started, absolute_path, canonical_path, cell_id_in, uuid_value};
use crate::host_tools::julia_repr;
use crate::mcp::julia_string;
use wire::backend::Backend;

/// How long the app's report of a user's run counts. (Also sent as the
/// `timeout` of runs the engine is not asked to wait for, which it ignores.)
pub(super) const TIMEOUT_SECONDS: f64 = 60.0;
/// The most a tool call waits for the user's answer and then a run, from the
/// start of the call: under the 60 seconds that Claude Code (by default, and
/// whatever progress it hears) and Codex give a tool call. The wait for the
/// user's own run of the same cells (`ran_after_user`) comes out of it.
pub const WAIT_SECONDS: f64 = 45.0;
/// The least a run is waited for, however much of `WAIT_SECONDS` went before.
pub const WAIT_FLOOR_SECONDS: f64 = 5.0;
/// The most `interrupt_notebook` waits for the cells to stop. One still running then is
/// usually in compiled code that ignores interrupts; Ember offers its restart 3 s after a Stop.
const INTERRUPT_WAIT_SECONDS: f64 = 10.0;
/// Claude accepts images up to about 5 MB; plots are typically tens of KB.
const MAX_IMAGE_BYTES: usize = 4_000_000;
/// How long a package step can take before the agent is told it is taking
/// longer than usual and to stop waiting unless the package log moves.
const INSTALL_USUAL_SECONDS: u64 = 600;
/// How `read_notebook_code` starts each cell of a Pluto notebook, as Pluto's file does.
const CELL_MARKER: &str = "# ╔═╡ ";
/// And of an Ember notebook, as an Ember `.R` file does.
const EMBER_CELL_MARKER: &str = "# %% id=";
/// What `open_notebook` says about a notebook in safe preview, so the agent passes it on.
pub(super) const NOT_RUN: &str = "Safe preview: none of this notebook's code has run, so its cells have no outputs yet. Tell the user that when you describe it. It runs only once the user allows it.";
/// How much of each output's text a change's receipt carries; `read_cell` has
/// the rest.
const RECEIPT_TEXT_MAX: usize = 2_000;
/// The same for a cell that ran along with the ones a run asked for (`Ran::along`).
const DEPENDENT_TEXT_MAX: usize = 200;
/// The notes a run's reply leaves out for a cell that only ran along, and gives after the
/// others; `read_cell` has them.
const QUIET_NOTES: [&str; 2] = ["printed", "logged_info"];
/// How many notes a run's reply carries, and how many bytes of them: a run of every cell
/// (`run_all_cells`) would otherwise send back what each one printed.
const RUN_NOTES_MAX: usize = 10;
const RUN_NOTES_BYTES: usize = 4_000;
/// How many of a session's calls a read or write of a cell counts as recent: within them, `read_cell`
/// leaves out code it hasn't changed (`code_unchanged`), as the agent still has it.
const RECENT_CALLS: u64 = 10;

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
        *self.state.lock().unwrap().calls.entry(owner.to_owned()).or_default() += 1;
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
            "interrupt_notebook" => t.interrupt_notebook(),
            "answer_prompt" => t.answer_prompt(),
            "restart_notebook" => t.restart_notebook(),
            "preview_package_update" => t.preview_package_update(),
            "approve_install" => t.approve_install(),
            "apply_package_update" => t.apply_package_update(),
            "set_input" => t.set_input(),
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
                Some(ids) => julia_iterate(ids)?.iter().map(|c| cell_id_in(&nb, c).ok_or_else(|| malformed_uuid(c))).collect::<Result<Vec<_>, _>>()?,
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
        Ok(self.upstream(nb, targets)?.into_iter().filter(|id| nb.cells.get(id).is_some_and(|c| c.last_run == 0.0 && !nb.is_running(c))).collect())
    }

    /// The cells `targets` depend on, directly or not, the targets left out,
    /// in notebook order. None while the notebook isn't running code.
    fn upstream(&self, nb: &Snapshot, targets: &[String]) -> Result<Vec<String>, String> {
        if targets.is_empty() || !nb.execution_allowed {
            return Ok(Vec::new());
        }
        let upstream = self.graph(&nb.id, GraphQuery { fresh: true, edges: true, ..Default::default() })?.upstream_of(targets);
        Ok(nb.order.iter().filter(|id| upstream.contains(*id) && !targets.contains(id)).cloned().collect())
    }
}

/// The owner of a change the user made through the app (`endeavor/restore_cells`): no session's.
pub const USER: &str = "(the user)";

fn other_owner(change: &Change, owner: &str) -> bool {
    !change.owner.is_empty() && change.owner != owner
}

/// What a run did, for its receipt.
#[derive(Default)]
struct Ran {
    warnings: Vec<String>,
    /// The cells it ran, a waited run's dependents that reran with them too.
    cells: Vec<String>,
    /// A waited run whose wait ended with it unfinished: its cells and their
    /// dependents, which may still be running. Empty otherwise.
    going: Vec<String>,
    /// A run not waited for: the dependents it will rerun after its cells (#192).
    will_run: Vec<String>,
    /// Of `cells`, those that ran along with the ones the call asked for, other than one edited since
    /// it last ran: the dependents that reran, and the stale cells they needed (`also`). Their output
    /// text is cut shorter.
    along: Vec<String>,
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
        self.record_read_of(id, cell, code, true);
    }

    /// As `record_read`; `sent` is false when the reply left the code out. That read still counts
    /// before an edit, but not as the agent having been given the code: else reads every few calls
    /// would keep code it lost to a compaction "unchanged" for good.
    fn record_read_of(&self, id: &str, cell: &str, code: &str, sent: bool) {
        let mut state = self.nbs.state.lock().unwrap();
        state.seq += 1;
        let seq = state.seq;
        let calls = state.calls.get(self.owner).copied().unwrap_or(0);
        let notebook = state.notebooks.entry(id.to_owned()).or_default();
        let key = (self.owner.to_owned(), cell.to_owned());
        let at = match notebook.reads.get(&key) {
            Some((read, _, at)) if !sent && read == code => *at,
            _ => calls,
        };
        notebook.reads.insert(key, (code.to_owned(), seq, at));
    }

    /// The code of `cell` that this session read or wrote within its last `RECENT_CALLS` calls, if any:
    /// what its agent can be taken to still have. An older read may have left the agent's context (a
    /// compaction, old tool results cleared), and an agent that is told it has code it lost edits
    /// from memory.
    fn recent_read(&self, id: &str, cell: &str) -> Option<String> {
        let state = self.nbs.state.lock().unwrap();
        let key = (self.owner.to_owned(), cell.to_owned());
        let now = state.calls.get(self.owner).copied().unwrap_or(0);
        let notebook = state.notebooks.get(id)?;
        let (code, _, at) = notebook.reads.get(&key)?;
        (now.saturating_sub(*at) <= RECENT_CALLS).then(|| code.clone())
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
            Some((code, _, _)) if *code != nb.cells[cell].code => Err(argument_error(&format!("stale_read::Cell {cell} changed since last read; call read_cell again"))),
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
            let read_seq = |cell: &String| notebook.reads.get(&(self.owner.to_owned(), cell.clone())).map_or(0, |(_, seq, _)| *seq);
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
            "run_conflict::{} changed since this session last read them (by the user, by another Endeavor session, or by this conversation before it was resumed), and the cells you're running depend on them. \
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
        // Edited and not yet run: a dependent of these that reran is code the agent hasn't seen run.
        let staged = self.pending_run(nb);
        let upstream = self.nbs.upstream(nb, targets)?;
        let needed: Vec<String> = upstream.iter().filter(|id| nb.cells.get(*id).is_some_and(|c| c.last_run == 0.0 && !nb.is_running(c))).cloned().collect();
        let cells: Vec<String> = needed.iter().chain(targets).cloned().collect();
        let timeout = if wait { self.run_wait().as_secs_f64() } else { TIMEOUT_SECONDS };
        let reply = self.nbs.call("run", json!({ "notebook_id": nb.id, "cells": cells, "wait": wait, "timeout": timeout }))?;
        self.ran(nb, targets, &upstream, needed, cells, &reply, wait, &staged)
    }

    /// What a run of `cells` did, from the engine's `reply` (`run`'s, or `set_input`'s, which reruns
    /// the cells that read an input): it records the run for the session and says what the receipt
    /// needs. `needed` are the cells `targets` needed that had never run, which the run ran first.
    /// `staged` are the cells edited and not yet run before the run started (`pending_run`).
    #[allow(clippy::too_many_arguments)]
    fn ran(&self, nb: &Snapshot, targets: &[String], upstream: &[String], needed: Vec<String>, mut cells: Vec<String>, reply: &Value, wait: bool, staged: &[String]) -> Result<Ran, String> {
        let ids = |key: &str| reply[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        if reply.get("exited").is_some() {
            let graph = self.nbs.graph(&nb.id, GraphQuery::default()).ok();
            let cell = ids("exited").first().map(|id| graph.and_then(|g| g.name(id)).unwrap_or_else(|| id.clone()));
            return Err(argument_error(&format!("process_exited::{}", exited_message(super::engines::language(super::engines::of_path(&nb.path)), cell.as_deref()))));
        }
        // The cells that depend on `cells`, which autorun reruns: those the graph links to them,
        // and those the engine says it reruns besides (`dependents`), less the ancestors it
        // runs first: Ember reruns the readers of a name a cell's last run defined, which the
        // graph no longer links once the cell drops the name (#228). The run is under way by
        // now, so a graph that can't be read leaves only the engine's.
        let dependents_of = |cells: &[String]| -> HashSet<String> {
            let mut out = self.nbs.graph(&nb.id, GraphQuery { edges: true, ..Default::default() }).map(|g| g.downstream_of(cells)).unwrap_or_default();
            out.extend(ids("dependents").into_iter().filter(|id| !upstream.contains(id)));
            out
        };
        let mut warnings = Vec::new();
        // Cells the targets need that the engine ran first, as Ember does one
        // edited since it last ran (a staged edit) or stale (an ancestor ran
        // since): this run's cells too, so the receipt shows them (#187).
        // Known once it is over, from what ran rather than from the engine's
        // plan, so it misses an ancestor still running when the wait ends (it
        // stays staged) and counts one the user ran during the wait.
        let mut also = Vec::new();
        // The dependents that reran with a waited run, as autorun reruns them: the
        // run's cells too, so its receipt shows their outputs and errors (#192).
        // Known the same way, from what ran, so it misses one still running when
        // the wait ends (that one is in `going`) and counts one the user ran during it.
        let mut reran = Vec::new();
        let mut dependents = HashSet::new();
        if reply["accepted"] == true && wait {
            let now = self.nbs.snapshot(&nb.id)?;
            also = upstream
                .iter()
                .filter(|id| !cells.contains(id))
                .filter(|id| match (nb.cells.get(*id), now.cells.get(*id)) {
                    (Some(before), Some(after)) => !(after.running || after.queued) && after.last_run > before.last_run,
                    _ => false,
                })
                .cloned()
                .collect();
            if !also.is_empty() {
                cells = nb.order.iter().filter(|id| also.contains(id) || needed.contains(id)).chain(targets).cloned().collect();
            }
            dependents = dependents_of(&cells);
            reran = nb
                .order
                .iter()
                .filter(|id| dependents.contains(*id) && !cells.contains(id))
                .filter(|id| match (nb.cells.get(*id), now.cells.get(*id)) {
                    (Some(before), Some(after)) => !(after.running || after.queued) && after.last_run > before.last_run,
                    _ => false,
                })
                .cloned()
                .collect();
        }
        let mut will_run = Vec::new();
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
                going = nb.order.iter().filter(|id| cells.contains(*id) || dependents.contains(*id)).cloned().collect();
            }
        } else {
            warnings.push("async_execution::cells running; read them for the result".into());
            // The dependents autorun reruns after these cells: those that have run
            // since the notebook started, as Ember skips one that hasn't (Pluto has
            // none). In lazy mode they go stale instead.
            if !nb.lazy {
                let dependents = dependents_of(&cells);
                will_run = nb.order.iter().filter(|id| dependents.contains(*id) && !cells.contains(id) && nb.cells.get(*id).is_some_and(|c| !c.not_run && !c.off && !c.hidden)).cloned().collect();
            }
        }
        // A run of these cells was accepted: they are no longer staged, whether or not they have finished.
        if reply["accepted"] == true {
            self.nbs.with_state(&nb.id, |state| {
                // Its cells that haven't finished: a waited run's that did finish are done, and
                // so are all of a run that ended before this records it.
                let finished: Vec<String> = ids("completed").into_iter().chain(also.iter().cloned()).collect();
                let run = reply["run"].as_str().map(str::to_owned);
                let ended = run.as_ref().is_some_and(|run| state.ended.contains(run));
                state.ended.retain(|r| Some(r) != run.as_ref());
                let unfinished: HashSet<String> = cells.iter().filter(|c| !finished.contains(c)).cloned().collect();
                // A run the engine named goes on until its `run_finished`, the cells that depend
                // on its cells too, even once its own cells have finished.
                if !self.owner.is_empty() && !ended && (run.is_some() || !unfinished.is_empty()) {
                    let follows = will_run.iter().filter_map(|id| Some((id.clone(), nb.cells.get(id)?.last_run))).collect();
                    state.started.push(Started { owner: self.owner.to_owned(), run, cells: unfinished, follows });
                }
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
        if !also.is_empty() {
            warnings.push(format!("also_ran::Also ran {}: cells this run needs that were edited or stale since they last ran.", also.join(", ")));
        }
        let along = also.iter().chain(&reran).filter(|id| !staged.contains(id)).cloned().collect();
        cells.extend(reran);
        Ok(Ran { warnings, cells, going, will_run, along })
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
        Ok(Ran { warnings: conflict.map(|c| vec![format!("{c} The edit is staged, not run.")]).unwrap_or_default(), ..Default::default() })
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
        self.receipt_of_run(id, mutation, Ran { warnings, cells: cells_run.to_vec(), ..Default::default() }, status)
    }

    /// `receipt` for a run.
    fn receipt_of_run(&self, id: &str, mutation: Value, ran: Ran, status: Option<&str>) -> Result<(Map<String, Value>, Snapshot), String> {
        let Ran { mut warnings, cells: cells_run, going, will_run, along } = ran;
        let cells_run = &cells_run[..];
        let nb = self.nbs.snapshot(id)?;
        // The cells of a run that ran out of its wait that are running now, read
        // after the wait: some may have finished meanwhile.
        let still_running: Vec<String> = nb.order.iter().filter(|id| going.contains(id) && nb.cells.get(*id).is_some_and(|c| c.running || c.queued)).cloned().collect();
        // A cell of this run waiting on a prompt ends the wait at once (#261). A run waiting on
        // the user's OK for a download didn't run out of time: the message says what it waits for.
        let asks = nb.prompted().filter(|id| still_running.contains(id) || cells_run.contains(id)).cloned();
        if let Some(cell) = &asks {
            warnings.push(format!("waiting_for_input::Cell {cell} is waiting for input; the run goes on once it is answered"));
        } else if !still_running.is_empty() && !nb.held() {
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
        // A cell still running or queued has the output of its last run, so it
        // isn't listed: a run not waited for comes back before its cells finish (#190).
        let ran: &[String] = if status == "blocked" { &[] } else { cells_run };
        let changed: Vec<Value> = ran
            .iter()
            .filter(|id| !still_running.contains(id))
            .filter_map(|id| Some((id, nb.cells.get(id)?)))
            .filter(|(_, cell)| !cell.output.is_empty() && !cell.running && !cell.queued)
            .map(|(id, cell)| {
                let mut entry = json!({ "cell_id": id, "output_summary": cell.output });
                if let Some(error) = &cell.error {
                    entry["error"] = error.clone();
                }
                // The output of a cell that only ran along is the agent's to read if it wants it: a run of
                // a cell many others read sent back each one's table, most of a long session's run
                // replies. An error isn't cut.
                let max = if along.contains(id) && cell.error.is_none() { DEPENDENT_TEXT_MAX } else { RECEIPT_TEXT_MAX };
                if let Some(text) = self.output_text(&nb, id)? {
                    entry["output_text"] = json!(cut(&text, max));
                }
                Ok(entry)
            })
            .collect::<Result<_, String>>()?;
        // The notes Ember shows under a cell this run ran, such as a value
        // another cell defines changed in place, whose readers don't rerun
        // (#207), and what a Pluto cell printed and logged, such as a `@warn`
        // that rows were dropped (#257): an agent that doesn't see them reads a
        // wrong result as right.
        let finished: Vec<(&String, &Cell)> = ran.iter().filter(|id| !still_running.contains(id)).filter_map(|id| Some((id, nb.cells.get(id).filter(|c| !c.running && !c.queued)?))).collect();
        warnings.extend(run_notes(&finished, &along));
        // Haiku 4.5 read a logged warning in `warnings` and reported the result as clean (N11).
        // The first one is quoted in the message: the agent passes on what it reads there.
        let logged: Vec<&str> = warnings.iter().filter(|w| w.starts_with("logged_warning::") || w.starts_with("logged_error::")).map(String::as_str).collect();
        let logged_message = logged_message(&logged);
        let pending = self.pending_run(&nb);
        // The notebook's order isn't in it: two lists of every cell's id grow
        // with the notebook and come back on every change, and an agent that
        // needs an order asks for it (`get_cell_order`, `get_execution_order`;
        // cells that can't run, in a cycle or defining a name twice, show
        // their error in `read_cell`).
        let receipt = json!({
            "applied": true,
            "mutation": mutation,
            "affected_cells": cells_run,
            "execution": { "status": status },
            "outputs": { "changed": changed },
            "pending_run": &pending,
            "warnings": warnings,
        });
        let Value::Object(mut receipt) = receipt else { unreachable!() };
        if !still_running.is_empty() {
            receipt["execution"]["still_running"] = json!(still_running);
        }
        if !will_run.is_empty() {
            receipt["execution"]["will_run"] = json!(will_run);
        }
        // In lazy mode the run's dependents don't run: they're marked stale,
        // which the agent wouldn't otherwise learn until it read them (#196).
        // The list is every stale cell an earlier run left too, not only this
        // run's dependents, leaving out the ones that are pending, running,
        // queued or hidden. A notebook with none says nothing.
        if nb.lazy && status != "blocked" {
            let stale_cells: Vec<&String> = nb
                .order
                .iter()
                .filter(|id| !cells_run.contains(id) && !pending.contains(id))
                .filter(|id| nb.cells.get(*id).is_some_and(|c| c.stale && !c.running && !c.queued && !c.hidden && !c.off))
                .collect();
            if !stale_cells.is_empty() {
                receipt["warnings"].as_array_mut().unwrap().push(json!(
                    "lazy_mode::This notebook is in lazy mode: the cells that depend on a run are marked stale instead of rerunning. `stale_cells` names the cells that are stale now; run the ones that should be current"
                ));
                receipt.insert("stale_cells".into(), json!(stale_cells));
            }
        }
        if let Some(cell) = &asks {
            receipt.insert("waiting_for_input".into(), waiting_json(&nb, cell));
            receipt.insert("message".into(), json!(prompt_message(&nb, cell)));
        } else if nb.installing() || nb.asking() {
            receipt.insert("packages".into(), nb.packages.clone().unwrap_or_default());
            let mut message = installing_message(&nb);
            if !still_running.is_empty() {
                message.push_str(" Don't run the cells in `execution.still_running` again: they run once the packages are ready.");
            }
            receipt.insert("message".into(), json!(message));
        } else if let Some(message) = logged_message.filter(|_| still_running.is_empty()) {
            receipt.insert("message".into(), json!(message));
        } else if !still_running.is_empty() {
            receipt.insert(
                "message".into(),
                json!(format!(
                    "The run is still going and continues: a waited run returns after at most {WAIT_SECONDS} seconds. \
                     Call read_cell on one of them: it waits up to {WAIT_SECONDS} seconds for the cell to finish. Don't run the cells in `execution.still_running` again."
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
        if !cell.notes.is_empty() {
            out["notes"] = json!(cell.notes);
        }
        if !cell.inputs.is_empty() {
            out["inputs"] = json!(cell.inputs);
        }
        if cell.running && cell.prompt.is_some() {
            out["waiting_for_input"] = waiting_json(nb, id);
        }
        let Value::Object(out) = out else { unreachable!() };
        out
    }

    /// The cell an edit changed, as `read_cell` shows it without its code:
    /// the agent sent that code, and gets it back from `read_cell`. While the
    /// edited cell is running or queued, its output and error are the ones from
    /// before the edit, so they are `previous_output`, `previous_errored` and
    /// `previous_error` (#190).
    fn changed_cell_json(&self, nb: &Snapshot, id: &str) -> Map<String, Value> {
        let mut out = self.cell_json(nb, id);
        out.remove("code");
        if out["running"] == true || out["queued"] == true {
            for key in ["output", "errored", "error", "notes"] {
                if let Some(previous) = out.remove(key) {
                    out.insert(format!("previous_{key}"), previous);
                }
            }
        }
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

    /// Wait, within the call's `WAIT_SECONDS`, for the runs this session started in `nb` to end:
    /// the cells seen running or that finished a run during the wait, with the ones still running
    /// or queued if a run is still going at its end. A queued cell that never ran (an interrupt
    /// dropped it) isn't one. A cell another run ran meanwhile counts too: the snapshots don't
    /// say whose run ran a cell.
    fn wait_for_own_run(&self, nb: &Snapshot) -> Result<(Vec<String>, Vec<String>), String> {
        let mut nb = self.nbs.snapshot(&nb.id)?;
        let before: HashMap<String, f64> = nb.cells.iter().map(|(id, c)| (id.clone(), c.last_run)).collect();
        let busy = |nb: &Snapshot, id: &String| nb.cells.get(id).is_some_and(|c| c.running || c.queued);
        let running = |nb: &Snapshot| nb.cells.iter().filter(|(_, c)| c.running).map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let mut ran: HashSet<String> = running(&nb).into_iter().collect();
        let deadline = Instant::now() + self.wait_left().saturating_sub(Duration::from_secs(1));
        let mut going = Vec::new();
        while self.nbs.own_run_going(self.owner, &nb) {
            // A prompt holds the run until it is answered, and an install waiting for the
            // user's OK doesn't go on by itself: waiting longer only spends the call.
            if Instant::now() >= deadline || nb.prompted().is_some() || nb.held() {
                going = nb.order.iter().filter(|id| busy(&nb, id)).cloned().collect();
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
            if (self.gone)() {
                return Err(argument_error("client_gone::The caller hung up during the wait"));
            }
            nb = self.nbs.snapshot(&nb.id)?;
            ran.extend(running(&nb));
        }
        ran.extend(nb.cells.iter().filter(|(id, c)| before.get(*id).is_some_and(|last| c.last_run != *last)).map(|(id, _)| id.clone()));
        // A cell still busy once the session's runs ended is another run's.
        ran.retain(|id| !busy(&nb, id) || going.contains(id));
        ran.extend(going.iter().cloned());
        Ok((nb.order.iter().filter(|id| ran.contains(*id)).cloned().collect(), going))
    }

    fn read_cell(&self) -> Result<Value, String> {
        let mut nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        // A cell waiting on a package step is waited for, within the call's
        // `WAIT_SECONDS`: an agent that reads it at once sees no change and
        // gives up long before a first install ends. So is a cell that a run
        // this session started is running or will run: an agent that reads it
        // at once reads it again, each read a whole request of the agent's
        // context. The user's own run, or another session's, isn't waited for,
        // nor a queued cell once the install waits for the user's OK with
        // nothing running (`held`): only approve_install moves it on. Cells that
        // don't need the held packages still run, and are waited for. (Asked
        // on every read, so that a run seen to end is forgotten.)
        let waits = |nb: &Snapshot| {
            let own = self.nbs.own_run_going(self.owner, nb);
            // A cell waiting on a prompt, or behind one, waits until it is answered: not waited for.
            nb.prompted().is_none() && (nb.cells.get(&cell).is_some_and(|c| (c.running && own) || (c.queued && !nb.held() && (nb.installing() || own))) || (own && !nb.held() && self.nbs.own_run_will_run(self.owner, nb, &cell)))
        };
        let waited = Instant::now();
        let deadline = waited + self.wait_left().saturating_sub(Duration::from_secs(1));
        while waits(&nb) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            // Nobody will see this read, so it doesn't count as one.
            if (self.gone)() {
                return Err(argument_error("client_gone::The caller hung up during the wait"));
            }
            nb = self.nbs.snapshot(&nb.id)?;
        }
        // The wait ran out with the cell still going. An agent can't tell how long a call took,
        // so it is told (#203); else it reads an answer like an immediate one, and reports that
        // the read didn't wait.
        let ran_out = (waits(&nb) && waited.elapsed() >= Duration::from_millis(500)).then(|| self.began.elapsed().as_secs());
        let Some(code) = nb.cells.get(&cell).map(|c| c.code.clone()) else { return Err(key_error(&format!("cell_not_found::No cell with id '{cell}' in notebook"))) };
        let include_code = bool_arg(self.args, "include_code", false)?;
        // Code this session read or wrote a few calls ago, unchanged since, isn't sent again: an agent
        // that reads each cell it just ran gets its own code back each time, a tenth of a long session.
        let left_out = !include_code && self.recent_read(&nb.id, &cell).is_some_and(|read| read == code);
        self.record_read_of(&nb.id, &cell, &code, !left_out);
        let mut out = self.cell_json(&nb, &cell);
        if left_out {
            out.remove("code");
            out.insert("code_unchanged".into(), json!(true));
            // Which code it means, so the app keeps its copy only if it is that one.
            out.insert("code_sha".into(), json!(code_sha(&code)));
        }
        if let Some(text) = self.output_text(&nb, &cell)? {
            out.insert("output_text".into(), json!(text));
        }
        let asks = nb.prompted().filter(|asks| **asks == cell || nb.cells.get(&cell).is_some_and(|c| c.running || c.queued)).cloned();
        if let Some(asks) = asks {
            // The cell asking, or one queued behind it.
            if asks != cell {
                out.insert("waiting_for_input".into(), waiting_json(&nb, &asks));
            }
            out.insert("message".into(), json!(prompt_message(&nb, &asks)));
        } else if nb.installing() || nb.asking() {
            out.insert("packages".into(), nb.packages.clone().unwrap_or_default());
            out.insert("message".into(), json!(installing_message(&nb)));
        } else if let Some(seconds) = ran_out {
            out.insert("message".into(), json!(format!(
                "This read waited {seconds} s, and the cell is still {}: the run goes on. Read it again to keep waiting, or call interrupt_notebook if the user wants it stopped.",
                if nb.cells.get(&cell).is_some_and(|c| c.running) { "running" } else { "queued" }
            )));
        } else if let Some(cell_now) = nb.cells.get(&cell).filter(|c| !c.running && !c.queued) {
            // The same message as a run's, for a read of a cell that logged a warning or an error.
            let logged: Vec<String> = cell_now.notes.iter().filter(|n| n.starts_with("logged_warning::") || n.starts_with("logged_error::")).map(|n| n.replacen("::", &format!("::Cell {cell}: "), 1)).collect();
            if let Some(message) = logged_message(&logged.iter().map(String::as_str).collect::<Vec<_>>()) {
                out.insert("message".into(), json!(message));
            }
        }
        Ok(Value::Object(out))
    }

    fn view_cell_output(&self) -> Result<Reply, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let shown = &nb.cells[&cell];
        if shown.errored {
            return Err(argument_error(&format!("no_image::Cell {cell} errored; read_cell shows the error")));
        }
        // A cell that hasn't run yet has no output to render, which isn't the same as a text output.
        if shown.output.is_empty() && (shown.running || shown.queued || shown.not_run) {
            let then = if shown.running || shown.queued { "read_cell says when it has finished" } else { "run it first (execute_cell)" };
            let why = if shown.running { "is running" } else if shown.queued { "is queued to run" } else { "hasn't run" };
            return Err(argument_error(&format!("no_image::Cell {cell} {why}, so it has no output yet; {then}")));
        }
        let rendered = self.nbs.call("render_png", json!({ "notebook_id": nb.id, "cell_id": cell }))?;
        let mime = rendered["mime"].as_str().unwrap_or_default();
        let Some(png) = rendered["png"].as_str() else {
            return Err(argument_error(&format!("no_image::Cell {cell}: its output ({mime}) {NO_PNG}; read_cell shows it as text")));
        };
        let bytes = base64_len(png);
        if bytes > MAX_IMAGE_BYTES {
            return Err(argument_error(&format!("image_too_large::Cell {cell} renders to {bytes} bytes (max {MAX_IMAGE_BYTES})")));
        }
        let mut meta = json!({ "cell_id": cell, "shown_as": mime, "png_bytes": bytes });
        // An image read from the output's HTML (a widget's static picture), not rendered from the value.
        if let Some(source) = rendered["source"].as_str() {
            meta["source"] = json!(source);
        }
        Ok(Reply::Image { meta, png_base64: png.to_owned() })
    }

    fn edit_cell(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let code = self.arg("code")?;
        self.require_fresh_read(&nb, &cell)?;
        let run_after = bool_arg(self.args, "run_after", false)?;
        let folded = optional_folded(self.args.get("folded"))?;
        let before = nb.cells[&cell].code.clone();
        let mut ops = vec![json!({ "op": "set_code", "cell_id": cell, "code": code, "expected": before })];
        ops.extend(folded.map(|folded| json!({ "op": "fold", "cell_id": cell, "folded": folded })));
        let applied = self.apply(&nb.id, ops)?;
        let code = code.as_str().unwrap_or_default();
        self.edited(&nb.id, &applied, &cell, &before, code);
        let ran = self.run_or_stage(&nb, &cell, run_after)?;
        let (mut receipt, after) = self.receipt_of_run(&nb.id, json!({ "type": "edit_cell", "cell_id": cell }), ran, None)?;
        receipt.extend(self.changed_cell_json(&after, &cell));
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
        let mut folds = Vec::new();
        for edit in &edits {
            if !edit.is_object() {
                return Err(argument_error("invalid_argument::cells must be a list of {cell_id, code}"));
            }
            let cell = cell_named(&nb, edit.get("cell_id").ok_or_else(|| key_error("cell_id"))?)?;
            self.require_fresh_read(&nb, &cell)?;
            folds.push(optional_folded(edit.get("folded"))?);
            cells.push(cell);
        }
        let mut ops = Vec::new();
        for ((edit, cell), folded) in edits.iter().zip(&cells).zip(&folds) {
            let code = edit.get("code").ok_or_else(|| key_error("code"))?;
            ops.push(json!({ "op": "set_code", "cell_id": cell, "code": code, "expected": nb.cells[cell].code }));
            ops.extend(folded.map(|folded| json!({ "op": "fold", "cell_id": cell, "folded": folded })));
        }
        let applied = self.apply(&nb.id, ops)?;
        for (edit, cell) in edits.iter().zip(&cells) {
            self.edited(&nb.id, &applied, cell, &nb.cells[cell].code, edit["code"].as_str().unwrap_or_default());
        }
        self.mark_pending(&nb.id, &cells);
        let mutation = json!({ "type": "edit_cells", "cell_ids": cells });
        let (mut receipt, after) = self.receipt(&nb.id, mutation, &[], Vec::new(), None)?;
        // Each cell's fold after the edits, as `edit_cell`'s `code_folded` says its cell's.
        let folds: Map<String, Value> = cells.iter().filter_map(|c| Some((c.clone(), json!(after.cells.get(c)?.folded)))).collect();
        receipt["mutation"]["code_folded"] = Value::Object(folds);
        Ok(Value::Object(receipt))
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
                let target = cell_id_in(&nb, after).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{}'", julia_string(after))))?;
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
        // Its code only if the engine changed what the agent sent: the agent has it, and the app
        // takes it from the call too.
        let mut added = self.cell_json(&after, &cell);
        if self.args.get("code").and_then(Value::as_str) == Some(code.as_str()) {
            added.remove("code");
        }
        receipt.extend(added);
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

    /// Answer the prompt a running cell waits on (`readline()`, `menu()` and the like, #261),
    /// which lets the run go on; waited for as `execute_cell` waits for a run.
    fn answer_prompt(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let cell = self.cell(&nb)?;
        let Value::String(text) = self.arg("text")? else { return Err(argument_error("invalid_argument::text must be a string")) };
        let wait = bool_arg(self.args, "wait_for_completion", false)?;
        let asked = match self.args.get("prompt_id") {
            None | Some(Value::Null) => None,
            Some(Value::Number(id)) => Some(id.clone()),
            Some(_) => return Err(argument_error("invalid_argument::prompt_id must be a number")),
        };
        if nb.prompted() != Some(&cell) {
            return Err(argument_error(&format!("no_prompt::Cell {cell} isn't waiting for input")));
        }
        // An answer for a question that has gone (menu() asking again, the user answering first)
        // isn't given to the next one; Ember checks too, in case it changes before the answer lands.
        let waiting = nb.cells[&cell].prompt.as_ref().map(|p| p["id"].clone()).unwrap_or_default();
        if asked.as_ref().is_some_and(|id| waiting.as_f64() != id.as_f64()) {
            return Err(argument_error(&format!("stale_prompt::Cell {cell} is asking something else now; read it and answer what it asks")));
        }
        self.nbs.call("answer", json!({ "notebook_id": nb.id, "cell_id": cell, "text": text, "prompt_id": asked }))?;
        let mutation = json!({ "type": "answer_prompt", "cell_id": cell });
        let ran = if wait {
            let (cells, going) = self.wait_for_own_run(&nb)?;
            // A run the user or another session started isn't waited for, so this cell is read as it is now.
            let cells = if cells.is_empty() { vec![cell] } else { cells };
            Ran { cells, going, ..Default::default() }
        } else {
            Ran { cells: vec![cell], warnings: vec!["async_execution::cells running; read them for the result".into()], ..Default::default() }
        };
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
                    .map(|c| cell_id_in(&nb, c).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{}'", julia_string(c)))))
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
            // Nothing staged, but a run this session started may still be going (its dependents,
            // say): a waited call waits for it, as `read_cell` does, rather than say it completed
            // while it runs (#151).
            // Its receipt says how the cells that ran meanwhile went: "errored" if one of them
            // did, as for the cells a call runs itself (#171).
            if wait == true {
                let (ran, going) = self.wait_for_own_run(&nb)?;
                let status = ran.is_empty().then_some("completed");
                return Ok(Value::Object(self.receipt_of_run(&nb.id, mutation, Ran { cells: ran, going, ..Default::default() }, status)?.0));
            }
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
            let target = cell_id_in(&nb, after).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{shown}'")))?;
            rest.iter().position(|c| **c == target).ok_or_else(|| key_error(&format!("cell_not_found::Target cell '{shown}' not found")))? + 1
        };
        self.apply(&nb.id, vec![json!({ "op": "move", "cell_id": cell, "index": index })])?;
        let mutation = json!({ "type": "move_cell", "cell_id": cell, "old_index": old_index + 1, "new_index": index + 1 });
        // A move is about the order, so its receipt has it.
        let (mut receipt, after) = self.receipt(&nb.id, mutation, &[], Vec::new(), Some("completed"))?;
        receipt.insert("cell_order".into(), json!(after.order));
        Ok(Value::Object(receipt))
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
        // An Ember file's text cells are already `#'` comments, so they need no `# md:`.
        let ember = self.nbs.backend_of(&nb.id) == Backend::Ember;
        let marker = if ember { EMBER_CELL_MARKER } else { CELL_MARKER };
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
            let body = if cell.markdown && !ember {
                format!("# md:\n{}", cell.code)
            } else if julia_strip(&cell.code).is_empty() {
                "# (empty)".to_owned()
            } else {
                cell.code.clone()
            };
            blocks.push(format!("{marker}{id}\n{body}"));
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
        // A plain script isn't opened: Pluto would leave a backup copy of it next to it before refusing it.
        match wire::backend::Backend::of_file(std::path::Path::new(path)) {
            None => return Err(argument_error(&format!("not_a_notebook::'{path}' isn't a notebook: it doesn't start with a Pluto or Ember notebook's first line"))),
            Some(wire::backend::Backend::Ember) => r_notebooks(&format!("'{path}' is an Ember notebook (R)"))?,
            Some(wire::backend::Backend::Pluto) => {}
        }
        let run = self.args.get("run_notebook").cloned().unwrap_or(json!(false));
        let Value::Bool(run) = run else { return Err(non_boolean(&run)) };
        let key = format!("open {}", canonical_path(path).unwrap_or_else(|_| path.clone()));
        // A call made again while an open is under way gets that open, run as that one asked.
        let (opened, run) = match self.nbs.open_waited("open", json!({ "path": path, "run": run }), key, self.run_wait(), &format!("Opening '{path}'")) {
            Ok((opened, params)) => (opened, params["run"] == true),
            Err(error) if already_open(&error) => return self.join(path).unwrap_or(Err(error)),
            Err(error) => return Err(error),
        };
        let mut result = json!({
            "notebook_id": opened["notebook_id"], "path": opened["path"], "execution_allowed": run, "ran": run,
            "process_status": opened["process_status"],
        });
        if opened["lazy"] == true {
            result["on_cell_change"] = json!("lazy");
        }
        if opened["package_date"].is_string() {
            result["package_date"] = opened["package_date"].clone();
        }
        let mut warnings = self.nbs.take_notices();
        if run {
            warnings.push("async_execution::open queued non-blocking notebook run; poll read_cell for completion".into());
        } else {
            result["note"] = json!(NOT_RUN);
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
        let mut joined = json!({
            "notebook_id": nb.id, "path": nb.path, "execution_allowed": nb.execution_allowed, "ran": false,
            "process_status": nb.process_status, "already_open": true,
        });
        if !nb.execution_allowed {
            joined["note"] = json!(NOT_RUN);
        }
        if nb.lazy {
            joined["on_cell_change"] = json!("lazy");
        }
        if let Some(date) = &nb.package_date {
            joined["package_date"] = json!(date);
        }
        Some(Ok(joined))
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
                // The engine saves the file before it opens it, so a creation still under way has made it already.
                let held = self.nbs.opening(&format!("new {path}"));
                if !held && std::path::Path::new(&path).exists() {
                    return Err(argument_error(&format!("file_exists::'{path}' already exists; use open_notebook to load it")));
                }
                let dir = super::parent_dir(&path);
                if !held && !std::path::Path::new(&dir).is_dir() {
                    return Err(argument_error(&format!("invalid_path::Directory does not exist: '{dir}'")));
                }
                json!({ "path": path })
            }
            Some(_) => return Err(argument_error("invalid_path::path must be a string")),
        };
        // A new notebook without a path is the session's one being made, whatever name the engine gives it.
        let (key, doing) = match params["path"].as_str() {
            Some(path) => (format!("new {path}"), format!("Creating '{path}'")),
            None => (format!("new in {} for {}", params["folder"].as_str().unwrap_or_default(), self.owner), "Creating the notebook".to_owned()),
        };
        let (made, _) = self.nbs.open_waited("new", params, key, self.run_wait(), &doing)?;
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
        let run = self.args.get("run_notebook").cloned().unwrap_or(json!(true));
        let Value::Bool(run) = run else { return Err(non_boolean(&run)) };
        // Leaving safe preview starts a new Julia process for the notebook, which can outlast the
        // call; a call made again while it is under way gets its result. While it starts the
        // runtime answers nothing else, so that call waits for it without reading the notebook.
        let shown = julia_string(&self.args["notebook_id"]);
        let id = uuid_value(&self.args["notebook_id"]).ok_or_else(|| argument_error(&format!("invalid_notebook_id::Invalid notebook ID: '{shown}'")))?;
        let key = format!("allow {id}");
        if !self.nbs.opening(&key) {
            let nb = self.notebook()?;
            // Already out of safe preview, perhaps still starting from an earlier call whose result
            // was collected: the engine would refuse it as not in safe preview.
            if nb.execution_allowed {
                return Ok(json!({
                    "notebook_id": nb.id, "execution_allowed": true, "already_allowed": true, "ran": false, "process_status": nb.process_status,
                }));
            }
        }
        let params = json!({ "notebook_id": id, "run": run, "timeout": TIMEOUT_SECONDS });
        let why = "a new Julia process starts for it, and its packages may need installing";
        let (allowed, _) = self.nbs.waited("allow_execution", params, key, self.run_wait(), &format!("Starting the notebook is taking longer than one call waits: {why}"))?;
        let ran = allowed["ran"] == true;
        let mut result = json!({
            "notebook_id": id, "execution_allowed": true, "already_allowed": allowed["already_allowed"], "ran": ran,
            "process_status": allowed["process_status"],
        });
        if ran {
            let nb = self.notebook()?;
            self.nbs.with_state(&nb.id, |state| state.prune(&nb));
            // The run of the whole notebook is the session's, as one `submit_changes` starts is: a
            // read or a waited submit waits for it (#170). The engine names it; one that didn't
            // goes on while a cell it started hasn't finished.
            let run = allowed["run"].as_str().map(str::to_owned);
            self.nbs.with_state(&nb.id, |state| {
                let ended = run.as_ref().is_some_and(|run| state.ended.contains(run));
                state.ended.retain(|r| Some(r) != run.as_ref());
                if !self.owner.is_empty() && !ended {
                    state.started.push(Started { owner: self.owner.to_owned(), run, cells: nb.order.iter().cloned().collect(), follows: HashMap::new() });
                }
            });
            result["run_warnings"] = json!(["async_execution::allow_execution queued non-blocking notebook run; poll read_cell for completion"]);
        }
        Ok(result)
    }

    /// The page's Stop, for the agent (#161): interrupt the notebook's run, whoever started it.
    /// The running cells end interrupted and the queued ones don't run. Pluto's interrupt
    /// returns once the run has stopped (ending the process if a cell won't); Ember's sends
    /// SIGINT and returns, so the cells are watched until they stop, for at most
    /// `INTERRUPT_WAIT_SECONDS` and within the call's `WAIT_SECONDS`.
    fn interrupt_notebook(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let busy = |nb: &Snapshot, id: &String| nb.cells.get(id).is_some_and(|c| c.running || c.queued);
        let running: Vec<String> = nb.order.iter().filter(|id| nb.cells.get(*id).is_some_and(|c| c.running)).cloned().collect();
        let queued: Vec<String> = nb.order.iter().filter(|id| nb.cells.get(*id).is_some_and(|c| c.queued && !c.running)).cloned().collect();
        if running.is_empty() && queued.is_empty() {
            return Ok(json!({ "notebook_id": nb.id, "interrupted": false, "message": "Nothing was running." }));
        }
        let reply = self.nbs.call("interrupt", json!({ "notebook_id": nb.id }))?;
        // The notebook's process is still starting, so its run hasn't started a cell, and it skips
        // them all once the process is up (#179): they are dropped, however long the start takes.
        let pending = reply["pending"] == true;
        let mut now = self.nbs.snapshot(&nb.id)?;
        let deadline = Instant::now() + self.wait_left().min(Duration::from_secs_f64(INTERRUPT_WAIT_SECONDS));
        while !pending && running.iter().chain(&queued).any(|id| busy(&now, id)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            if (self.gone)() {
                return Err(argument_error("client_gone::The caller hung up during the wait"));
            }
            now = self.nbs.snapshot(&nb.id)?;
        }
        let (still_running, stopped): (Vec<String>, Vec<String>) = running.into_iter().partition(|id| busy(&now, id));
        let dropped: Vec<String> = queued.into_iter().filter(|id| pending || !busy(&now, id)).collect();
        let mut result = json!({
            "notebook_id": nb.id, "interrupted": reply["interrupted"] == true || !stopped.is_empty() || !dropped.is_empty(),
            "stopped": stopped, "dropped": dropped, "still_running": still_running, "process_status": now.process_status,
        });
        if pending {
            result["message"] = json!("The notebook's process is still starting. Its run will skip these cells once the process is up.");
        } else if !result["still_running"].as_array().unwrap().is_empty() {
            result["message"] = json!(
                "These cells haven't stopped yet. Compiled code often stops only when it returns. Read them later; if one doesn't stop, restart_notebook ends it, but it clears every value, so ask the user first. In Julia the restart runs every cell again, so edit the cell first."
            );
        } else if now.process_status != nb.process_status && now.process_status == "no_process" {
            result["message"] = json!("A cell didn't stop, so the notebook's process was ended. Its values are gone: the next run starts a new process.");
        }
        Ok(result)
    }

    /// The page's Restart, for the agent (#202): a new process for the notebook, with every value
    /// gone. Pluto's then runs every cell, and that run is the session's, as `allow_execution`'s
    /// is: a read or a waited submit waits for it. Since it runs every cell, a cell that was running
    /// runs again, so the result names those (`running_again`): a restart meant to end a stuck cell
    /// doesn't, unless the cell was edited first. Ember's leaves every cell not run. The engines
    /// refuse it in safe preview.
    fn restart_notebook(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        let running: Vec<String> = nb.order.iter().filter(|id| nb.cells.get(*id).is_some_and(|c| c.running)).cloned().collect();
        let reply = self.nbs.restart(&nb.id)?;
        if reply["restarted"] != true {
            return Ok(json!({ "notebook_id": nb.id, "restarted": false, "message": "The notebook is already restarting. Read a cell to wait for it." }));
        }
        let now = self.nbs.snapshot(&nb.id)?;
        let mut result = json!({ "notebook_id": nb.id, "restarted": true, "process_status": now.process_status });
        result["message"] = json!(match self.nbs.backend_of(&nb.id) {
            Backend::Pluto => {
                self.nbs.with_state(&nb.id, |state| {
                    if !self.owner.is_empty() {
                        state.started.push(Started { owner: self.owner.to_owned(), run: None, cells: nb.order.iter().cloned().collect(), follows: HashMap::new() });
                    }
                });
                if running.is_empty() {
                    "A new process started, and every cell runs in it. Read a cell to wait for its result.".to_owned()
                } else {
                    let again = running.len();
                    result["running_again"] = json!(running);
                    format!(
                        "A new process started, and every cell runs in it, so the {} in `running_again` {} running again with the same code. To end {}, interrupt_notebook, edit {} so it finishes (the edit stays staged), then restart_notebook again.",
                        if again == 1 { "cell" } else { "cells" }, if again == 1 { "is" } else { "are" }, if again == 1 { "it" } else { "them" }, if again == 1 { "it" } else { "them" }
                    )
                }
            }
            Backend::Ember => "R started fresh. Every cell is `not_run` and holds no value until it runs: `run_all_cells` runs the notebook from the top.".to_owned(),
        });
        Ok(result)
    }

    /// The session's notebook, if it is an R notebook: only Ember's packages have a snapshot date.
    fn r_notebook(&self) -> Result<Snapshot, String> {
        let nb = self.notebook()?;
        if self.nbs.backend_of(&nb.id) != Backend::Ember {
            return Err(argument_error("not_r_notebook::Only an R notebook has a package date. A Julia notebook's package versions are in its Pkg environment"));
        }
        Ok(nb)
    }

    /// A new value for an R notebook's input (emberui, Ember's docs/inputs.md), as moving its
    /// widget on the page does: Ember's `set_input()`. R checks the value against the input; in
    /// autorun the cells that read its name rerun, as a run of those cells, and in lazy mode they
    /// are marked stale. The input's own cell doesn't rerun, and the value isn't saved in the file.
    fn set_input(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        if self.nbs.backend_of(&nb.id) != Backend::Ember {
            return Err(argument_error("not_r_notebook::Only an R notebook has inputs. A Julia notebook's `@bind` widgets are set from the page"));
        }
        let Value::String(name) = self.arg("name")? else { return Err(argument_error("invalid_argument::name must be the name the input is assigned to")) };
        let value = self.arg("value")?.clone();
        let wait = bool_arg(self.args, "wait_for_completion", false)?;
        let timeout = if wait { self.run_wait().as_secs_f64() } else { TIMEOUT_SECONDS };
        // Edited and not yet run, as `run` takes them before its call.
        let staged = self.pending_run(&nb);
        let reply = self.nbs.call("set_input", json!({ "notebook_id": nb.id, "name": name, "value": value, "wait": wait, "timeout": timeout }))?;
        self.nbs.publish();
        let cells: Vec<String> = reply["cells"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        let mutation = json!({ "type": "set_input", "name": name, "cell_id": reply["cell"], "value": reply["value"] });
        // R reads the value only once its running cell finishes. Not read by the wait's end,
        // it is still on its way: said so, not reported as refused (it would land anyway).
        if reply["pending"] == true {
            let (mut receipt, _) = self.receipt(&nb.id, mutation, &[], Vec::new(), Some("running"))?;
            receipt.insert("pending".into(), json!(true));
            receipt.insert("message".into(), json!(format!("`{name}` will be set when R finishes the cell it is running; then the cells that read it {}. read_cell the input's cell to see its value once it has.", if reply["lazy"] == true { "turn stale" } else { "rerun" })));
            return Ok(Value::Object(receipt));
        }
        let (mut receipt, _) = if reply["lazy"] == true || cells.is_empty() {
            self.receipt(&nb.id, mutation, &[], Vec::new(), Some("completed"))?
        } else {
            let ran = self.ran(&nb, &cells, &[], Vec::new(), cells.clone(), &reply, wait, &staged)?;
            self.receipt_of_run(&nb.id, mutation, ran, None)?
        };
        receipt.insert("message".into(), json!(if cells.is_empty() {
            format!("`{name}` is set. No cell that has run reads it.")
        } else if reply["lazy"] == true {
            format!("`{name}` is set. The notebook is in lazy mode, so the cells that read it are stale and haven't rerun: {}. Run them to see the new value's results.", cells.join(", "))
        } else {
            format!("`{name}` is set, and the cells that read it rerun: {}.", cells.join(", "))
        }));
        if reply["lazy"] == true {
            receipt.insert("stale_cells".into(), json!(cells));
        }
        Ok(Value::Object(receipt))
    }

    /// Ember's preview of moving an R notebook's snapshot date, which its package versions come
    /// from (#201): what would change, with nothing changed yet. Without `date` it is today, the
    /// page's "Update" (the newest packages). Fetching that date's index can take a while, so it
    /// is waited for within the call. A day not published yet is previewed at the newest day that
    /// is, and the result's `date` is that day. With `package` (and `version`, else CRAN's newest),
    /// the date is Ember's for that package (`preview_update`, #266): the first day a newer
    /// version was current, or the last day an older one was, so the others move as little as
    /// they can. Every package moves with the date.
    fn preview_package_update(&self) -> Result<Value, String> {
        let nb = self.r_notebook()?;
        let arg = |name: &str| self.args.get(name).cloned().unwrap_or(Value::Null);
        let (date, package, version) = (arg("date"), arg("package"), arg("version"));
        if !date.is_null() && !date.is_string() {
            return Err(argument_error("invalid_argument::date must be a day as YYYY-MM-DD"));
        }
        if !package.is_null() && !package.is_string() || !version.is_null() && !version.is_string() {
            return Err(argument_error("invalid_argument::package and version must be strings, such as \"dplyr\" and \"1.1.2\""));
        }
        if !version.is_null() && package.is_null() {
            return Err(argument_error("invalid_argument::version needs package: the package to put at that version"));
        }
        if !date.is_null() && !package.is_null() {
            return Err(argument_error("invalid_argument::Give date or package, not both: a package's version decides the date"));
        }
        let timeout = self.run_wait().saturating_sub(Duration::from_secs(1)).as_secs_f64();
        let mut result = self.nbs.call("preview_packages", json!({ "notebook_id": nb.id, "date": date, "package": package, "version": version, "timeout": timeout }))?;
        let reason = result["message"].take();
        if reason.is_string() {
            result["reason"] = reason;
        }
        // A day not published yet is previewed at the newest one that is, an earlier `date` (#247).
        let asked = result.as_object_mut().and_then(|r| r.remove("asked"));
        let earlier = asked.as_ref().is_some_and(|a| a.is_string() && *a != result["date"]);
        let none = result["changes"].as_array().is_none_or(Vec::is_empty);
        // With no packages yet there is nothing to change, but applying still moves the date, which
        // every package added later comes from (#269). An adapter that doesn't say counts as having some.
        let no_packages = result.as_object_mut().and_then(|r| r.remove("packages")).is_some_and(|n| n == 0);
        let said = match result["status"].as_str() {
            Some("ready") if none && no_packages => "The notebook has no packages yet, so none would change. apply_package_update with this date still moves the notebook's date to it, and every package added later comes from that date: tell the user, since an old date can give old versions or none at all.",
            Some("ready") if none => "Nothing would change: every package is already at its version for this date.",
            Some("ready") if package.is_string() => "Nothing has changed yet. This date moves every package, not only this one: tell the user what else would change; apply_package_update with this date applies it.",
            Some("ready") => "Nothing has changed yet. Tell the user what would change; apply_package_update with this date applies it.",
            Some("failed") if package.is_string() => "No package date puts this package at that version, and the notebook is unchanged: `reason` or `problems` says why.",
            Some("failed") => "This date can't be used, and the notebook is unchanged: `problems` says why.",
            _ if package.is_string() => "The date for this package is still being worked out. Call again with the same package and version to wait for it.",
            _ => "The package index for this date is still being fetched. Call again with the same date to wait for it.",
        };
        result["message"] = json!(match (earlier, result["status"].as_str()) {
            (true, Some("ready")) => format!("{} isn't published yet, so this is the newest day that is, {}. {said}", asked.unwrap().as_str().unwrap(), result["date"].as_str().unwrap_or("")),
            _ => said.to_owned(),
        });
        result["notebook_id"] = json!(nb.id);
        Ok(result)
    }

    /// Apply the preview of `date` (Ember's `set_date`), refused unless that preview is ready
    /// and current. The header's date and the lock change and the file is saved; the library
    /// installs, and if R had a changed package loaded it restarts once the library is ready.
    fn apply_package_update(&self) -> Result<Value, String> {
        let nb = self.r_notebook()?;
        let date = self.arg("date")?.clone();
        if !date.is_string() {
            return Err(argument_error("invalid_argument::date must be a day as YYYY-MM-DD"));
        }
        let mut result = self.nbs.call("set_package_date", json!({ "notebook_id": nb.id, "date": date }))?;
        self.nbs.publish();
        result["notebook_id"] = json!(nb.id);
        // In safe preview Ember installs nothing until the user lets the notebook run.
        result["message"] = json!(if result["process_status"] == "waiting_for_permission" {
            "The notebook's package date has moved. Its library installs once the user allows the notebook to run (the notebook's page, or allow_execution): nothing installs or runs until then."
        } else {
            "The notebook's package date has moved. Its library installs now: cells that need it wait, queued, and a read of one waits for the install. If R had a changed package loaded, R restarts once the library is ready and every cell is left `not_run`."
        });
        Ok(result)
    }

    /// The user's OK for an R notebook's install that stopped before a large download (Ember
    /// #37's `approve_install`). The tool's description and the asking message say to call it
    /// only after the user said yes. Refused when no install waits for one.
    fn approve_install(&self) -> Result<Value, String> {
        let nb = self.notebook()?;
        if self.nbs.backend_of(&nb.id) != Backend::Ember {
            return Err(argument_error("not_r_notebook::Only an R notebook asks before a large download. A Julia notebook's packages install through Pkg"));
        }
        let mut result = self.nbs.call("approve_install", json!({ "notebook_id": nb.id }))?;
        self.nbs.publish();
        result["notebook_id"] = json!(nb.id);
        result["message"] = json!(
            "The download is approved and the library installs now: the queued cells run once it is done, and a read of one waits for the install."
        );
        Ok(result)
    }
}

/// The cell a `cell_id` argument names, refused as Julia's lookup refused it.
fn cell_named(nb: &Snapshot, value: &Value) -> Result<String, String> {
    let shown = julia_string(value);
    let id = cell_id_in(nb, value).ok_or_else(|| argument_error(&format!("invalid_cell_id::Invalid cell ID: '{shown}'")))?;
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
        // A cell still to run carries its last result's error until it runs, so
        // that error isn't this run's: a run waiting at a prompt is still running.
        if cells.iter().any(|c| c.errored && !c.running && !c.queued) {
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
/// The prompt `cell` waits on, as the tools show it.
fn waiting_json(nb: &Snapshot, cell: &str) -> Value {
    let prompt = nb.cells.get(cell).and_then(|c| c.prompt.clone()).unwrap_or_default();
    json!({ "cell_id": cell, "prompt_id": prompt["id"], "prompt": prompt["text"].as_str().unwrap_or_default(), "choices": prompt["choices"].as_array().cloned().unwrap_or_default() })
}

/// What the agent is told while `cell` waits on a prompt.
fn prompt_message(nb: &Snapshot, cell: &str) -> String {
    let menu = nb.cells.get(cell).and_then(|c| c.prompt.as_ref()).and_then(|p| p["choices"].as_array()).is_some_and(|c| !c.is_empty());
    let how = if menu { " (a choice's number or its text)" } else { "" };
    format!(
        "Cell {cell} is waiting for input, as R does at a console prompt: `waiting_for_input` has what it asked, and the cell's output what it printed before. \
         Nothing runs until it is answered. Answer with answer_prompt{how}, passing `prompt_id`, or stop the run with interrupt_notebook. \
         If the answer is the user's to give (a download, an overwrite, a choice of theirs), ask the user first."
    )
}

fn installing_message(nb: &Snapshot) -> String {
    if nb.asking() {
        return asking_message(nb);
    }
    let packages = nb.packages.as_ref().cloned().unwrap_or_default();
    let names: Vec<&str> = packages["packages"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let which = if names.is_empty() { "the packages it uses".to_owned() } else { names.join(", ") };
    let step = packages["step"].as_str().unwrap_or("installing");
    let seconds = packages["seconds"].as_u64().unwrap_or(0);
    let so_far = if seconds >= 90 { format!("about {} minutes", (seconds + 30) / 60) } else { format!("{seconds} seconds") };
    let last = packages["last_line"].as_str().map(|line| format!(" Last line of the package log: {line}")).unwrap_or_default();
    let language = super::engines::language(super::engines::of_path(&nb.path));
    let mut what = format!("{language} is getting {which} ready for this notebook ({step}, {so_far} so far). The cells are queued and run when that is done.");
    // Ember's notice that these build from source (Ember #19), as the Packages tab says it.
    let source: Vec<&str> = packages["source_builds"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !source.is_empty() {
        what.push_str(&format!(
            " These have no binary for this {language} at the notebook's package date, so they are being built on this computer: {}. \
             That takes longer, and versions from before this {language} may not build on it. A newer package date has binaries (preview_package_update).",
            source.join(", ")
        ));
    }
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

/// What the agent hears while an R notebook's install waits for an OK before a large download
/// (Ember #37): what it would download and how much, and that the user decides. Only the user
/// can say yes to it, so the agent asks them and doesn't wait.
fn asking_message(nb: &Snapshot) -> String {
    let download = nb.packages.as_ref().map(|p| p["download"].clone()).unwrap_or_default();
    let sized = |bytes: &Value| bytes.as_f64().map(|b| if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else { format!("{:.0} MB", b / 1e6) });
    let each: Vec<String> = download["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let name = [p["package"].as_str(), p["version"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" ");
            match sized(&p["bytes"]) {
                Some(size) => format!("{name} ({size})"),
                None => format!("{name} (size unknown)"),
            }
        })
        .collect();
    let which = if each.is_empty() { "some large packages".to_owned() } else { each.join(", ") };
    let total = sized(&download["bytes"]).map(|size| format!(", {size} in all")).unwrap_or_default();
    format!(
        "Before this notebook's cells can run, R has to download {which}{total}. A download this large waits for the user's OK, \
         so nothing is installing and the cells stay queued. Ask the user whether to download it, and call approve_install only after they say yes; \
         the page's own button does the same. Don't call read_cell to wait: nothing changes until then."
    )
}

/// What Claude hears when a notebook's own process (`language`'s, Julia or R) ends during a run it
/// waits for, in the words of the app's crash page.
fn exited_message(language: &str, cell: Option<&str>) -> String {
    let stopped = match cell {
        Some(cell) => format!("{language} stopped unexpectedly while running `{cell}`."),
        None => format!("{language} stopped unexpectedly."),
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

/// The first 12 hex digits of the code's SHA-256 (Endeavor's `celldiff::code_sha` is the same).
fn code_sha(code: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(code.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// An edit's `folded`: a Bool, or left out (or null) to keep the cell's fold as it is.
fn optional_folded(value: Option<&Value>) -> Result<Option<bool>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(folded)) => Ok(Some(*folded)),
        Some(_) => Err(argument_error("invalid_argument::folded must be a boolean")),
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

/// How `view_cell_output` says an output has no PNG form, and how the runtime says it couldn't ask
/// the worker for one (`_cell_png` in `Output.jl`): the failures a picture of the notebook can stand in for.
const NO_PNG: &str = "has no PNG rendering";
const NO_WORKER: &str = "has no running worker";

/// Whether a picture of the output as the notebook draws it (`guard::Snapshot`) shows the agent more
/// than `view_cell_output`'s `result`: an HTML output with no PNG form, one whose picture came from
/// its HTML (maybe only part of it), or any output while the notebook has no worker to render it.
pub(crate) fn snapshot_may_help(result: &Value) -> bool {
    let content = &result["content"];
    if result["isError"] == true {
        let error: Value = content[0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()).unwrap_or_default();
        let message = error["message"].as_str().unwrap_or_default();
        return error["error"] == "no_image" && ((message.contains("(text/html)") && message.contains(NO_PNG)) || message.contains(NO_WORKER));
    }
    let meta: Value = content[0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()).unwrap_or_default();
    content[1]["type"] == "image" && meta["source"] == "embedded image"
}

/// `view_cell_output`'s result for `png` (base64), a picture of cell `cell` as the notebook draws it,
/// in place of the runtime's `result`, or None if it is over the size an agent takes. It keeps the
/// output's MIME type when the runtime's result named it.
pub(crate) fn screenshot_result(cell: &str, png: String, result: &Value) -> Option<Value> {
    let bytes = base64_len(&png);
    if bytes > MAX_IMAGE_BYTES {
        return None;
    }
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let said: Value = serde_json::from_str(text).unwrap_or_default();
    let shown_as = said["shown_as"].as_str().map(str::to_owned).or_else(|| {
        let message = said["message"].as_str()?;
        let (_, rest) = message.split_once("its output (")?;
        Some(rest.split_once(')')?.0.to_owned())
    });
    let mut meta = json!({ "cell_id": cell, "source": "screenshot", "png_bytes": bytes });
    if let Some(shown_as) = shown_as {
        meta["shown_as"] = json!(shown_as);
    }
    Some(crate::mcp::image_result(&meta, png))
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

/// The notes of a run's cells as the reply's warnings, "kind::Cell <id>: text". The same note on
/// several cells (a `@warn` in a function they all call) is one line, "kind::Cells <id>, <id>:
/// text". Logged errors come first, then warnings and Ember's notes, then `QUIET_NOTES`, which a cell that
/// only ran along doesn't give. At most `RUN_NOTES_MAX` lines and `RUN_NOTES_BYTES`, then a
/// `notes_cut` line.
fn run_notes(cells: &[(&String, &Cell)], along: &[String]) -> Vec<String> {
    let mut folded: Vec<(&str, &str, Vec<&str>)> = Vec::new();
    for (id, cell) in cells {
        for note in &cell.notes {
            let (kind, text) = note.split_once("::").unwrap_or(("note", note));
            if along.contains(id) && QUIET_NOTES.contains(&kind) {
                continue;
            }
            match folded.iter_mut().find(|(k, t, _)| *k == kind && *t == text) {
                Some((_, _, ids)) => ids.push(id),
                None => folded.push((kind, text, vec![id])),
            }
        }
    }
    folded.sort_by_key(|(kind, _, _)| if *kind == "logged_error" { 0 } else if QUIET_NOTES.contains(kind) { 2 } else { 1 });
    let (mut lines, mut bytes) = (Vec::new(), 0);
    for (shown, (kind, text, ids)) in folded.iter().enumerate() {
        let line = match &ids[..] {
            [id] => format!("{kind}::Cell {id}: {text}"),
            ids => format!("{kind}::Cells {}: {text}", ids.join(", ")),
        };
        if lines.len() == RUN_NOTES_MAX || bytes + line.len() > RUN_NOTES_BYTES {
            let left = folded.len() - shown;
            lines.push(format!("notes_cut::{left} more notes from this run are not shown; read_cell lists each cell's notes"));
            break;
        }
        bytes += line.len();
        lines.push(line);
    }
    lines
}

/// A run's `message` when a cell it ran logged a warning or an error: the first, quoted, and that
/// the user should hear it. In N11, Haiku 4.5 read such a warning in `warnings` and reported the
/// result as clean.
fn logged_message(logged: &[&str]) -> Option<String> {
    let first = logged.first()?;
    let (kind, rest) = first.split_once("::")?;
    let (cells, text) = rest.split_once(": ")?;
    let what = if kind == "logged_error" { "an error" } else { "a warning" };
    let more = if logged.len() > 1 { format!(" The run's `warnings` have {} more.", logged.len() - 1) } else { String::new() };
    Some(format!("{cells} logged {what}: \"{text}\". The user sees it under the cell. When you report the result, tell the user what it says, since the result may not be what it looks like.{more}"))
}

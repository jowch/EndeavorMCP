//! The harness's own MCP sessions: `endeavor mcp` over stdio, joining the
//! runtime the agent's session uses. The runner sets notebooks up with one and
//! reads what the agent left with another; the proxy plays a second person
//! with a third.

use std::io::{BufRead, BufReader, Lines, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Session {
    pub fn start(endeavor: &Path, work: &Path, folder: &Path) -> Result<Session, String> {
        let mut child = Command::new(endeavor)
            .arg("mcp")
            .arg("--folder")
            .arg(folder)
            .args(crate::run::runtime_args(work))
            .envs(crate::run::runtime_env(work))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", endeavor.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut session = Session { child, stdin, stdout, next: 0 };
        session.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} }))?;
        writeln!(session.stdin, "{}", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).map_err(|e| e.to_string())?;
        Ok(session)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        writeln!(self.stdin, "{}", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).map_err(|e| e.to_string())?;
        loop {
            let line = self.stdout.next().ok_or("the checker's server ended")?.map_err(|e| e.to_string())?;
            let msg: Value = serde_json::from_str(&line).map_err(|e| format!("{e}: {line}"))?;
            if msg["id"] == id {
                return Ok(msg);
            }
        }
    }

    /// A tool's reply text, parsed as JSON. A runtime that is still starting is asked again.
    pub fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let began = Instant::now();
        loop {
            let msg = self.request("tools/call", json!({ "name": name, "arguments": arguments }))?;
            let text = msg["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
            if msg["result"]["isError"] == true {
                if text.to_lowercase().contains("starting") && began.elapsed() < Duration::from_secs(600) {
                    continue;
                }
                return Err(format!("{name}: {text}"));
            }
            return serde_json::from_str(&text).map_err(|e| format!("{name}: {e}: {text}"));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A notebook as the checker read it after the agent ended.
#[derive(Debug, Default, Clone)]
pub struct Notebook {
    pub path: String,
    pub execution_allowed: bool,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Default, Clone)]
pub struct Cell {
    pub id: String,
    pub code: String,
    pub errored: bool,
    /// The output as text: `read_cell`'s `output_text` where the runtime renders one, else its `output`.
    pub output: String,
    /// Still running or queued when it was read.
    pub busy: bool,
}

impl Notebook {
    pub fn cell(&self, id: &str) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }
}

/// Every notebook open in the runtime, with each cell's code, output and error.
pub fn notebooks(session: &mut Session) -> Result<Vec<Notebook>, String> {
    let listed = session.tool("list_notebooks", json!({}))?;
    let mut out = Vec::new();
    for nb in listed.as_array().cloned().unwrap_or_default() {
        let id = nb["notebook_id"].clone();
        let code = session.tool("read_notebook_code", json!({ "notebook_id": id, "order": "visual", "include_markdown": true }))?;
        let mut cells = Vec::new();
        for cell in code["cell_ids"].as_array().cloned().unwrap_or_default() {
            let read = session.tool("read_cell", json!({ "notebook_id": id, "cell_id": cell }))?;
            let output = read["output_text"].as_str().or(read["output"].as_str()).unwrap_or_default().to_owned();
            cells.push(Cell {
                id: cell.as_str().unwrap_or_default().to_owned(),
                code: read["code"].as_str().unwrap_or_default().to_owned(),
                errored: read["errored"] == true,
                output,
                busy: read["running"] == true || read["queued"] == true,
            });
        }
        out.push(Notebook { path: nb["path"].as_str().unwrap_or_default().to_owned(), execution_allowed: nb["execution_allowed"] == true, cells });
    }
    Ok(out)
}

/// The id of the open notebook whose file is `name` (a path relative to the project, or its file name).
pub fn notebook_id(session: &mut Session, name: &str) -> Result<String, String> {
    let listed = session.tool("list_notebooks", json!({}))?;
    listed
        .as_array()
        .into_iter()
        .flatten()
        .find(|nb| nb["path"].as_str().is_some_and(|p| Path::new(p).ends_with(name)))
        .and_then(|nb| nb["notebook_id"].as_str().map(str::to_owned))
        .ok_or_else(|| format!("no open notebook is {name}: {listed}"))
}

/// Open a notebook from the project, allowed to run, and run every cell to the end.
pub fn open_and_run(session: &mut Session, path: &str, limit: Duration) -> Result<String, String> {
    let opened = session.tool("open_notebook", json!({ "path": path }))?;
    let id = opened["notebook_id"].as_str().ok_or_else(|| format!("open_notebook: no notebook_id: {opened}"))?.to_owned();
    session.tool("allow_execution", json!({ "notebook_id": id, "run_notebook": false }))?;
    session.tool("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }))?;
    wait_until_done(session, &id, limit)?;
    Ok(id)
}

/// Wait until no cell of a notebook is running or queued.
pub fn wait_until_done(session: &mut Session, id: &str, limit: Duration) -> Result<(), String> {
    let began = Instant::now();
    loop {
        let code = session.tool("read_notebook_code", json!({ "notebook_id": id }))?;
        let mut busy = false;
        for cell in code["cell_ids"].as_array().cloned().unwrap_or_default() {
            let read = session.tool("read_cell", json!({ "notebook_id": id, "cell_id": cell }))?;
            busy |= read["running"] == true || read["queued"] == true;
        }
        if !busy {
            return Ok(());
        }
        if began.elapsed() > limit {
            return Err(format!("cells still running after {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

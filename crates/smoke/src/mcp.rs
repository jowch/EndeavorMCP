//! The checker's own MCP session: `endeavor mcp` over stdio, joining the
//! runtime the agent's session used, to read the notebooks it left.

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
    pub code: String,
    pub errored: bool,
    pub output: String,
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
                code: read["code"].as_str().unwrap_or_default().to_owned(),
                errored: read["errored"] == true,
                output,
            });
        }
        out.push(Notebook { path: nb["path"].as_str().unwrap_or_default().to_owned(), execution_allowed: nb["execution_allowed"] == true, cells });
    }
    Ok(out)
}

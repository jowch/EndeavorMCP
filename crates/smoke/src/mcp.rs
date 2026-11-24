use std::io::{BufRead, BufReader, Lines, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn command(endeavor: &Path, work: &Path, folder: &Path) -> Command {
    let mut c = Command::new(endeavor);
    c.arg("mcp").arg("--folder").arg(folder).args(crate::run::runtime_args(work)).envs(crate::run::runtime_env(work)).env_remove("CLAUDE_CODE_SESSION_ID");
    c
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Session {
    pub fn start(endeavor: &Path, work: &Path, folder: &Path) -> Result<Session, String> {
        let mut child = command(endeavor, work, folder)
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

    pub fn view(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let msg = self.request("tools/call", json!({ "name": name, "arguments": arguments }))?;
        if msg["result"]["isError"] == true {
            return Err(format!("{name}: {}", msg["result"]["content"][0]["text"].as_str().unwrap_or_default()));
        }
        Ok(msg["result"]["structuredContent"].clone())
    }

    pub fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let began = Instant::now();
        loop {
            let msg = self.request("tools/call", json!({ "name": name, "arguments": arguments }))?;
            let text = msg["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
            let code = serde_json::from_str::<Value>(&text).ok().and_then(|reply| reply["error"].as_str().map(str::to_owned)).unwrap_or_default();
            if matches!(code.as_str(), "r_installing" | "python_installing") && began.elapsed() < Duration::from_secs(1200) {
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            let starting = matches!(code.as_str(), "julia_starting" | "notebook_opening") || (serde_json::from_str::<Value>(&text).is_err() && text.to_lowercase().contains("starting"));
            if starting && began.elapsed() < Duration::from_secs(600) {
                continue;
            }
            if msg["result"]["isError"] == true || !code.is_empty() {
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
    pub output: String,
    pub busy: bool,
}

impl Notebook {
    pub fn cell(&self, id: &str) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }
}

pub fn notebooks(session: &mut Session) -> Result<Vec<Notebook>, String> {
    let listed = session.tool("list_notebooks", json!({}))?;
    let mut out = Vec::new();
    for nb in listed.as_array().cloned().unwrap_or_default() {
        let id = nb["notebook_id"].clone();
        let code = session.tool("read_notebook_code", json!({ "notebook_id": id, "order": "visual", "include_markdown": true }))?;
        let mut cells = Vec::new();
        for cell in code["cell_ids"].as_array().cloned().unwrap_or_default() {
            let read = session.tool("read_cell", json!({ "notebook_id": id, "cell_id": cell, "include_code": true }))?;
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

pub fn open_and_run(session: &mut Session, path: &str, limit: Duration) -> Result<String, String> {
    let opened = session.tool("open_notebook", json!({ "path": path }))?;
    let id = opened["notebook_id"].as_str().ok_or_else(|| format!("open_notebook: no notebook_id: {opened}"))?.to_owned();
    session.tool("allow_execution", json!({ "notebook_id": id, "run_notebook": false }))?;
    session.tool("run_all_cells", json!({ "notebook_id": id, "wait_for_completion": true }))?;
    wait_until_done(session, &id, limit)?;
    Ok(id)
}

pub fn wait_until_done(session: &mut Session, id: &str, limit: Duration) -> Result<(), String> {
    let began = Instant::now();
    loop {
        let code = session.tool("read_notebook_code", json!({ "notebook_id": id }))?;
        let mut busy = false;
        for cell in code["cell_ids"].as_array().cloned().unwrap_or_default() {
            let read = session.tool("read_cell", json!({ "notebook_id": id, "cell_id": cell }))?;
            busy |= read["running"] == true || read["queued"] == true;
            if let Some(why) = held(&read) {
                return Err(why);
            }
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

fn held(read: &Value) -> Option<String> {
    (read["packages"]["step"] == "asking").then(|| format!("cell {} waits for an OK to install {}, which no one gives here", read["cell_id"].as_str().unwrap_or("?"), read["packages"]["packages"]))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn a_wait_ends_at_a_cell_held_for_an_ok_no_one_gives() {
        let asking = serde_json::json!({ "cell_id": "c1", "queued": true, "packages": { "step": "asking", "packages": ["numpy"] } });
        assert!(super::held(&asking).is_some_and(|w| w.contains("numpy")));
        let installing = serde_json::json!({ "cell_id": "c1", "queued": true, "packages": { "step": "installing", "packages": ["numpy"] } });
        assert_eq!(super::held(&installing), None, "an install under way ends by itself");
        assert_eq!(super::held(&serde_json::json!({ "cell_id": "c1", "running": true })), None);
    }

    #[test]
    fn a_harness_session_is_not_the_agents() {
        let c = super::command(Path::new("endeavor"), Path::new("/work"), Path::new("/project"));
        assert!(c.get_envs().any(|(k, v)| k == "CLAUDE_CODE_SESSION_ID" && v.is_none()));
    }
}

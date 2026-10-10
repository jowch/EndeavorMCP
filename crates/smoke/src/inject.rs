//! A second person in the notebook. A task's `inject.json` names a moment, the
//! first time the agent calls one of `tools` on `notebook` (`"when": "before"`
//! the call reaches the server, or `"after"` its reply, if it succeeded), and
//! the calls another Endeavor session then makes there. The proxy holds the
//! agent's message until they are done, so the timing is the same every run.
//! Each step is logged as `"dir": "inject"`, which the `injected` check reads.
//!
//!     { "when": "after", "tools": ["read_cell", "read_notebook_code"], "notebook": "decay.jl",
//!       "steps": [{ "tool": "read_cell", "args": { "cell_id": "..." } }, { "tool": "edit_cell", "args": { ... } }] }
//!
//! Each step's `notebook_id` is filled in. It happens once per attempt, even if
//! the agent restarts its server.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::log::Log;

pub struct Injection {
    spec: Value,
    work: PathBuf,
    endeavor: PathBuf,
    project: PathBuf,
}

impl Injection {
    /// The task's injection, if it has one.
    pub fn load(work: &Path, endeavor: &Path, project: &Path) -> Option<Injection> {
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(work.join("inject.json")).ok()?).ok()?;
        Some(Injection { spec, work: work.to_owned(), endeavor: endeavor.to_owned(), project: project.to_owned() })
    }

    /// Whether an agent's call to `tool` at this moment is the one to act on.
    pub fn matches(&self, when: &str, tool: &str) -> bool {
        self.spec["when"] == when && self.spec["tools"].as_array().into_iter().flatten().any(|t| t == tool) && !self.marker().exists()
    }

    fn marker(&self) -> PathBuf {
        self.work.join("injected")
    }

    /// Make the other session's calls, logging each.
    pub fn run(&self, log: &Mutex<Log>) {
        let _ = std::fs::write(self.marker(), "");
        let result = (|| -> Result<(), String> {
            let mut session = crate::mcp::Session::start(&self.endeavor, &self.work, &self.project)?;
            let id = crate::mcp::notebook_id(&mut session, self.spec["notebook"].as_str().unwrap_or_default())?;
            for step in self.spec["steps"].as_array().into_iter().flatten() {
                let tool = step["tool"].as_str().unwrap_or_default();
                let mut args = step["args"].clone();
                args["notebook_id"] = json!(id);
                let reply = session.tool(tool, args.clone());
                let entry = match &reply {
                    Ok(r) => json!({ "tool": tool, "args": args, "ok": true, "reply": r }),
                    Err(e) => json!({ "tool": tool, "args": args, "ok": false, "reply": e }),
                };
                log.lock().unwrap().write("inject", &entry.to_string());
                reply?;
            }
            // Its runs finish before the agent goes on.
            crate::mcp::wait_until_done(&mut session, &id, Duration::from_secs(300))
        })();
        if let Err(e) = result {
            log.lock().unwrap().write("inject", &json!({ "ok": false, "reply": e }).to_string());
        }
    }
}

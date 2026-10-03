//! Each agent session's last tool results, for the app to look up when the
//! agent doesn't pass a call's result on (Cursor reports only
//! `{"success": true}`; docs/other-agents.md, work item 2).
//!
//! A result is found by the id the agent's client gave the call, when the app
//! knows it (Claude Code sends `_meta["claudecode/toolUseId"]`, the same id
//! its ACP adapter gives the tool call), else as the oldest result not yet
//! looked up of a call to the same tool with the same arguments.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::{Map, Value, json};

use crate::mcp::to_json;

/// How many results each session keeps.
const KEPT: usize = 64;

#[derive(Default)]
pub struct Results {
    sessions: Mutex<HashMap<String, VecDeque<Record>>>,
}

struct Record {
    call_id: Option<String>,
    tool: String,
    arguments: String,
    content: Value,
    is_error: bool,
    claimed: bool,
}

impl Results {
    /// Keep `result` (a `tools/call` result) of session `owner`'s call.
    pub fn record(&self, owner: &str, call_id: Option<&str>, tool: &str, arguments: &Value, result: &Value) {
        let content: Vec<Value> = result["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "text").cloned().collect();
        let record = Record {
            call_id: call_id.map(str::to_owned),
            tool: tool.to_owned(),
            arguments: canonical(arguments),
            content: Value::Array(content),
            is_error: result["isError"] == true,
            claimed: false,
        };
        let mut sessions = self.sessions.lock().unwrap();
        let kept = sessions.entry(owner.to_owned()).or_default();
        kept.push_back(record);
        while kept.len() > KEPT {
            kept.pop_front();
        }
    }

    /// `endeavor/tool_result`: `{content, isError}` of session `owner`'s call
    /// with this id, else of its oldest call to `tool` with `arguments` not yet
    /// looked up; null if there's none.
    pub fn find(&self, owner: &str, call_id: Option<&str>, tool: &str, arguments: &Value) -> Value {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(kept) = sessions.get_mut(owner) else { return Value::Null };
        let by_id = call_id.and_then(|id| kept.iter().position(|r| r.call_id.as_deref() == Some(id)));
        let arguments = canonical(arguments);
        let found = by_id.or_else(|| kept.iter().position(|r| !r.claimed && r.tool == tool && r.arguments == arguments));
        let Some(record) = found.map(|at| &mut kept[at]) else { return Value::Null };
        record.claimed = true;
        json!({ "content": record.content, "isError": record.is_error })
    }
}

/// Arguments as one string, the same however the client wrote them: keys in
/// order, and a whole number the same whether written `1` or `1.0`.
fn canonical(arguments: &Value) -> String {
    fn normal(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), normal(v))).collect::<Map<_, _>>()),
            Value::Array(items) => Value::Array(items.iter().map(normal).collect()),
            Value::Number(n) => match n.as_f64() {
                Some(x) if n.is_f64() && x.fract() == 0.0 && x.abs() < 9.0e15 => json!(x as i64),
                _ => value.clone(),
            },
            other => other.clone(),
        }
    }
    to_json(&normal(arguments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(text: &str, is_error: bool) -> Value {
        json!({ "content": [{ "type": "text", "text": text }, { "type": "image", "data": "iVBOR", "mimeType": "image/png" }], "isError": is_error })
    }

    #[test]
    fn a_result_is_found_by_its_call_id_else_by_tool_and_arguments() {
        let results = Results::default();
        results.record("7", Some("toolu_1"), "edit_cell", &json!({ "cell_id": "a", "code": "x = 1" }), &reply("first", false));
        results.record("7", None, "edit_cell", &json!({ "code": "x = 1", "cell_id": "a" }), &reply("second", true));
        results.record("8", None, "read_cell", &json!({ "cell_id": "a" }), &reply("other session", false));

        assert_eq!(results.find("7", Some("toolu_1"), "edit_cell", &json!({})), json!({ "content": [{ "type": "text", "text": "first" }], "isError": false }));
        // Matched by tool and arguments, in any key order: the oldest not yet looked up.
        let edit = json!({ "cell_id": "a", "code": "x = 1" });
        assert_eq!(results.find("7", Some("cursor-call-9"), "edit_cell", &edit), json!({ "content": [{ "type": "text", "text": "second" }], "isError": true }));
        assert_eq!(results.find("7", None, "edit_cell", &edit), Value::Null, "each is found once by its arguments");
        assert_eq!(results.find("7", None, "read_cell", &json!({ "cell_id": "a" })), Value::Null, "another session's");
        assert_eq!(results.find("9", None, "read_cell", &json!({ "cell_id": "a" })), Value::Null);
    }

    #[test]
    fn whole_numbers_match_however_they_are_written_and_only_the_last_64_are_kept() {
        let results = Results::default();
        results.record("7", None, "move_cell", &json!({ "index": 2.0 }), &reply("moved", false));
        assert_eq!(results.find("7", None, "move_cell", &json!({ "index": 2 }))["content"][0]["text"], "moved");

        for i in 0..65 {
            results.record("7", Some(&format!("t{i}")), "read_cell", &json!({ "i": i }), &reply(&i.to_string(), false));
        }
        assert_eq!(results.find("7", Some("t0"), "read_cell", &json!({ "i": 0 })), Value::Null, "dropped");
        assert_eq!(results.find("7", Some("t1"), "read_cell", &json!({}))["content"][0]["text"], "1");
    }
}

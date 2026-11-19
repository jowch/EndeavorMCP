//! The user's own OK for a large R download, asked by the front itself (#285). An R notebook's
//! install stops before a large Bioconductor download until `approve_install`, which stands for the
//! user's yes. The app holds that call for the user in every mode; the plugin has no app, and the
//! agent was trusted to ask first. Haiku 4.5 often didn't ("Let me approve that for you"). So where
//! the client can show a form (MCP elicitation), the front asks the user before it passes the call
//! on, and nothing downloads unless they say yes there. A client that can't show one gets the call
//! as before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

use crate::mcp::{to_json, tool_error};

/// How long the front waits for the user's answer before it gives up and downloads nothing.
const ANSWER_WAIT: Duration = Duration::from_secs(30 * 60);

/// The start of the ids of the front's own requests to the client, which its answers carry back.
const ID_PREFIX: &str = "endeavor-consent-";

#[derive(Default)]
pub(super) struct Consent {
    /// The client said in `initialize` that it can show a form.
    can_ask: AtomicBool,
    next: AtomicU64,
    /// The questions out to the client, by request id.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    /// What each notebook's install would download, from the last reply that said it was asking.
    downloads: Mutex<HashMap<String, String>>,
}

/// What the user said.
pub(super) enum Asked {
    /// They said yes: pass the call on.
    Yes,
    /// They weren't asked: the client can't show a form.
    Unasked,
    /// The call's result: nothing downloads.
    No(Value),
}

impl Consent {
    /// From the client's `initialize`: whether it can show a form. An empty `elicitation` is the form
    /// alone (spec 2025-06-18); a later client lists `form` among its kinds.
    pub(super) fn learn_client(&self, initialize: &Value) {
        let elicitation = &initialize["params"]["capabilities"]["elicitation"];
        let form = elicitation.as_object().is_some_and(|kinds| kinds.is_empty() || kinds.contains_key("form"));
        self.can_ask.store(form, Ordering::SeqCst);
    }

    /// If `message` is the client's answer to one of the front's questions, hand it to the call that
    /// waits for it. Whether it was one.
    pub(super) fn take_answer(&self, message: &Value) -> bool {
        if message.get("method").is_some() {
            return false;
        }
        let Some(id) = message["id"].as_str().filter(|id| id.starts_with(ID_PREFIX)) else { return false };
        if let Some(waiting) = self.waiting.lock().unwrap().remove(id) {
            let _ = waiting.send(message.clone());
        }
        true
    }

    /// From a runtime's reply to the agent's `call`: what a notebook's install would download, when the
    /// reply says it waits for an OK.
    pub(super) fn learn_reply(&self, call: &Value, reply: &str) {
        let Some(id) = call["params"]["arguments"]["notebook_id"].as_str() else { return };
        if !reply.contains("asking") {
            return;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(reply) else { return };
        let Some(fields) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str::<Value>(text).ok()) else { return };
        if fields["packages"]["step"] == "asking" {
            self.downloads.lock().unwrap().insert(id.to_owned(), what(&fields["packages"]["download"]));
        }
    }

    /// Before `approve_install` reaches the runtime: ask the user, through `write`, whether to download.
    pub(super) fn ask(&self, call: &Value, write: &dyn Fn(&str)) -> Asked {
        if !self.can_ask.load(Ordering::SeqCst) {
            return Asked::Unasked;
        }
        let notebook = call["params"]["arguments"]["notebook_id"].as_str().unwrap_or_default();
        let known = self.downloads.lock().unwrap().get(notebook).cloned();
        let what = known.unwrap_or_else(|| "some large Bioconductor data packages".to_owned());
        let id = format!("{ID_PREFIX}{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (answered, answer) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id.clone(), answered);
        let question = format!("R needs to download {what} before this notebook's cells can run. Download it now?");
        write(&to_json(&json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": { "message": question, "requestedSchema": { "type": "object", "properties": {} } },
        })));
        let answer = answer.recv_timeout(ANSWER_WAIT).ok();
        self.waiting.lock().unwrap().remove(&id);
        let action = answer.as_ref().and_then(|a| a["result"]["action"].as_str()).unwrap_or_default().to_owned();
        if action == "accept" {
            self.downloads.lock().unwrap().remove(notebook);
            return Asked::Yes;
        }
        let why = match (action.as_str(), &answer) {
            ("decline", _) => "download_declined::The user was asked in Endeavor's own dialog and said no, so nothing downloads and the cells that need it stay queued. Tell the user; call approve_install again only if they ask for the download.".to_owned(),
            ("cancel", _) => "download_not_answered::The user closed Endeavor's dialog without answering, so nothing downloads and the cells that need it stay queued. Ask the user in the chat; if they want it, call approve_install again, and it asks them once more.".to_owned(),
            (_, None) => "download_not_answered::Endeavor asked the user in a dialog and got no answer, so nothing downloads. Ask the user in the chat; if they want it, call approve_install again, and it asks them once more.".to_owned(),
            (_, Some(answer)) => format!(
                "download_not_answered::Endeavor couldn't ask the user ({}), so nothing downloads. Ask the user in the chat; if they want it, call approve_install again.",
                answer["error"]["message"].as_str().unwrap_or("the client gave no answer")
            ),
        };
        Asked::No(tool_error(&why, false))
    }
}

/// What a download lists, as the runtime's asking message says it: "ALL 1.44.0 (11 MB)", and the
/// sum when there are several.
fn what(download: &Value) -> String {
    let sized = |bytes: &Value| bytes.as_f64().map(|b| if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else { format!("{:.0} MB", b / 1e6) });
    let each: Vec<String> = download["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let name = [p["package"].as_str(), p["version"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(" ");
            format!("{name} ({})", sized(&p["bytes"]).unwrap_or_else(|| "size unknown".into()))
        })
        .collect();
    if each.is_empty() {
        return "some large Bioconductor data packages".to_owned();
    }
    let total = if each.len() > 1 { sized(&download["bytes"]).map(|size| format!(", {size} in all")).unwrap_or_default() } else { String::new() };
    format!("{}{total}", each.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn initialize(elicitation: Value) -> Value {
        json!({ "method": "initialize", "params": { "capabilities": { "elicitation": elicitation } } })
    }

    fn call() -> Value {
        json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "approve_install", "arguments": { "notebook_id": "nb" } } })
    }

    /// Ask, with the client answering `answer` (or nothing) to whatever it is sent; what it was sent.
    fn asked(consent: Arc<Consent>, answer: Option<Value>) -> (Asked, Vec<Value>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (to_client, client) = mpsc::channel::<String>();
        let answering = {
            let consent = consent.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                for line in client {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    sent.lock().unwrap().push(request.clone());
                    if let Some(answer) = &answer {
                        assert!(consent.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer })));
                    } else {
                        assert!(consent.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": -32601, "message": "Method not found" } })));
                    }
                }
            })
        };
        let result = consent.ask(&call(), &|line| to_client.send(line.to_owned()).unwrap());
        drop(to_client);
        answering.join().unwrap();
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    #[test]
    fn a_client_that_can_show_a_form_is_asked_before_a_download() {
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({ "form": {}, "url": {} })));
        let reply = json!({ "jsonrpc": "2.0", "id": 8, "result": { "content": [{ "type": "text", "text": to_json(&json!({ "packages": { "step": "asking", "download": { "bytes": 11385380, "packages": [{ "package": "ALL", "version": "1.44.0", "bytes": 11385380 }] } } })) }] } });
        consent.learn_reply(&json!({ "params": { "arguments": { "notebook_id": "nb" } } }), &to_json(&reply));
        let (result, sent) = asked(consent.clone(), Some(json!({ "action": "accept", "content": {} })));
        assert!(matches!(result, Asked::Yes));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "elicitation/create");
        assert_eq!(sent[0]["params"]["message"], "R needs to download ALL 1.44.0 (11 MB) before this notebook's cells can run. Download it now?");
        // A no, a closed dialog or an error downloads nothing, and the agent hears which.
        for (answer, kind) in [(Some(json!({ "action": "decline" })), "said no"), (Some(json!({ "action": "cancel" })), "without answering"), (None, "Method not found")] {
            let (result, _) = asked(consent.clone(), answer);
            let Asked::No(result) = result else { panic!("{kind}: not refused") };
            assert_eq!(result["isError"], true, "{result}");
            assert!(result["content"][0]["text"].as_str().unwrap().contains(kind), "{result}");
        }
        // Not knowing what it would download, it still asks.
        let (_, sent) = asked(consent, Some(json!({ "action": "decline" })));
        assert!(sent[0]["params"]["message"].as_str().unwrap().contains("some large Bioconductor data packages"));
    }

    #[test]
    fn a_client_that_cant_show_one_gets_the_call_as_before() {
        for elicitation in [Value::Null, json!({ "url": {} })] {
            let consent = Consent::default();
            consent.learn_client(&initialize(elicitation));
            assert!(matches!(consent.ask(&call(), &|_| panic!("asked")), Asked::Unasked));
        }
        // An empty object is the form, as 2025-06-18 has it.
        let consent = Consent::default();
        consent.learn_client(&initialize(json!({})));
        assert!(consent.can_ask.load(Ordering::SeqCst));
        // An answer that isn't to one of its questions is left for the runtime.
        assert!(!consent.take_answer(&json!({ "jsonrpc": "2.0", "id": 3, "result": {} })));
        assert!(!consent.take_answer(&json!({ "jsonrpc": "2.0", "id": "endeavor-consent-1", "method": "x" })));
    }

    #[test]
    fn several_packages_say_their_sum() {
        let download = json!({ "bytes": 1.3e9, "packages": [{ "package": "GO.db", "version": "3.22.0", "bytes": 4e8 }, { "package": "BSgenome.Hsapiens.UCSC.hg38", "version": "1.4.5", "bytes": 8.7e8 }, { "package": "org.Hs.eg.db" }] });
        assert_eq!(what(&download), "GO.db 3.22.0 (400 MB), BSgenome.Hsapiens.UCSC.hg38 1.4.5 (870 MB), org.Hs.eg.db (size unknown), 1.3 GB in all");
    }
}

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

/// What `approve_install`'s description says of the user's yes where no dialog follows: the agent
/// asks in the chat.
const ASK_IN_CHAT: &str = "It doesn't ask the user: calling it is the user's yes. Ask them first, end your reply, and call it only once they have said yes in a later message; never on your own.";

/// The same where the front asks the user itself: asking in the chat too would have them say yes twice.
const ASKED_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that names what downloads and its size, and nothing downloads unless they say yes there. Tell the user it needs their OK, then call it; don't ask in the chat first. If they say no or close the dialog, the call fails with `download_declined` or `download_not_answered`.";

#[derive(Default)]
pub(super) struct Consent {
    /// The client said in `initialize` that it can show a form.
    can_ask: AtomicBool,
    next: AtomicU64,
    /// The questions out to the client, by request id.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    /// The question each waiting call asked, by the call's id (as JSON), for a cancel of the call.
    calls: Mutex<HashMap<String, String>>,
}

/// What a notebook's install waits for, as the runtime's `list_notebooks` says it.
pub(super) enum Waiting {
    /// The user's OK for this download (`packages.download`).
    Asking(Value),
    /// Nothing: the runtime refuses the call, and no one need be asked.
    Nothing,
    /// The front couldn't tell.
    Unknown,
}

/// What the user said.
pub(super) enum Asked {
    /// They said yes: pass the call on.
    Yes,
    /// They weren't asked: the client can't show a form, or nothing waits for an OK.
    Unasked,
    /// The call's result: nothing downloads.
    No(Value),
    /// The client cancelled the call: nothing downloads, and the call gets no reply.
    Dropped,
}

impl Consent {
    /// From the client's `initialize`: whether it can show a form. An empty `elicitation` is the form
    /// alone (spec 2025-06-18); a later client lists `form` among its kinds.
    pub(super) fn learn_client(&self, initialize: &Value) {
        let elicitation = &initialize["params"]["capabilities"]["elicitation"];
        let form = elicitation.as_object().is_some_and(|kinds| kinds.is_empty() || kinds.contains_key("form"));
        self.can_ask.store(form, Ordering::SeqCst);
    }

    pub(super) fn can_ask(&self) -> bool {
        self.can_ask.load(Ordering::SeqCst)
    }

    /// `tools/list`'s reply, with `approve_install` saying that Endeavor asks the user, where it does.
    pub(super) fn tools_listed(&self, reply: String) -> String {
        if !self.can_ask() {
            return reply;
        }
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        let Some(tools) = parsed["result"]["tools"].as_array_mut() else { return reply };
        for tool in tools.iter_mut().filter(|tool| tool["name"] == "approve_install") {
            if let Some(text) = tool["description"].as_str() {
                tool["description"] = text.replace(ASK_IN_CHAT, ASKED_IN_DIALOG).into();
            }
        }
        to_json(&parsed)
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

    /// If `message` cancels a call that waits for the user's answer, end the wait: nothing downloads.
    /// Whether it did; the runtime never saw that call.
    pub(super) fn take_cancel(&self, message: &Value) -> bool {
        if message["method"] != "notifications/cancelled" {
            return false;
        }
        let Some(question) = self.calls.lock().unwrap().remove(&to_json(&message["params"]["requestId"])) else { return false };
        if let Some(waiting) = self.waiting.lock().unwrap().remove(&question) {
            let _ = waiting.send(json!({ "cancelled": true }));
        }
        true
    }

    /// Before `approve_install` reaches the runtime: ask the user, through `write`, whether to make
    /// the download `waiting`.
    pub(super) fn ask(&self, call: &Value, waiting: Waiting, write: &dyn Fn(&str)) -> Asked {
        if !self.can_ask() {
            return Asked::Unasked;
        }
        let what = match &waiting {
            Waiting::Nothing => return Asked::Unasked,
            Waiting::Asking(download) => what(download),
            Waiting::Unknown => "some large Bioconductor data packages".to_owned(),
        };
        let id = format!("{ID_PREFIX}{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let call_id = to_json(&call["id"]);
        let (answered, answer) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id.clone(), answered);
        self.calls.lock().unwrap().insert(call_id.clone(), id.clone());
        let question = format!("R needs to download {what} before this notebook's cells can run. Download it now?");
        write(&to_json(&json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": { "message": question, "requestedSchema": { "type": "object", "properties": {} } },
        })));
        let answer = answer.recv_timeout(ANSWER_WAIT).ok();
        self.waiting.lock().unwrap().remove(&id);
        self.calls.lock().unwrap().remove(&call_id);
        let action = answer.as_ref().and_then(|a| a["result"]["action"].as_str()).unwrap_or_default().to_owned();
        if action == "accept" {
            return Asked::Yes;
        }
        // A question no answer is coming for any more: the client may take its dialog down.
        let withdraw = || write(&to_json(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })));
        let why = match (action.as_str(), &answer) {
            (_, Some(answer)) if answer["cancelled"] == true => {
                withdraw();
                return Asked::Dropped;
            }
            ("decline", _) => "download_declined::The user was asked in Endeavor's own dialog and said no, so nothing downloads and the cells that need it stay queued. Tell the user; call approve_install again only if they ask for the download.".to_owned(),
            ("cancel", _) => "download_not_answered::The user closed Endeavor's dialog without answering, so nothing downloads and the cells that need it stay queued. Ask the user in the chat; if they want it, call approve_install again, and it asks them once more.".to_owned(),
            (_, None) => {
                withdraw();
                "download_not_answered::Endeavor asked the user in a dialog and got no answer, so nothing downloads. Ask the user in the chat; if they want it, call approve_install again, and it asks them once more.".to_owned()
            }
            (_, Some(answer)) => format!(
                "download_not_answered::Endeavor couldn't ask the user ({}), so nothing downloads. Ask the user in the chat; if they want it, call approve_install again.",
                answer["error"]["message"].as_str().unwrap_or("the client gave no answer")
            ),
        };
        Asked::No(tool_error(&why, false))
    }
}

/// What `list_notebooks`' reply says `notebook`'s install waits for. A notebook it doesn't list, or
/// one in safe preview, waits for nothing the call could give: the runtime refuses it.
pub(super) fn waiting_in(listed: &str, notebook: &str) -> Waiting {
    let Ok(parsed) = serde_json::from_str::<Value>(listed) else { return Waiting::Unknown };
    if parsed["result"]["isError"] == true {
        return Waiting::Unknown;
    }
    let Some(fields) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str::<Value>(text).ok()) else { return Waiting::Unknown };
    let Some(notebooks) = fields.as_array().or_else(|| fields["notebooks"].as_array()) else { return Waiting::Unknown };
    let Some(nb) = notebooks.iter().find(|nb| nb["notebook_id"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(notebook.trim()))) else { return Waiting::Nothing };
    if nb["packages"]["step"] == "asking" && nb["execution_allowed"] != false { Waiting::Asking(nb["packages"]["download"].clone()) } else { Waiting::Nothing }
}

/// What a download lists, as the runtime's asking message says it: "ALL 1.44.0 (11 MB)", and the
/// sum when there are several.
fn what(download: &Value) -> String {
    let sized = |bytes: &Value| {
        bytes.as_f64().map(|b| match b {
            b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
            b if b >= 1e6 => format!("{:.0} MB", b / 1e6),
            _ => "under 1 MB".to_owned(),
        })
    };
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

    fn all() -> Waiting {
        Waiting::Asking(json!({ "bytes": 11385380, "packages": [{ "package": "ALL", "version": "1.44.0", "bytes": 11385380 }] }))
    }

    /// Ask, with the client answering `answer` (or an error) to whatever it is sent; what it was sent.
    fn asked(consent: Arc<Consent>, waiting: Waiting, answer: Option<Value>) -> (Asked, Vec<Value>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (to_client, client) = mpsc::channel::<String>();
        let answering = {
            let consent = consent.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                for line in client {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    sent.lock().unwrap().push(request.clone());
                    if request.get("id").is_none() {
                        continue;
                    }
                    let reply = match &answer {
                        Some(answer) => json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer }),
                        None => json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": -32601, "message": "Method not found" } }),
                    };
                    assert!(consent.take_answer(&reply));
                }
            })
        };
        let result = consent.ask(&call(), waiting, &|line| to_client.send(line.to_owned()).unwrap());
        drop(to_client);
        answering.join().unwrap();
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    #[test]
    fn a_client_that_can_show_a_form_is_asked_before_a_download() {
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({ "form": {}, "url": {} })));
        let (result, sent) = asked(consent.clone(), all(), Some(json!({ "action": "accept", "content": {} })));
        assert!(matches!(result, Asked::Yes));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "elicitation/create");
        assert_eq!(sent[0]["params"]["message"], "R needs to download ALL 1.44.0 (11 MB) before this notebook's cells can run. Download it now?");
        // A no, a closed dialog or an error downloads nothing, and the agent hears which.
        for (answer, kind) in [(Some(json!({ "action": "decline" })), "said no"), (Some(json!({ "action": "cancel" })), "without answering"), (None, "Method not found")] {
            let (result, _) = asked(consent.clone(), all(), answer);
            let Asked::No(result) = result else { panic!("{kind}: not refused") };
            assert_eq!(result["isError"], true, "{result}");
            assert!(result["content"][0]["text"].as_str().unwrap().contains(kind), "{result}");
        }
        // Not knowing what it would download, it still asks; with nothing waiting, it doesn't.
        let (_, sent) = asked(consent.clone(), Waiting::Unknown, Some(json!({ "action": "decline" })));
        assert!(sent[0]["params"]["message"].as_str().unwrap().contains("some large Bioconductor data packages"));
        let (result, sent) = asked(consent, Waiting::Nothing, None);
        assert!(matches!(result, Asked::Unasked) && sent.is_empty());
    }

    #[test]
    fn a_cancelled_call_stops_waiting_and_downloads_nothing() {
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({})));
        let (to_client, client) = mpsc::channel::<String>();
        let asking = {
            let consent = consent.clone();
            std::thread::spawn(move || consent.ask(&call(), all(), &|line| to_client.send(line.to_owned()).unwrap()))
        };
        let question: Value = serde_json::from_str(&client.recv().unwrap()).unwrap();
        assert!(!consent.take_cancel(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 8 } })), "another call's cancel is the runtime's");
        assert!(consent.take_cancel(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 9 } })));
        assert!(matches!(asking.join().unwrap(), Asked::Dropped));
        // The dialog is withdrawn, and a yes to it after that goes nowhere.
        let withdrawn: Value = serde_json::from_str(&client.recv().unwrap()).unwrap();
        assert_eq!((&withdrawn["method"], &withdrawn["params"]["requestId"]), (&json!("notifications/cancelled"), &question["id"]));
        assert!(consent.take_answer(&json!({ "jsonrpc": "2.0", "id": question["id"], "result": { "action": "accept" } })));
    }

    #[test]
    fn a_client_that_cant_show_one_gets_the_call_as_before() {
        for elicitation in [Value::Null, json!({ "url": {} })] {
            let consent = Consent::default();
            consent.learn_client(&initialize(elicitation));
            assert!(matches!(consent.ask(&call(), all(), &|_| panic!("asked")), Asked::Unasked));
            let listed = to_json(&json!({ "result": { "tools": [{ "name": "approve_install", "description": ASK_IN_CHAT }] } }));
            assert_eq!(consent.tools_listed(listed.clone()), listed);
        }
        // An empty object is the form, as 2025-06-18 has it.
        let consent = Consent::default();
        consent.learn_client(&initialize(json!({})));
        assert!(consent.can_ask());
        // An answer that isn't to one of its questions is left for the runtime.
        assert!(!consent.take_answer(&json!({ "jsonrpc": "2.0", "id": 3, "result": {} })));
        assert!(!consent.take_answer(&json!({ "jsonrpc": "2.0", "id": "endeavor-consent-1", "method": "x" })));
    }

    #[test]
    fn where_it_asks_the_tool_says_not_to_ask_in_the_chat_too() {
        let consent = Consent::default();
        consent.learn_client(&initialize(json!({ "form": {} })));
        let tools: Value = serde_json::from_str(crate::mcp::NOTEBOOK_TOOLS_JSON).unwrap();
        let listed = to_json(&json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": tools } }));
        let shown: Value = serde_json::from_str(&consent.tools_listed(listed)).unwrap();
        let approve = shown["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "approve_install").unwrap();
        let text = approve["description"].as_str().unwrap();
        assert!(text.contains(ASKED_IN_DIALOG) && !text.contains(ASK_IN_CHAT), "{text}");
    }

    #[test]
    fn the_runtimes_list_says_what_waits() {
        let listed = |notebooks: Value| to_json(&json!({ "jsonrpc": "2.0", "id": "x", "result": { "content": [{ "type": "text", "text": to_json(&notebooks) }] } }));
        let asking = json!({ "step": "asking", "download": { "bytes": 5 } });
        let nb = |packages: Value, allowed: bool| json!({ "notebook_id": "AB-12", "execution_allowed": allowed, "packages": packages });
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), "ab-12"), Waiting::Asking(download) if download["bytes"] == 5));
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), false)])), "AB-12"), Waiting::Nothing), "safe preview");
        assert!(matches!(waiting_in(&listed(json!([nb(json!({ "step": "installing" }), true)])), "AB-12"), Waiting::Nothing));
        assert!(matches!(waiting_in(&listed(json!([nb(asking, true)])), "CD-34"), Waiting::Nothing), "another notebook");
        assert!(matches!(waiting_in("ran", "AB-12"), Waiting::Unknown));
    }

    #[test]
    fn several_packages_say_their_sum() {
        let download = json!({ "bytes": 1.3e9, "packages": [{ "package": "GO.db", "version": "3.22.0", "bytes": 4e8 }, { "package": "BSgenome.Hsapiens.UCSC.hg38", "version": "1.4.5", "bytes": 8.7e8 }, { "package": "org.Hs.eg.db" }, { "package": "tiny", "bytes": 3e5 }] });
        assert_eq!(what(&download), "GO.db 3.22.0 (400 MB), BSgenome.Hsapiens.UCSC.hg38 1.4.5 (870 MB), org.Hs.eg.db (size unknown), tiny (under 1 MB), 1.3 GB in all");
    }
}

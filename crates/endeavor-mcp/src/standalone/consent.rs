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

/// A call cancelled before its question went out.
const CANCELLED: &str = "cancelled";

/// What `approve_install`'s description says of the user's yes where no dialog follows: the agent
/// asks in the chat.
const ASK_IN_CHAT: &str = "It doesn't ask the user: calling it is the user's yes. Ask them first, end your reply, and call it only once they have said yes in a later message; never on your own.";

/// The same where the front asks the user itself: asking in the chat too would have them say yes twice.
const ASKED_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that the app they talk to you in shows, which names what downloads and its size, and nothing downloads unless they say yes there or with the button on the notebook's page. Tell the user it needs their OK, then call it; don't ask in the chat first. If they say no or close the dialog, the call fails with `download_declined` or `download_not_answered`.";

/// The runtime's asking step (`notebooks::ASK_STEP`) where the front asks the user itself.
const ASK_STEP_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that the app they talk to you in shows, which names what downloads and its size: tell the user it needs their OK, then call approve_install now; don't ask in the chat first. The notebook's page also has a button for it.";

/// The machine tools' words about asking the user before `install: true`, and what they say where
/// the front asks the user itself (#378). Each chat wording is in machine_tools.json (a test checks).
const MACHINE_SWAPS: &[(&str, &str)] = &[
    (
        "ask the user, and call again with `install` true only if they agree",
        "Endeavor asks the user itself, in a dialog that names it, when you call again with `install` true, so tell the user it needs their OK and call again; don't ask in the chat first",
    ),
    (
        "`needs_install`: ask the user, and call again with `install` true only if they agree to what the result names",
        "`needs_install`: tell the user it needs their OK and call again with `install` true; Endeavor asks the user itself, in a dialog that names what it installs, so don't ask in the chat first",
    ),
    (
        "the result is `needs_install`: ask the user, and set `install` only if they agree",
        "the result is `needs_install`: tell the user it needs their OK and call again with `install` true; Endeavor asks the user itself in a dialog, so don't ask in the chat first",
    ),
    ("Only after the user agreed to what `needs_install` named; never on a first call.", "Endeavor asks the user in a dialog first and installs only on their yes."),
    ("Only after the user agreed; this call only; never on a first call.", "Endeavor asks the user in a dialog first and installs only on their yes; this call only."),
];

#[derive(Default)]
pub(super) struct Consent {
    /// The client said in `initialize` that it can show a form.
    can_ask: AtomicBool,
    next: AtomicU64,
    /// The questions out to the client, by request id.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    /// The question each waiting call asked, by the call's id (as JSON), for a cancel of the call;
    /// none yet while the front looks at what it would download.
    calls: Mutex<HashMap<String, Option<String>>>,
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
        for tool in tools.iter_mut() {
            let swaps: &[(&str, &str)] = match tool["name"].as_str() {
                Some("approve_install") => &[(ASK_IN_CHAT, ASKED_IN_DIALOG)],
                Some("add_machine" | "use_machine" | "stop_machine") => MACHINE_SWAPS,
                _ => continue,
            };
            for pointer in ["/description", "/inputSchema/properties/install/description"] {
                if let Some(field) = tool.pointer_mut(pointer)
                    && let Some(text) = field.as_str()
                {
                    *field = swaps.iter().fold(text.to_owned(), |text, (chat, dialog)| text.replace(chat, dialog)).into();
                }
            }
        }
        to_json(&parsed)
    }

    /// A runtime's reply to a call, with its asking message telling the agent that Endeavor asks the
    /// user, where it does. The step's words need no escaping, so they read the same in the reply's
    /// JSON. A runtime from another build whose words differ keeps its own: the user is asked twice.
    pub(super) fn reply_said(&self, reply: String) -> String {
        if !self.can_ask() || !reply.contains(crate::notebooks::ASK_STEP) {
            return reply;
        }
        reply.replace(crate::notebooks::ASK_STEP, ASK_STEP_IN_DIALOG)
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

    /// `call` is on its way to `ask`: a cancel from now on is the front's, not the runtime's.
    pub(super) fn looking(&self, call: &Value) {
        self.calls.lock().unwrap().insert(to_json(&call["id"]), None);
    }

    /// If `message` cancels a call that waits for the user's answer, end the wait: nothing downloads.
    /// Whether it did; the runtime never saw that call.
    pub(super) fn take_cancel(&self, message: &Value) -> bool {
        if message["method"] != "notifications/cancelled" {
            return false;
        }
        let mut calls = self.calls.lock().unwrap();
        let Some(question) = calls.get_mut(&to_json(&message["params"]["requestId"])) else { return false };
        match question.as_deref() {
            Some(CANCELLED) => {}
            Some(asked) => {
                if let Some(waiting) = self.waiting.lock().unwrap().remove(asked) {
                    let _ = waiting.send(json!({ "cancelled": true }));
                }
            }
            // Cancelled while the front looks: `ask` finds it marked and asks nothing.
            None => *question = Some(CANCELLED.to_owned()),
        }
        true
    }

    /// Before `approve_install` reaches the runtime: ask the user, through `write`, whether to make
    /// the download `waiting`.
    pub(super) fn ask(&self, call: &Value, waiting: Waiting, write: &dyn Fn(&str)) -> Asked {
        let what = match &waiting {
            _ if !self.can_ask() => None,
            Waiting::Nothing => None,
            Waiting::Asking(download) => Some(what(download)),
            Waiting::Unknown => Some("some large Bioconductor data packages".to_owned()),
        };
        let question = what.map(|what| format!("R needs to download {what} before this notebook's cells can run. Download it now?"));
        let why = match self.question(call, question.as_deref(), write) {
            Answer::Yes => return Asked::Yes,
            Answer::Unasked => return Asked::Unasked,
            Answer::Dropped => return Asked::Dropped,
            Answer::Declined => "download_declined::The user said no in Endeavor's download dialog, which the app they talk to you in showed, so nothing downloads and the cells that need it stay queued. Tell the user; call approve_install again only if they ask for the download.".to_owned(),
            Answer::Closed => "download_not_answered::The user closed Endeavor's download dialog without answering. That dialog was in the app they talk to you in, and it is gone now. Nothing downloads and the cells that need it stay queued. Ask the user in the chat whether to download it; if they say yes, call approve_install again, and the same dialog asks them once more. The notebook's page also has its own button that approves it.".to_owned(),
            Answer::TimedOut => "download_not_answered::Endeavor asked the user in a dialog in the app they talk to you in and got no answer, so nothing downloads. Ask the user in the chat; if they want it, call approve_install again, and it asks them once more.".to_owned(),
            Answer::Failed(error) => format!("download_not_answered::Endeavor couldn't ask the user ({error}), so nothing downloads. Ask the user in the chat; if they want it, call approve_install again."),
        };
        Asked::No(tool_error(&why, false))
    }

    /// Before a machine tool's `install: true` goes ahead (#378): ask the user, through `write`,
    /// `question`, which names what it would install. `again` is how the agent calls it again.
    /// The call goes ahead on `Ok(true)`, gets no reply on `Ok(false)` (the client cancelled it),
    /// and on `Err` installs nothing, with the `kind::message` it fails with.
    pub(super) fn ask_install(&self, call: &Value, question: &str, again: &str, write: &dyn Fn(&str)) -> Result<bool, String> {
        Err(match self.question(call, Some(question), write) {
            Answer::Yes | Answer::Unasked => return Ok(true),
            Answer::Dropped => return Ok(false),
            Answer::Declined => format!("install_declined::The user said no in Endeavor's install dialog, which the app they talk to you in showed, so nothing was installed. Tell the user; call {again} with `install: true` only if they ask for it."),
            Answer::Closed => format!("install_not_answered::The user closed Endeavor's install dialog without answering. That dialog was in the app they talk to you in, and it is gone now. Nothing was installed. Ask the user in the chat whether Endeavor may install it; if they say yes, call {again} with `install: true`, and the same dialog asks them once more."),
            Answer::TimedOut => format!("install_not_answered::Endeavor asked the user in a dialog in the app they talk to you in and got no answer, so nothing was installed. Ask the user in the chat; if they want it, call {again} with `install: true`, and it asks them once more."),
            Answer::Failed(error) => format!("install_not_answered::Endeavor couldn't ask the user ({error}), so nothing was installed. Ask the user in the chat; if they want it, call {again} with `install: true`."),
        })
    }

    /// `call` was on its way to `ask` or `ask_install` (`looking`), and nothing needs asking: forget
    /// it. Whether the client cancelled it meanwhile.
    pub(super) fn not_asking(&self, call: &Value) -> bool {
        self.calls.lock().unwrap().remove(&to_json(&call["id"])).is_some_and(|question| question.as_deref() == Some(CANCELLED))
    }

    /// Ask the user `question` through `write`, for `call`; none, or a client that can't show a
    /// form, asks nothing.
    fn question(&self, call: &Value, question: Option<&str>, write: &dyn Fn(&str)) -> Answer {
        let call_id = to_json(&call["id"]);
        // Under the calls' lock from the check to the question's entry, so that a cancel either marks
        // the call or finds its question.
        let mut calls = self.calls.lock().unwrap();
        if calls.get(&call_id).is_some_and(|question| question.as_deref() == Some(CANCELLED)) {
            calls.remove(&call_id);
            return Answer::Dropped;
        }
        let Some(question) = question.filter(|_| self.can_ask()) else {
            // The call goes on, and so does a cancel of it.
            calls.remove(&call_id);
            return Answer::Unasked;
        };
        let id = format!("{ID_PREFIX}{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (answered, answer) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id.clone(), answered);
        calls.insert(call_id.clone(), Some(id.clone()));
        drop(calls);
        write(&to_json(&json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": { "message": question, "requestedSchema": { "type": "object", "properties": {} } },
        })));
        let answer = answer.recv_timeout(ANSWER_WAIT).ok();
        self.waiting.lock().unwrap().remove(&id);
        self.calls.lock().unwrap().remove(&call_id);
        let action = answer.as_ref().and_then(|a| a["result"]["action"].as_str()).unwrap_or_default().to_owned();
        if action == "accept" {
            return Answer::Yes;
        }
        // A question no answer is coming for any more: the client may take its dialog down.
        let withdraw = || write(&to_json(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })));
        match (action.as_str(), &answer) {
            (_, Some(answer)) if answer["cancelled"] == true => {
                withdraw();
                Answer::Dropped
            }
            ("decline", _) => Answer::Declined,
            ("cancel", _) => Answer::Closed,
            (_, None) => {
                withdraw();
                Answer::TimedOut
            }
            (_, Some(answer)) => Answer::Failed(answer["error"]["message"].as_str().unwrap_or("the client gave no answer").to_owned()),
        }
    }
}

/// How the user answered one of the front's questions.
enum Answer {
    Yes,
    /// Nothing was asked.
    Unasked,
    /// The client cancelled the call meanwhile.
    Dropped,
    Declined,
    /// They closed the dialog.
    Closed,
    /// No answer came in `ANSWER_WAIT`.
    TimedOut,
    /// The client couldn't show it, with what it said.
    Failed(String),
}

/// What `list_notebooks`' reply says `notebook`'s install waits for. A notebook it doesn't list, or
/// one in safe preview, waits for nothing the call could give: the runtime refuses it.
pub(super) fn waiting_in(listed: &str, notebook: &Value) -> Waiting {
    let Ok(parsed) = serde_json::from_str::<Value>(listed) else { return Waiting::Unknown };
    if parsed["result"]["isError"] == true {
        return Waiting::Unknown;
    }
    let Some(fields) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str::<Value>(text).ok()) else { return Waiting::Unknown };
    let Some(notebooks) = fields.as_array().or_else(|| fields["notebooks"].as_array()) else { return Waiting::Unknown };
    // The id as the runtime reads it.
    let Some(notebook) = crate::notebooks::uuid_value(notebook) else { return Waiting::Nothing };
    let Some(nb) = notebooks.iter().find(|nb| crate::notebooks::uuid_value(&nb["notebook_id"]).as_ref() == Some(&notebook)) else { return Waiting::Nothing };
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
    fn every_machine_wording_swapped_where_a_dialog_asks_is_in_the_tools() {
        for (chat, _) in MACHINE_SWAPS {
            assert!(crate::mcp::MACHINE_TOOLS_JSON.contains(chat), "machine_tools.json no longer says: {chat}");
        }
    }

    #[test]
    fn an_install_on_a_machine_is_asked_about_and_a_no_says_what_to_do() {
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({})));
        let ask = |answer: Value| {
            let consent2 = consent.clone();
            let (to_client, client) = mpsc::channel::<String>();
            let answering = std::thread::spawn(move || {
                let mut asked = None;
                for line in client {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    if request.get("id").is_some() {
                        asked = Some(request["params"]["message"].clone());
                        assert!(consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer })));
                    }
                }
                asked
            });
            let result = consent.ask_install(&call(), "Endeavor needs to install Endeavor's helper on lab. Install it now?", "`add_machine` again", &|line| to_client.send(line.to_owned()).unwrap());
            drop(to_client);
            (result, answering.join().unwrap())
        };
        let (yes, asked) = ask(json!({ "action": "accept" }));
        assert_eq!((yes, asked), (Ok(true), Some(json!("Endeavor needs to install Endeavor's helper on lab. Install it now?"))));
        let (no, _) = ask(json!({ "action": "decline" }));
        assert!(no.as_ref().is_err_and(|e| e.starts_with("install_declined::") && e.contains("`add_machine` again with `install: true` only if they ask")), "{no:?}");
        let (closed, _) = ask(json!({ "action": "cancel" }));
        assert!(closed.as_ref().is_err_and(|e| e.starts_with("install_not_answered::")), "{closed:?}");
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
        for (answer, kind) in [(Some(json!({ "action": "decline" })), "said no"), (Some(json!({ "action": "cancel" })), "without answering. That dialog was in the app they talk to you in, and it is gone now"), (None, "Method not found")] {
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
    fn a_call_cancelled_while_the_front_looks_is_never_asked_about() {
        let consent = Consent::default();
        consent.learn_client(&initialize(json!({})));
        consent.looking(&call());
        let cancel = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 9 } });
        assert!(consent.take_cancel(&cancel) && consent.take_cancel(&cancel));
        assert!(matches!(consent.ask(&call(), all(), &|_| panic!("asked")), Asked::Dropped));
        // Done with: a later cancel of the same id is the runtime's again.
        assert!(!consent.take_cancel(&cancel));
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
    fn where_it_asks_the_asking_message_says_so() {
        let reply = |text: &str| to_json(&json!({ "jsonrpc": "2.0", "id": 4, "result": { "content": [{ "type": "text", "text": to_json(&json!({ "message": text })) }] } }));
        let asking = format!("R has to download ALL. {} Don't call read_cell to wait.", crate::notebooks::ASK_STEP);
        let consent = Consent::default();
        assert_eq!(consent.reply_said(reply(&asking)), reply(&asking));
        consent.learn_client(&initialize(json!({ "form": {} })));
        let said = consent.reply_said(reply(&asking));
        assert_eq!(said, reply(&format!("R has to download ALL. {ASK_STEP_IN_DIALOG} Don't call read_cell to wait.")));
        // It says where the dialog is, and the page's button is still there.
        assert!(said.contains("The notebook's page also has a button for it.") && !said.contains("The page's own button does the same."), "{said}");
    }

    #[test]
    fn the_runtimes_list_says_what_waits() {
        let listed = |notebooks: Value| to_json(&json!({ "jsonrpc": "2.0", "id": "x", "result": { "content": [{ "type": "text", "text": to_json(&notebooks) }] } }));
        let asking = json!({ "step": "asking", "download": { "bytes": 5 } });
        let id = "1b2c3d4e-0000-4000-8000-00000000000a";
        let nb = |packages: Value, allowed: bool| json!({ "notebook_id": id, "execution_allowed": allowed, "packages": packages });
        // The id as the runtime reads it: in capitals too.
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), &json!(id.to_uppercase())), Waiting::Asking(download) if download["bytes"] == 5));
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), false)])), &json!(id)), Waiting::Nothing), "safe preview");
        assert!(matches!(waiting_in(&listed(json!([nb(json!({ "step": "installing" }), true)])), &json!(id)), Waiting::Nothing));
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), &json!("1b2c3d4e-0000-4000-8000-00000000000b")), Waiting::Nothing), "another notebook");
        assert!(matches!(waiting_in(&listed(json!([nb(asking, true)])), &json!("nb")), Waiting::Nothing), "not an id");
        assert!(matches!(waiting_in("ran", &json!(id)), Waiting::Unknown));
    }

    #[test]
    fn several_packages_say_their_sum() {
        let download = json!({ "bytes": 1.3e9, "packages": [{ "package": "GO.db", "version": "3.22.0", "bytes": 4e8 }, { "package": "BSgenome.Hsapiens.UCSC.hg38", "version": "1.4.5", "bytes": 8.7e8 }, { "package": "org.Hs.eg.db" }, { "package": "tiny", "bytes": 3e5 }] });
        assert_eq!(what(&download), "GO.db 3.22.0 (400 MB), BSgenome.Hsapiens.UCSC.hg38 1.4.5 (870 MB), org.Hs.eg.db (size unknown), tiny (under 1 MB), 1.3 GB in all");
    }
}

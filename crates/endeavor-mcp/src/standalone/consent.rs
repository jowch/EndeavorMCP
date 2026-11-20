//! The user's own OK for a large R download, asked by the front itself (#285). An R notebook's
//! install stops before a large Bioconductor download until `approve_install`, which stands for the
//! user's yes. The app holds that call for the user in every mode; the plugin has no app, and the
//! agent was trusted to ask first. Haiku 4.5 often didn't ("Let me approve that for you"). So where
//! the client can show a form (MCP elicitation), the front asks the user before it passes the call
//! on, and nothing downloads unless they say yes there. A client that can't show one has them
//! asked on a page in their browser (`install_page`, #388), and with no browser, nothing downloads
//! through the agent: the notebook page's own button is the user's way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

use crate::mcp::{to_json, tool_error};

/// How long the front waits for the user's answer before it gives up and downloads nothing.
pub(super) const ANSWER_WAIT: Duration = Duration::from_secs(30 * 60);

/// A dialog with no answer after this may not be up at all: Claude Code's `-p` takes the request
/// and never answers it (#396's smoke runs). Where a browser can be opened, the front then asks on
/// the page too, with the dialog still up, and whichever the user answers first is their answer.
/// A person answers a two-line question well within this; one who doesn't just sees the page as well.
/// A debug build takes `ENDEAVOR_TEST_DIALOG_SILENT_MS`.
fn dialog_silent() -> Duration {
    #[cfg(test)]
    if let Some(silent) = SILENT_FOR_TEST.get() {
        return silent;
    }
    super::machines::test_var("ENDEAVOR_TEST_DIALOG_SILENT_MS").and_then(|ms| ms.to_str()?.parse().ok()).map_or(Duration::from_secs(20), Duration::from_millis)
}

/// A dialog closed sooner than this after it was sent wasn't seen by anyone: the client said it can
/// show forms but closed it unshown (Claude Code's `-p` does, without a hook to answer it). No one
/// reads a question and closes it this fast.
const DIALOG_GONE: Duration = Duration::from_millis(700);

/// What `ask_install` fails with when the client turned out not to show dialogs (`Answer::NoDialog`):
/// the caller asks on the page instead. Never an agent's error.
pub(super) const NO_DIALOG: &str = "no_dialog";

#[cfg(test)]
thread_local! {
    /// `dialog_silent` for the unit tests on this thread.
    static SILENT_FOR_TEST: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// What `ask_install` fails with when the dialog went unanswered for `dialog_silent` (`Answer::Silent`):
/// the caller asks on the page as well, and takes the first answer (`install_answer_after_page`).
/// Never an agent's error.
pub(super) const SILENT: &str = "dialog_silent";

/// The start of the ids of the front's own requests to the client, which its answers carry back.
const ID_PREFIX: &str = "endeavor-consent-";

/// A call cancelled before its question went out.
const CANCELLED: &str = "cancelled";

/// What `approve_install`'s description says of the user's yes where no dialog follows: the agent
/// asks in the chat.
use crate::mcp::ASK_IN_CHAT;

/// The same where the front asks the user itself: asking in the chat too would have them say yes twice.
const ASKED_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that the app they talk to you in shows, which names what downloads and its size, and nothing downloads unless they say yes there or with the button on the notebook's page. Tell the user it needs their OK, then call it; don't ask in the chat first. If they say no or close the dialog, the call fails with `download_declined` or `download_not_answered`.";

/// `approve_install`'s words where the client can't show a dialog and Endeavor asks the user on a
/// page in their browser instead (#388).
const ASKED_ON_PAGE: &str = "Endeavor asks the user itself, on a page that it opens in their browser and that names what downloads and its size, and nothing downloads unless they click Install there or use the button on the notebook's page. Tell the user it needs their OK, then call it; don't ask in the chat first. If they say no or haven't answered yet, the call fails with `download_declined` or `download_not_answered`.";

/// The same where no one can be asked: no dialog, and no browser to open.
const CANT_ASK: &str = "Endeavor can't ask the user from this app (no dialog, and no browser it can open), so calling it downloads nothing: tell the user what would download and its size, and that they can approve it with the button on the notebook's page. Its link is the notebook's browser_url from open_notebook or list_notebooks; where that page is on another computer, the note that came with it says how to reach it.";

/// The runtime's asking step where Endeavor asks the user on a page in their browser.
const ASK_STEP_ON_PAGE: &str = "Endeavor asks the user itself, on a page that it opens in their browser and that names what downloads and its size: tell the user it needs their OK, then call approve_install now; don't ask in the chat first. The notebook's page also has a button for it.";

/// The same where no one can be asked.
const ASK_STEP_CANT: &str = "Endeavor can't ask the user from this app, so approve_install downloads nothing here: tell the user what would download and its size, and that they can approve it with the button on the notebook's page, then end your reply. Its link is the notebook's browser_url from open_notebook or list_notebooks; where that page is on another computer, the note that came with it says how to reach it.";

/// The runtime's asking step (`notebooks::ASK_STEP`) where the front asks the user itself.
const ASK_STEP_IN_DIALOG: &str = "Endeavor asks the user itself, in a dialog that the app they talk to you in shows, which names what downloads and its size: tell the user it needs their OK, then call approve_install now; don't ask in the chat first. The notebook's page also has a button for it.";

/// The machine tools' words about asking the user before `install: true`, and what they say where
/// the front asks the user itself (#378). Each chat wording is in machine_tools.json (a test checks).
const MACHINE_SWAPS: &[(&str, &str)] = &[
    (
        "ask the user, and call again with `install` true only if they agree",
        "Endeavor then asks the user itself, at once, in a dialog that names it, and installs it only on their yes; don't ask in the chat first",
    ),
    (
        "`needs_install`: ask the user, and call again with `install` true only if they agree to what the result names",
        "`needs_install` doesn't come back: Endeavor asks the user itself, at once, in a dialog that names what it installs, and installs it only on their yes; don't ask in the chat first",
    ),
    (
        "the result is `needs_install`: ask the user, and set `install` only if they agree",
        "Endeavor asks the user itself, at once, in a dialog, and installs it only on their yes; don't ask in the chat first",
    ),
    ("Only after the user agreed to what `needs_install` named; never on a first call.", "Not needed: where something has to be installed, Endeavor asks the user in a dialog and installs only on their yes."),
    ("Only after the user agreed; this call only; never on a first call.", "Where something has to be installed, Endeavor asks the user in a dialog and installs only on their yes; set it only for what a notebook tool's message asks for, such as Julia or R not found."),
];

/// The same where the client can't show a dialog and Endeavor asks the user on a page in their
/// browser (#381): the agent still sets `install`, and that opens the page.
const MACHINE_PAGE_SWAPS: &[(&str, &str)] = &[
    (
        "ask the user, and call again with `install` true only if they agree",
        "call again with `install` true: Endeavor then asks the user itself, on a page that it opens in their browser and that names it, and installs it only once they click Install there; don't ask in the chat first",
    ),
    (
        "`needs_install`: ask the user, and call again with `install` true only if they agree to what the result names",
        "`needs_install`: call again with `install` true, and Endeavor asks the user itself, on a page that it opens in their browser and that names what it installs, and installs it only once they click Install there; don't ask in the chat first",
    ),
    (
        "the result is `needs_install`: ask the user, and set `install` only if they agree",
        "the result is `needs_install`: set `install`, and Endeavor asks the user itself, on a page in their browser, and installs it only once they click Install there; don't ask in the chat first",
    ),
    ("Only after the user agreed to what `needs_install` named; never on a first call.", "After `needs_install`. Endeavor then asks the user on a page in their browser and installs only once they click Install there."),
    ("Only after the user agreed; this call only; never on a first call.", "Endeavor then asks the user on a page in their browser and installs only once they click Install there; set it after `needs_install`, or for what a notebook tool's message asks for, such as Julia or R not found."),
];

/// The same where the client can't show a dialog and no browser can be opened: no one can be asked,
/// so nothing is installed, and the agent hears so before it asks the user for a yes it can't use.
const MACHINE_CANT_SWAPS: &[(&str, &str)] = &[
    (
        "ask the user, and call again with `install` true only if they agree",
        "tell the user what it names: Endeavor can't install it from this app, because it can neither show its dialog nor open a page in their browser to ask them",
    ),
    (
        "`needs_install`: ask the user, and call again with `install` true only if they agree to what the result names",
        "`needs_install`: tell the user what it names; Endeavor can't install it from this app, because it can neither show its dialog nor open a page in their browser to ask them",
    ),
    ("the result is `needs_install`: ask the user, and set `install` only if they agree", "the result is `needs_install`: tell the user; Endeavor can't install it from this app"),
    ("Only after the user agreed to what `needs_install` named; never on a first call.", "Installs nothing from this app: Endeavor can't ask the user here."),
    ("Only after the user agreed; this call only; never on a first call.", "Installs nothing from this app: Endeavor can't ask the user here."),
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
    /// The dialogs still up while the page asks too (`Answer::Silent`), by the call's id (as JSON).
    silent: Mutex<HashMap<String, Silent>>,
}

/// A dialog that got no answer in `dialog_silent` and stays up while the page asks.
struct Silent {
    id: String,
    answers: mpsc::Receiver<Value>,
    /// Its answer, once one came.
    answer: Option<Value>,
}

/// What a notebook's install waits for, as the runtime's `list_notebooks` says it.
#[derive(Clone)]
pub(super) enum Waiting {
    /// The user's OK for this download (`packages.download`), for the notebook whose file is `name`.
    Asking { download: Value, name: String },
    /// Nothing: the runtime refuses the call, and no one need be asked.
    Nothing,
    /// The front couldn't tell: what downloads, or for which notebook (the agent's id is all it has).
    Unknown(String),
}

/// Where the user can be asked about a download neither in a dialog nor on a page.
const DOWNLOAD_CANT_ASK: &str = "download_cant_ask::Endeavor can't ask the user from this app: it can neither show its dialog nor open a page in their browser. So nothing downloads, and calling approve_install again won't download it. Tell the user what would download and its size, and that they can approve it with the button on the notebook's page: its link is the notebook's browser_url from open_notebook or list_notebooks, and where that page is on another computer, the note that came with it says how to reach it. Don't work around it.";

/// `approve_install`'s refusal where no one was asked: what passes on to the runtime is only ever
/// the user's yes.
pub(super) fn download_cant_ask() -> Value {
    tool_error(DOWNLOAD_CANT_ASK, false)
}

/// What the user said.
pub(super) enum Asked {
    /// They said yes: pass the call on.
    Yes,
    /// They weren't asked: the client can't show a form, or nothing waits for an OK.
    Unasked,
    /// The call's result: nothing downloads.
    No(Value),
    /// The client said it shows dialogs but didn't show this one (`Answer::NoDialog`): ask on the page.
    NoDialog,
    /// The dialog is up with no answer yet (`Answer::Silent`): ask on the page as well.
    Silent,
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

    /// `tools/list`'s reply, with `approve_install` and the machine tools saying that Endeavor asks
    /// the user, where it does: in a dialog, or for the machine tools, on a page in their browser
    /// where the client can't show one and `page` says a browser can be opened. Where neither can
    /// be, the machine tools say they install nothing.
    pub(super) fn tools_listed(&self, reply: String, page: bool) -> String {
        let dialog = self.can_ask();
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        let Some(tools) = parsed["result"]["tools"].as_array_mut() else { return reply };
        for tool in tools.iter_mut() {
            let swaps: &[(&str, &str)] = match tool["name"].as_str() {
                Some("approve_install") if dialog => &[(ASK_IN_CHAT, ASKED_IN_DIALOG)],
                Some("approve_install") if page => &[(ASK_IN_CHAT, ASKED_ON_PAGE)],
                Some("approve_install") => &[(ASK_IN_CHAT, CANT_ASK)],
                Some("add_machine" | "use_machine" | "stop_machine") if dialog => MACHINE_SWAPS,
                Some("add_machine" | "use_machine" | "stop_machine") if page => MACHINE_PAGE_SWAPS,
                Some("add_machine" | "use_machine" | "stop_machine") => MACHINE_CANT_SWAPS,
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
    pub(super) fn reply_said(&self, reply: String, page: bool) -> String {
        if !reply.contains(crate::notebooks::ASK_STEP) {
            return reply;
        }
        let step = if self.can_ask() {
            ASK_STEP_IN_DIALOG
        } else if page {
            ASK_STEP_ON_PAGE
        } else {
            ASK_STEP_CANT
        };
        reply.replace(crate::notebooks::ASK_STEP, step)
    }

    /// What the user is asked before `approve_install` makes the download `waiting`, naming the
    /// notebook, so a yes on one notebook's page is never another's (#389); none when nothing waits
    /// for their OK.
    pub(super) fn download_question(waiting: &Waiting) -> Option<String> {
        Some(match waiting {
            Waiting::Nothing => return None,
            Waiting::Asking { download, name } => format!("R needs to download {} before the cells of the notebook {name} can run. Download it now?", what(download)),
            Waiting::Unknown(id) => format!("R needs to download some large Bioconductor data packages before the cells of the notebook with id {id} can run. Endeavor couldn't tell which packages or their size. Download them now?"),
        })
    }

    /// Before `approve_install` reaches the runtime where the client can't show a dialog (#388): the
    /// user's click on a page in their browser, through `pages`, waiting up to `wait`. `page` says
    /// whether a browser can be opened. As `ask` answers.
    /// After `Asked::Silent`, the dialog is still up, and whichever the user answers first counts;
    /// `write` takes the dialog down when the page's answer stands.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ask_on_page(&self, call: &Value, waiting: Waiting, page: bool, wait: std::time::Duration, pages: &super::install_page::Pages, open: &dyn Fn(&str) -> bool, write: &dyn Fn(&str)) -> Asked {
        use super::install_page::Clicked;
        let Some(question) = Consent::download_question(&waiting) else {
            if let Some(asked) = self.download_answer_after_page(call, write) {
                self.not_asking(call);
                return asked;
            }
            return if self.not_asking(call) { Asked::Dropped } else { Asked::Unasked };
        };
        // A yes to a question that can't say what downloads is for this call only.
        let hold = matches!(waiting, Waiting::Asking { .. });
        let clicked = if page { pages.ask(&question, hold, wait, &|| self.cancelled(call) || self.dialog_answered(call), open) } else { Clicked::NoBrowser };
        if let Some(asked) = self.download_answer_after_page(call, write) {
            self.not_asking(call);
            return asked;
        }
        if self.not_asking(call) || clicked == Clicked::Dropped {
            return Asked::Dropped;
        }
        let why = match clicked {
            Clicked::Install => return Asked::Yes,
            Clicked::Dropped => return Asked::Dropped,
            Clicked::Declined => "download_declined::The user clicked Don't install on Endeavor's page in their browser, so nothing downloads and the cells that need it stay queued. Endeavor won't ask them about it again for 10 minutes: until then, approve_install gets this same answer. Tell the user, and don't call it again unless they ask. If they change their mind, the page they answered has an Install button, and the notebook's page has its own: once they say they clicked one, call approve_install again.",
            Clicked::NotYet => "download_not_answered::Endeavor opened a page in the user's browser that names what downloads and its size, and nothing downloads until they click Install there. They haven't answered yet, so nothing downloads and the cells that need it stay queued. Tell the user to answer that page, and end your reply. Once they say they clicked Install, call approve_install again: their click holds for 10 minutes. The notebook's page also has its own button.",
            Clicked::NoBrowser => DOWNLOAD_CANT_ASK,
        };
        Asked::No(tool_error(why, false))
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
                } else {
                    // Its dialog was answered, and the call goes on to the page (`Answer::NoDialog`):
                    // marked, so that it doesn't.
                    *question = Some(CANCELLED.to_owned());
                }
            }
            // Cancelled while the front looks: `ask` finds it marked and asks nothing.
            None => *question = Some(CANCELLED.to_owned()),
        }
        true
    }

    /// Before `approve_install` reaches the runtime: ask the user, through `write`, whether to make
    /// the download `waiting`.
    /// `page` says whether the page can ask as well, if the dialog goes unanswered.
    pub(super) fn ask(&self, call: &Value, waiting: Waiting, page: bool, write: &dyn Fn(&str)) -> Asked {
        let question = if self.can_ask() { Consent::download_question(&waiting) } else { None };
        download_answer(self.question(call, question.as_deref(), page, write))
    }

    /// After the page asked about a download while the dialog was up (`Asked::Silent`): the dialog's
    /// answer if it came first; none if the page's stands, and the dialog is taken down.
    pub(super) fn download_answer_after_page(&self, call: &Value, write: &dyn Fn(&str)) -> Option<Asked> {
        self.dialog_after_page(call, write).map(download_answer)
    }

    /// As `download_answer_after_page`, for a machine install: as `ask_install` answers.
    pub(super) fn install_answer_after_page(&self, call: &Value, again: &str, write: &dyn Fn(&str)) -> Option<Result<bool, String>> {
        self.dialog_after_page(call, write).map(|answer| install_answer(answer, again))
    }

    /// Whether the dialog that stays up for `call` while the page asks has been answered (or the
    /// call cancelled), so that the page stops waiting.
    pub(super) fn dialog_answered(&self, call: &Value) -> bool {
        let mut silent = self.silent.lock().unwrap();
        let Some(dialog) = silent.get_mut(&to_json(&call["id"])) else { return false };
        if dialog.answer.is_none() {
            dialog.answer = dialog.answers.try_recv().ok();
        }
        dialog.answer.is_some()
    }

    fn dialog_after_page(&self, call: &Value, write: &dyn Fn(&str)) -> Option<Answer> {
        let Silent { id, answers, answer } = self.silent.lock().unwrap().remove(&to_json(&call["id"]))?;
        self.waiting.lock().unwrap().remove(&id);
        let answer = answer.or_else(|| answers.try_recv().ok());
        let Some(answer) = answer else {
            // The page answered first, or neither did: the dialog goes.
            write(&to_json(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })));
            return None;
        };
        let action = answer["result"]["action"].as_str().unwrap_or_default();
        Some(match action {
            _ if answer["cancelled"] == true => {
                write(&to_json(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": id } })));
                Answer::Dropped
            }
            "accept" => Answer::Yes,
            "decline" => Answer::Declined,
            "cancel" => Answer::Closed,
            _ => Answer::Failed(answer["error"]["message"].as_str().unwrap_or("the client gave no answer").to_owned()),
        })
    }
}

/// `ask`'s answer from the user's.
fn download_answer(answer: Answer) -> Asked {
    {
        let why = match answer {
            Answer::Yes => return Asked::Yes,
            Answer::Unasked => return Asked::Unasked,
            Answer::Dropped => return Asked::Dropped,
            Answer::NoDialog => return Asked::NoDialog,
            Answer::Silent => return Asked::Silent,
            Answer::Declined => "download_declined::The user said no in Endeavor's download dialog, which the app they talk to you in showed, so nothing downloads and the cells that need it stay queued. Tell the user; call approve_install again only if they ask for the download.".to_owned(),
            Answer::Closed => "download_not_answered::The user closed Endeavor's download dialog without answering. That dialog was in the app they talk to you in, and it is gone now. Nothing downloads and the cells that need it stay queued. Ask the user in the chat whether to download it; if they say yes, call approve_install again, and the same dialog asks them once more. The notebook's page also has its own button that approves it.".to_owned(),
            Answer::TimedOut => "download_not_answered::Endeavor asked the user in a dialog and got no answer in 30 minutes, so it took the dialog down and nothing downloads. No dialog is showing now: don't tell the user to answer one. Ask them in the chat whether to download it; if they want it, call approve_install again, and it asks them once more. The notebook's page also has its own button that approves it.".to_owned(),
            Answer::Failed(error) => format!("download_not_answered::Endeavor couldn't ask the user ({error}), so nothing downloads. Ask the user in the chat; if they want it, call approve_install again."),
        };
        Asked::No(tool_error(&why, false))
    }
}

/// `ask_install`'s answer from the user's.
fn install_answer(answer: Answer, again: &str) -> Result<bool, String> {
    Err(match answer {
        Answer::Yes => return Ok(true),
        Answer::Dropped => return Ok(false),
        Answer::NoDialog => NO_DIALOG.to_owned(),
        Answer::Silent => SILENT.to_owned(),
        // Never a yes for the user: the client stopped saying it can show a form since the caller looked.
        Answer::Unasked => "install_not_answered::Endeavor couldn't ask the user in a dialog, so nothing was installed. Ask the user in the chat; if they want it, call again with `install: true`.".to_owned(),
        Answer::Declined => format!("install_declined::The user said no in Endeavor's install dialog, which the app they talk to you in showed, so nothing was installed. Tell the user; call {again} with `install: true` only if they ask for it."),
        Answer::Closed => format!("install_not_answered::The user closed Endeavor's install dialog without answering. That dialog was in the app they talk to you in, and it is gone now. Nothing was installed. Ask the user in the chat whether Endeavor may install it; if they say yes, call {again} with `install: true`, and the same dialog asks them once more."),
        Answer::TimedOut => format!("install_not_answered::Endeavor asked the user in a dialog and got no answer in 30 minutes, so it took the dialog down and nothing was installed. No dialog is showing now: don't tell the user to answer one. Ask them in the chat; if they want it, call {again} with `install: true`, and it asks them once more."),
        Answer::Failed(error) => format!("install_not_answered::Endeavor couldn't ask the user ({error}), so nothing was installed. Ask the user in the chat; if they want it, call {again} with `install: true`."),
    })
}

impl Consent {

    /// Before a machine tool's `install: true` goes ahead (#378): ask the user, through `write`,
    /// `question`, which names what it would install. `again` is how the agent calls it again.
    /// The call goes ahead on `Ok(true)`, gets no reply on `Ok(false)` (the client cancelled it),
    /// and on `Err` installs nothing, with the `kind::message` it fails with.
    pub(super) fn ask_install(&self, call: &Value, question: &str, again: &str, page: bool, write: &dyn Fn(&str)) -> Result<bool, String> {
        install_answer(self.question(call, Some(question), page, write), again)
    }

    /// Whether the client cancelled `call` since `looking`.
    pub(super) fn cancelled(&self, call: &Value) -> bool {
        self.calls.lock().unwrap().get(&to_json(&call["id"])).is_some_and(|question| question.as_deref() == Some(CANCELLED))
    }

    /// `call` was on its way to `ask` or `ask_install` (`looking`), and nothing needs asking: forget
    /// it. Whether the client cancelled it meanwhile.
    pub(super) fn not_asking(&self, call: &Value) -> bool {
        self.calls.lock().unwrap().remove(&to_json(&call["id"])).is_some_and(|question| question.as_deref() == Some(CANCELLED))
    }

    /// Ask the user `question` through `write`, for `call`; none, or a client that can't show a
    /// form, asks nothing. With `page`, a dialog unanswered in `dialog_silent` stays up and is
    /// `Answer::Silent`: the caller asks on the page as well.
    fn question(&self, call: &Value, question: Option<&str>, page: bool, write: &dyn Fn(&str)) -> Answer {
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
        let sent = std::time::Instant::now();
        let answer = match answer.recv_timeout(if page { dialog_silent().min(ANSWER_WAIT) } else { ANSWER_WAIT }) {
            Ok(answer) => Some(answer),
            Err(_) if page => {
                eprintln!("[ Info: No answer to Endeavor's dialog yet; asking on a page as well");
                self.silent.lock().unwrap().insert(call_id, Silent { id, answers: answer, answer: None });
                return Answer::Silent;
            }
            Err(_) => None,
        };
        self.waiting.lock().unwrap().remove(&id);
        let action = answer.as_ref().and_then(|a| a["result"]["action"].as_str()).unwrap_or_default().to_owned();
        // A client that says it shows forms may not (#381): one that answers with an error, or closes
        // the dialog before anyone could have read it, doesn't. From then on the user is asked on the
        // page, this call too, so the call stays the front's to cancel.
        let shown = match (action.as_str(), &answer) {
            ("cancel", _) => sent.elapsed() >= DIALOG_GONE,
            // Only an error that says the client has no dialog, or that comes before anyone could
            // have seen one: a later error (a dialog that broke while up) is no reason to stop
            // asking in dialogs for good.
            (_, Some(answer)) => answer.get("error").is_none_or(|error| error["code"] != -32601 && sent.elapsed() >= DIALOG_GONE),
            _ => true,
        };
        let mut calls = self.calls.lock().unwrap();
        if calls.get(&call_id).is_some_and(|question| question.as_deref() == Some(CANCELLED)) {
            // Cancelled after its answer came.
            calls.remove(&call_id);
            return Answer::Dropped;
        }
        if shown {
            calls.remove(&call_id);
        } else {
            calls.insert(call_id, None);
            drop(calls);
            self.can_ask.store(false, Ordering::SeqCst);
            eprintln!("[ Info: The client didn't show Endeavor's dialog ({}); asking on a page from now on", if action == "cancel" { "closed at once" } else { "an error" });
            return Answer::NoDialog;
        }
        drop(calls);
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
    /// The client didn't show it: it answered with an error, or closed it before anyone could read it.
    /// It is taken for one that can't show dialogs from now on.
    NoDialog,
    /// No answer yet in `dialog_silent`: the dialog stays up, and the page asks as well.
    Silent,
}

/// When the front can't tell what `notebook`'s install waits for: an id that isn't one waits for
/// nothing the call could give (the runtime refuses it), and any other is named as the runtime reads it.
pub(super) fn unknown(notebook: &Value) -> Waiting {
    crate::notebooks::uuid_value(notebook).map_or(Waiting::Nothing, Waiting::Unknown)
}

/// What `list_notebooks`' reply says `notebook`'s install waits for. A notebook it doesn't list, or
/// one in safe preview, waits for nothing the call could give: the runtime refuses it.
pub(super) fn waiting_in(listed: &str, notebook: &Value) -> Waiting {
    let Ok(parsed) = serde_json::from_str::<Value>(listed) else { return unknown(notebook) };
    if parsed["result"]["isError"] == true {
        return unknown(notebook);
    }
    let Some(fields) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str::<Value>(text).ok()) else { return unknown(notebook) };
    let Some(notebooks) = fields.as_array().or_else(|| fields["notebooks"].as_array()) else { return unknown(notebook) };
    // The id as the runtime reads it.
    let Some(notebook) = crate::notebooks::uuid_value(notebook) else { return Waiting::Nothing };
    let Some(nb) = notebooks.iter().find(|nb| crate::notebooks::uuid_value(&nb["notebook_id"]).as_ref() == Some(&notebook)) else { return Waiting::Nothing };
    if nb["packages"]["step"] != "asking" || nb["execution_allowed"] == false {
        return Waiting::Nothing;
    }
    // The whole path: a yes is held by the question's words, so two notebooks with the same file
    // name in different folders must not read the same.
    let name = nb["path"].as_str().filter(|path| !path.is_empty()).map(str::to_owned);
    Waiting::Asking { download: nb["packages"]["download"].clone(), name: name.unwrap_or(notebook) }
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
        Waiting::Asking { download: json!({ "bytes": 11385380, "packages": [{ "package": "ALL", "version": "1.44.0", "bytes": 11385380 }] }), name: "leukemia.R".to_owned() }
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
                    // A person takes longer than this to close a dialog.
                    if answer.as_ref().is_some_and(|a| a["action"] == "cancel") {
                        std::thread::sleep(DIALOG_GONE + Duration::from_millis(50));
                    }
                    let reply = match &answer {
                        Some(answer) => json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer }),
                        None => json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": -32601, "message": "Method not found" } }),
                    };
                    assert!(consent.take_answer(&reply));
                }
            })
        };
        let result = consent.ask(&call(), waiting, false, &|line| to_client.send(line.to_owned()).unwrap());
        drop(to_client);
        answering.join().unwrap();
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    #[test]
    fn every_machine_wording_swapped_where_a_dialog_asks_is_in_the_tools() {
        for (chat, _) in MACHINE_SWAPS.iter().chain(MACHINE_PAGE_SWAPS).chain(MACHINE_CANT_SWAPS) {
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
                        if answer["action"] == "cancel" {
                            std::thread::sleep(DIALOG_GONE + Duration::from_millis(50));
                        }
                        assert!(consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer })));
                    }
                }
                asked
            });
            let result = consent.ask_install(&call(), "Endeavor needs to install Endeavor's helper on lab. Install it now?", "`add_machine` again", false, &|line| to_client.send(line.to_owned()).unwrap());
            drop(to_client);
            (result, answering.join().unwrap())
        };
        let (yes, asked) = ask(json!({ "action": "accept" }));
        assert_eq!((yes, asked), (Ok(true), Some(json!("Endeavor needs to install Endeavor's helper on lab. Install it now?"))));
        let (no, _) = ask(json!({ "action": "decline" }));
        assert!(no.as_ref().is_err_and(|e| e.starts_with("install_declined::") && e.contains("`add_machine` again with `install: true` only if they ask")), "{no:?}");
        let (closed, _) = ask(json!({ "action": "cancel" }));
        assert!(closed.as_ref().is_err_and(|e| e.starts_with("install_not_answered::")), "{closed:?}");
        // A client that no longer says it can show a form is never taken as a yes.
        consent.learn_client(&initialize(Value::Null));
        let unasked = consent.ask_install(&call(), "Install it now?", "`add_machine` again", false, &|_| panic!("nothing is sent"));
        assert!(unasked.as_ref().is_err_and(|e| e.starts_with("install_not_answered::")), "{unasked:?}");
    }

    #[test]
    fn a_client_that_says_it_shows_dialogs_and_doesnt_is_asked_on_the_page() {
        // A client can say it shows forms and still show nothing (#381): one that answers with an
        // error, or closes the dialog before anyone could read it. From then on it is one without
        // dialogs, and this call goes to the page.
        let closed_at_once = |answer: Value| {
            let consent = Arc::new(Consent::default());
            consent.learn_client(&initialize(json!({ "form": {} })));
            let consent2 = consent.clone();
            let (to_client, client) = mpsc::channel::<String>();
            let answering = std::thread::spawn(move || {
                for line in client {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    if request.get("id").is_some() {
                        assert!(consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "result": answer.clone() })));
                    }
                }
            });
            consent.looking(&call());
            let asked = consent.ask(&call(), all(), false, &|line| to_client.send(line.to_owned()).unwrap());
            drop(to_client);
            answering.join().unwrap();
            (asked, consent)
        };
        let (got, consent) = closed_at_once(json!({ "action": "cancel" }));
        assert!(matches!(got, Asked::NoDialog) && !consent.can_ask());
        // The call is still the front's: a cancel of it now stops the page's wait.
        assert!(consent.take_cancel(&json!({ "method": "notifications/cancelled", "params": { "requestId": 9 } })) && consent.cancelled(&call()));
        // A yes at once is a yes: a client may answer for its user, as `-p` with a hook does.
        let (got, consent) = closed_at_once(json!({ "action": "accept" }));
        assert!(matches!(got, Asked::Yes) && consent.can_ask());
        // So is a no at once: only a close is too fast to be a person's.
        let (got, consent) = closed_at_once(json!({ "action": "decline" }));
        assert!(matches!(got, Asked::No(_)) && consent.can_ask());
        // An error is no dialog, for a machine install too.
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({})));
        let (got, _) = asked(consent.clone(), all(), None);
        assert!(matches!(got, Asked::NoDialog) && !consent.can_ask());
        consent.learn_client(&initialize(json!({})));
        let consent2 = consent.clone();
        let (to_client, client) = mpsc::channel::<String>();
        let answering = std::thread::spawn(move || {
            for line in client {
                let request: Value = serde_json::from_str(&line).unwrap();
                assert!(consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": -32601, "message": "Method not found" } })));
            }
        });
        let install = consent.ask_install(&call(), "Install it now?", "`add_machine` again", false, &|line| to_client.send(line.to_owned()).unwrap());
        drop(to_client);
        answering.join().unwrap();
        assert_eq!(install, Err(NO_DIALOG.to_owned()));
        assert!(!consent.can_ask());
        // Another error is no dialog only when it comes at once: one that comes after a dialog could
        // have been up (it broke) keeps the client's dialogs.
        let other_error = |after: Duration| {
            let consent = Arc::new(Consent::default());
            consent.learn_client(&initialize(json!({ "form": {} })));
            let consent2 = consent.clone();
            let (to_client, client) = mpsc::channel::<String>();
            let answering = std::thread::spawn(move || {
                for line in client {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    if request.get("id").is_some() {
                        std::thread::sleep(after);
                        assert!(consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": -32603, "message": "Internal error" } })));
                    }
                }
            });
            consent.looking(&call());
            let got = consent.ask(&call(), all(), false, &|line| to_client.send(line.to_owned()).unwrap());
            drop(to_client);
            answering.join().unwrap();
            (got, consent.can_ask())
        };
        assert!(matches!(other_error(Duration::ZERO), (Asked::NoDialog, false)));
        let (got, dialogs) = other_error(DIALOG_GONE + Duration::from_millis(50));
        assert!(dialogs && !matches!(got, Asked::NoDialog | Asked::Yes), "a late error isn't a yes or the end of dialogs");
    }

    #[test]
    fn a_client_that_can_show_a_form_is_asked_before_a_download() {
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({ "form": {}, "url": {} })));
        let (result, sent) = asked(consent.clone(), all(), Some(json!({ "action": "accept", "content": {} })));
        assert!(matches!(result, Asked::Yes));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "elicitation/create");
        assert_eq!(sent[0]["params"]["message"], "R needs to download ALL 1.44.0 (11 MB) before the cells of the notebook leukemia.R can run. Download it now?");
        // A no or a closed dialog downloads nothing, and the agent hears which.
        for (answer, kind) in [(Some(json!({ "action": "decline" })), "said no"), (Some(json!({ "action": "cancel" })), "without answering. That dialog was in the app they talk to you in, and it is gone now")] {
            let (result, _) = asked(consent.clone(), all(), answer);
            let Asked::No(result) = result else { panic!("{kind}: not refused") };
            assert_eq!(result["isError"], true, "{result}");
            assert!(result["content"][0]["text"].as_str().unwrap().contains(kind), "{result}");
        }
        // Not knowing what it would download, it still asks; with nothing waiting, it doesn't.
        let (_, sent) = asked(consent.clone(), Waiting::Unknown("nb-7".into()), Some(json!({ "action": "decline" })));
        assert!(sent[0]["params"]["message"].as_str().unwrap().contains("some large Bioconductor data packages before the cells of the notebook with id nb-7"));
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
            std::thread::spawn(move || consent.ask(&call(), all(), false, &|line| to_client.send(line.to_owned()).unwrap()))
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
        assert!(matches!(consent.ask(&call(), all(), false, &|_| panic!("asked")), Asked::Dropped));
        // Done with: a later cancel of the same id is the runtime's again.
        assert!(!consent.take_cancel(&cancel));
    }

    #[test]
    fn a_client_that_cant_show_one_isnt_sent_a_dialog() {
        for elicitation in [Value::Null, json!({ "url": {} })] {
            let consent = Consent::default();
            consent.learn_client(&initialize(elicitation));
            assert!(matches!(consent.ask(&call(), all(), false, &|_| panic!("asked")), Asked::Unasked));
            // approve_install says where it asks instead (#388): on a page, or nowhere.
            let listed = to_json(&json!({ "result": { "tools": [{ "name": "approve_install", "description": ASK_IN_CHAT }] } }));
            let description = |page: bool| serde_json::from_str::<Value>(&consent.tools_listed(listed.clone(), page)).unwrap()["result"]["tools"][0]["description"].as_str().unwrap().to_owned();
            assert_eq!((description(true), description(false)), (ASKED_ON_PAGE.to_owned(), CANT_ASK.to_owned()));
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
        let shown: Value = serde_json::from_str(&consent.tools_listed(listed, false)).unwrap();
        let approve = shown["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "approve_install").unwrap();
        let text = approve["description"].as_str().unwrap();
        assert!(text.contains(ASKED_IN_DIALOG) && !text.contains(ASK_IN_CHAT), "{text}");
    }

    #[test]
    fn where_it_asks_the_asking_message_says_so() {
        let reply = |text: &str| to_json(&json!({ "jsonrpc": "2.0", "id": 4, "result": { "content": [{ "type": "text", "text": to_json(&json!({ "message": text })) }] } }));
        let asking = format!("R has to download ALL. {} Don't call read_cell to wait.", crate::notebooks::ASK_STEP);
        let consent = Consent::default();
        assert_eq!(consent.reply_said(reply(&asking), true), reply(&format!("R has to download ALL. {ASK_STEP_ON_PAGE} Don't call read_cell to wait.")));
        assert_eq!(consent.reply_said(reply(&asking), false), reply(&format!("R has to download ALL. {ASK_STEP_CANT} Don't call read_cell to wait.")));
        assert_eq!(consent.reply_said(reply("ran"), false), reply("ran"));
        consent.learn_client(&initialize(json!({ "form": {} })));
        let said = consent.reply_said(reply(&asking), true);
        assert_eq!(said, reply(&format!("R has to download ALL. {ASK_STEP_IN_DIALOG} Don't call read_cell to wait.")));
        // It says where the dialog is, and the page's button is still there.
        assert!(said.contains("The notebook's page also has a button for it.") && !said.contains("The page's own button does the same."), "{said}");
    }

    #[test]
    fn a_dialog_that_goes_unanswered_gets_the_page_too_and_its_late_answer_counts() {
        // Claude Code's `-p` takes the dialog and never answers (#396's smoke runs): after a while
        // the page asks as well, and the dialog stays up. A slow person who then answers the dialog
        // gives the answer: here, a no, which the page's wait doesn't outlast.
        SILENT_FOR_TEST.set(Some(Duration::from_millis(200)));
        let consent = Arc::new(Consent::default());
        consent.learn_client(&initialize(json!({ "form": {} })));
        let consent2 = consent.clone();
        let (to_client, client) = mpsc::channel::<String>();
        let answering = std::thread::spawn(move || {
            let mut sent = Vec::new();
            for line in client {
                let request: Value = serde_json::from_str(&line).unwrap();
                if request["method"] == "elicitation/create" {
                    let (consent2, id) = (consent2.clone(), request["id"].clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(600));
                        consent2.take_answer(&json!({ "jsonrpc": "2.0", "id": id, "result": { "action": "decline" } }));
                    });
                }
                sent.push(request);
            }
            sent
        });
        let write = |line: &str| to_client.send(line.to_owned()).unwrap();
        let pages = super::super::install_page::Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        consent.looking(&call());
        assert!(matches!(consent.ask(&call(), all(), true, &write), Asked::Silent));
        assert!(consent.can_ask(), "silence isn't the end of dialogs");
        let began = std::time::Instant::now();
        let asked = consent.ask_on_page(&call(), all(), true, Duration::from_secs(10), &pages, &open, &write);
        assert!(began.elapsed() < Duration::from_secs(5), "the dialog's answer ends the page's wait");
        let Asked::No(result) = asked else { panic!("a no in the dialog is a no") };
        assert!(result["content"][0]["text"].as_str().unwrap().contains("said no"), "{result}");
        assert_eq!(opened.lock().unwrap().len(), 1, "the page asked too");
        drop(to_client);
        let sent = answering.join().unwrap();
        assert_eq!(sent.len(), 1, "the answered dialog isn't withdrawn: {sent:?}");

        // A silent client whose call is cancelled while both ask: nothing downloads, and no reply.
        let (to_client, client) = mpsc::channel::<String>();
        let answering = std::thread::spawn(move || client.into_iter().map(|line| serde_json::from_str::<Value>(&line).unwrap()).collect::<Vec<_>>());
        let write = |line: &str| to_client.send(line.to_owned()).unwrap();
        consent.looking(&call());
        assert!(matches!(consent.ask(&call(), all(), true, &write), Asked::Silent));
        let cancelling = { let consent = consent.clone(); std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(300)); consent.take_cancel(&json!({ "method": "notifications/cancelled", "params": { "requestId": 9 } })) }) };
        assert!(matches!(consent.ask_on_page(&call(), all(), true, Duration::from_secs(10), &pages, &open, &write), Asked::Dropped));
        assert!(cancelling.join().unwrap());
        drop(to_client);
        let sent = answering.join().unwrap();
        assert!(sent.iter().any(|l| l["method"] == "notifications/cancelled" && l["params"]["requestId"] == sent[0]["id"]), "the dialog is withdrawn: {sent:?}");
        SILENT_FOR_TEST.set(None);
    }

    #[test]
    fn without_a_dialog_a_download_waits_for_the_pages_button() {
        use std::io::{Read, Write};
        let consent = Consent::default();
        consent.learn_client(&initialize(Value::Null));
        let pages = super::super::install_page::Pages::default();
        let opened = Mutex::new(Vec::<String>::new());
        let open = |url: &str| {
            opened.lock().unwrap().push(url.to_owned());
            true
        };
        let ask = |waiting: Waiting, page: bool| {
            consent.looking(&call());
            consent.ask_on_page(&call(), waiting, page, Duration::from_millis(50), &pages, &open, &|_| ())
        };
        let error = |asked: Asked| match asked {
            Asked::No(result) => result["content"][0]["text"].as_str().unwrap().to_owned(),
            _ => panic!("not refused"),
        };
        // Nothing waits: the call goes on, and no page opens.
        assert!(matches!(ask(Waiting::Nothing, true), Asked::Unasked));
        // No browser: nothing downloads, and the agent hears where the user can approve it.
        let cant = error(ask(all(), false));
        assert!(cant.contains("download_cant_ask") && cant.contains("button on the notebook's page"), "{cant}");
        assert!(opened.lock().unwrap().is_empty());
        // A page names the download; unanswered, nothing downloads.
        let waiting = error(ask(all(), true));
        assert!(waiting.contains("download_not_answered") && !waiting.contains("http"), "{waiting}");
        let url = opened.lock().unwrap()[0].clone();
        // The user's click on the page is the yes.
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, path) = rest.split_once('/').unwrap();
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        write!(socket, "GET /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut page = String::new();
        socket.read_to_string(&mut page).unwrap();
        assert!(page.contains("ALL 1.44.0 (11 MB)"), "{page}");
        let body = "answer=install";
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        write!(socket, "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        socket.read_to_string(&mut String::new()).unwrap();
        assert!(matches!(ask(all(), true), Asked::Yes));
        assert_eq!(opened.lock().unwrap().len(), 1);
        // The yes held is for that notebook only: the same download for another notebook gets its own page (#389).
        let Waiting::Asking { download, .. } = all() else { unreachable!() };
        let other = Waiting::Asking { download, name: "other.R".to_owned() };
        assert!(error(ask(other, true)).contains("download_not_answered"));
        assert_eq!(opened.lock().unwrap().len(), 2);
        // A yes to a question that can't say what downloads holds for no later call.
        let vague = || Waiting::Unknown("1b2c3d4e-0000-4000-8000-00000000000a".to_owned());
        assert!(error(ask(vague(), true)).contains("download_not_answered"));
        let url = opened.lock().unwrap()[2].clone();
        let (port, path) = url.strip_prefix("http://127.0.0.1:").unwrap().split_once('/').unwrap();
        let mut socket = std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        write!(socket, "POST /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        socket.read_to_string(&mut String::new()).unwrap();
        assert!(matches!(ask(vague(), true), Asked::Yes), "the click answers the next call");
        assert!(error(ask(vague(), true)).contains("download_not_answered"), "and only that one");
    }

    #[test]
    fn the_runtimes_list_says_what_waits() {
        let listed = |notebooks: Value| to_json(&json!({ "jsonrpc": "2.0", "id": "x", "result": { "content": [{ "type": "text", "text": to_json(&notebooks) }] } }));
        let asking = json!({ "step": "asking", "download": { "bytes": 5 } });
        let id = "1b2c3d4e-0000-4000-8000-00000000000a";
        let nb = |packages: Value, allowed: bool| json!({ "notebook_id": id, "path": "/data/leukemia.R", "execution_allowed": allowed, "packages": packages });
        // The id as the runtime reads it: in capitals too.
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), &json!(id.to_uppercase())), Waiting::Asking { download, name } if download["bytes"] == 5 && name == "/data/leukemia.R"));
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), false)])), &json!(id)), Waiting::Nothing), "safe preview");
        assert!(matches!(waiting_in(&listed(json!([nb(json!({ "step": "installing" }), true)])), &json!(id)), Waiting::Nothing));
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), &json!("1b2c3d4e-0000-4000-8000-00000000000b")), Waiting::Nothing), "another notebook");
        assert!(matches!(waiting_in(&listed(json!([nb(asking.clone(), true)])), &json!("nb")), Waiting::Nothing), "not an id");
        assert!(matches!(waiting_in("ran", &json!(id)), Waiting::Unknown(_)));
        // Two notebooks with one file name in different folders are asked about apart: a yes is
        // held by the question's words.
        let elsewhere = json!({ "notebook_id": id, "path": "/other/leukemia.R", "execution_allowed": true, "packages": asking });
        let question = |listed: String| Consent::download_question(&waiting_in(&listed, &json!(id))).unwrap();
        assert_ne!(question(listed(json!([nb(asking.clone(), true)]))), question(listed(json!([elsewhere]))));
    }

    #[test]
    fn several_packages_say_their_sum() {
        let download = json!({ "bytes": 1.3e9, "packages": [{ "package": "GO.db", "version": "3.22.0", "bytes": 4e8 }, { "package": "BSgenome.Hsapiens.UCSC.hg38", "version": "1.4.5", "bytes": 8.7e8 }, { "package": "org.Hs.eg.db" }, { "package": "tiny", "bytes": 3e5 }] });
        assert_eq!(what(&download), "GO.db 3.22.0 (400 MB), BSgenome.Hsapiens.UCSC.hg38 1.4.5 (870 MB), org.Hs.eg.db (size unknown), tiny (under 1 MB), 1.3 GB in all");
    }
}

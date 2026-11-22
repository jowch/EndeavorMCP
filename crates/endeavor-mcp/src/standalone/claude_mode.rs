//! Claude Code's own permission mode, for the front it started. In auto mode the user has said not to
//! be asked, so a Julia run there adds a new package without the install card, as in the app's Auto
//! mode (#397). Claude Code tells an MCP server nothing of its mode, but it gives it to its hooks: the
//! plugin's PreToolUse hook (`endeavor hook`, run before each of Endeavor's tool calls) writes the
//! mode down, and the front reads it as it passes each call on (`X-Endeavor-Mode: auto`,
//! `mcp::Caller::auto_mode`).
//!
//! The hook writes two files. One is named by the conversation (the hook's `session_id`), which the
//! front knows from `CLAUDE_CODE_SESSION_ID`. The other is named by the Claude Code process the hook
//! runs under, `by-process/<pid>`: the front is that process's child, registers that file when it
//! starts, with its own pid beside it, and the hook writes it for the nearest of its own ancestors
//! whose registered front is still running as that ancestor's child. Claude Code runs one
//! front of the plugin per process (subagents share it, in their parent's mode), so one file per
//! process is one per front. `/clear` starts a
//! new conversation in the same front, whose `CLAUDE_CODE_SESSION_ID` then names the old one (#505), so
//! the process's file is what carries the mode there; Windows has no parent lookup here and has only
//! the conversation's. The front reads whichever of its two was written last.
//!
//! Anything missing, unreadable or old is not auto, so the user is asked as before: a hook that didn't
//! run, a client that isn't Claude Code. The hook writes before every call, so a file written more than
//! `FRESH` ago doesn't say what the mode is now.
//!
//! Like `X-Endeavor-Consent`, the files are a declaration, not a check. An agent outside auto mode
//! could write "auto" into one itself, but only with the user's OK for a shell command (which could
//! as well install the package), and only between its call's hook and the call: the hook rewrites
//! them before every call. No text an agent reads names them.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// How long after the hook wrote it a mode still counts. The hook runs just before the call, and in
/// auto mode only Claude Code's own check of the call comes between. Kept short, since a hook that
/// failed to write after the user left auto mode leaves the old mode standing this long.
const FRESH: Duration = Duration::from_secs(30);

/// A file is removed this long after it was last written.
const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// How many of its ancestors the hook looks through for its Claude Code process: Claude Code runs it
/// through a shell or two.
#[cfg(unix)]
const ANCESTORS: usize = 8;

/// The modes in which the user isn't asked: auto, and bypassPermissions, which asks for nothing.
fn skips_asking(mode: &str) -> bool {
    matches!(mode, "auto" | "bypassPermissions")
}

/// A conversation id that can name a file: what `session_key` takes too.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The file named by conversation `conversation` in `dir`.
fn conversation_file(dir: &Path, conversation: Option<&str>) -> Option<PathBuf> {
    let id = conversation?.trim();
    valid_id(id).then(|| dir.join(id))
}

/// The file named by process `pid` in `dir`.
fn process_file(dir: &Path, pid: u32) -> PathBuf {
    dir.join("by-process").join(pid.to_string())
}

/// Which front registered `process_file`: its pid.
fn front_file(process_file: &Path) -> PathBuf {
    process_file.with_extension("front")
}

/// Whether the front registered for process `pid` is still running as its child.
fn front_alive(dir: &Path, pid: u32) -> bool {
    let Some(front) = std::fs::read_to_string(front_file(&process_file(dir, pid))).ok().and_then(|f| f.trim().parse::<u32>().ok()) else { return false };
    #[cfg(test)]
    if let Some(alive) = FRONTS_FOR_TEST.with(|f| f.borrow().get(&front).copied()) {
        return alive == pid;
    }
    #[cfg(unix)]
    return crate::unixproc::parent(front as i32) == Some(pid as i32);
    #[cfg(not(unix))]
    false
}

#[cfg(test)]
thread_local! {
    /// The test fronts that are running, by pid, with their parents.
    static FRONTS_FOR_TEST: std::cell::RefCell<std::collections::HashMap<u32, u32>> = Default::default();
}

/// Where a front reads Claude Code's mode.
#[derive(Default)]
pub(super) struct Files {
    conversation: Option<PathBuf>,
    process: Option<PathBuf>,
}

impl Files {
    /// A front's files in `dir`: by its conversation, when it knows it, and by its parent, Claude Code.
    pub(super) fn of(dir: &Path, conversation: Option<&str>, parent: Option<u32>) -> Files {
        Files { conversation: conversation_file(dir, conversation), process: parent.filter(|&pid| pid > 1).map(|pid| process_file(dir, pid)) }
    }

    /// Make the process's file, empty (no mode yet), and say which front it is for (`front_alive`), so
    /// the hook finds it. Any mode a former process given the same pid left goes.
    pub(super) fn register(&self) {
        self.register_as(std::process::id());
    }

    fn register_as(&self, front: u32) {
        let Some(file) = &self.process else { return };
        if let Some(folder) = file.parent()
            && crate::make_state_dir(folder).is_ok()
        {
            write(file, "");
            write(&front_file(file), &front.to_string());
        }
    }

    /// Whether the mode the hook wrote last, lately, skips asking.
    pub(super) fn auto(&self) -> bool {
        let written = |file: &Path| std::fs::metadata(file).and_then(|m| m.modified()).ok().map(|at| (at, file.to_owned()));
        // On a tie, the process's: `max_by_key` takes the last.
        let last = [&self.conversation, &self.process].into_iter().flatten().filter_map(|f| written(f)).max_by_key(|(at, _)| *at);
        let Some((at, file)) = last else { return false };
        let fresh = SystemTime::now().duration_since(at).is_ok_and(|age| age <= FRESH);
        fresh && std::fs::read_to_string(file).is_ok_and(|mode| skips_asking(mode.trim()))
    }
}

/// `endeavor hook`: Claude Code's PreToolUse hook input on stdin. Writes the mode and prints nothing,
/// so the call goes on as it would have. It never fails the call: what it can't read or write is left
/// alone.
pub fn hook_main() -> ! {
    let mut input = String::new();
    // Read whole: a cut input wouldn't parse, and then an earlier auto mode would stay.
    let _ = std::io::stdin().read_to_string(&mut input);
    record(&crate::paths::Env::here().claude_modes_dir(), &input, &ancestors());
    std::process::exit(0)
}

/// This process's ancestors, nearest first.
fn ancestors() -> Vec<u32> {
    #[cfg(unix)]
    {
        let mut pids = vec![std::os::unix::process::parent_id()];
        while pids.len() < ANCESTORS
            && let Some(parent) = crate::unixproc::parent(*pids.last().unwrap() as i32)
            && parent > 1
        {
            pids.push(parent as u32);
        }
        pids
    }
    #[cfg(not(unix))]
    Vec::new()
}

/// Write the mode in the hook input `input` to the conversation's file in `dir`, and to the process
/// file of the nearest of `ancestors` whose front is running.
pub(super) fn record(dir: &Path, input: &str, ancestors: &[u32]) {
    let Ok(input) = serde_json::from_str::<Value>(input) else { return };
    let mode = input["permission_mode"].as_str().unwrap_or_default();
    let mode = if mode.len() <= 32 && mode.chars().all(|c| c.is_ascii_alphanumeric()) { mode } else { "" };
    // The nearest whose front is still its child: a file a dead front left under a pid that a shell
    // between has now is passed over, and a Claude Code started from another's shell writes its own
    // front's file, not the outer one's.
    if let Some(file) = ancestors.iter().filter(|&&pid| pid > 1).find(|&&pid| front_alive(dir, pid)).map(|&pid| process_file(dir, pid)) {
        write(&file, mode);
    }
    let Some(file) = conversation_file(dir, input["session_id"].as_str()) else { return };
    if crate::make_state_dir(dir).is_err() {
        return;
    }
    let new = !file.exists();
    if write(&file, mode) && new {
        forget_old(dir);
        forget_old(&dir.join("by-process"));
    }
}

/// Write `text` to `file` whole and then rename it there, so the front never reads half a file.
fn write(file: &Path, text: &str) -> bool {
    let (Some(folder), Some(name)) = (file.parent(), file.file_name()) else { return false };
    let partial = folder.join(format!(".{}.{}", name.to_string_lossy(), std::process::id()));
    let written = crate::owner_only(std::fs::OpenOptions::new().write(true).create(true).truncate(true)).open(&partial).and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
    if written.is_err() || std::fs::rename(&partial, file).is_err() {
        let _ = std::fs::remove_file(&partial);
        return false;
    }
    true
}

/// Remove the files not written for `KEPT`.
fn forget_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().is_ok_and(|m| m.is_file() && m.modified().is_ok_and(|written| SystemTime::now().duration_since(written).is_ok_and(|age| age > KEPT)));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("endeavor-claude-mode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn input(session: &str, mode: &str) -> String {
        serde_json::json!({ "session_id": session, "permission_mode": mode, "hook_event_name": "PreToolUse", "tool_name": "mcp__plugin_endeavor_endeavor__execute_cell", "tool_input": { "code": "\"permission_mode\": \"auto\"" } }).to_string()
    }

    /// A front of conversation `conversation` under process 4242, registered and running.
    fn front(dir: &Path, conversation: Option<&str>) -> Files {
        front_under(dir, conversation, 4242, 50_000)
    }

    fn front_under(dir: &Path, conversation: Option<&str>, parent: u32, pid: u32) -> Files {
        let files = Files::of(dir, conversation, Some(parent));
        files.register_as(pid);
        FRONTS_FOR_TEST.with(|f| f.borrow_mut().insert(pid, parent));
        files
    }

    /// The hook, run under the shell 777 under Claude Code's process 4242 under 1.
    fn hook(dir: &Path, input: &str) {
        record(dir, input, &[777, 4242, 1]);
    }

    #[test]
    fn the_hooks_mode_is_the_conversations_until_it_changes() {
        let dir = scratch("changes");
        let files = front(&dir, Some("abc-123"));
        assert!(!files.auto(), "no hook has run");
        hook(&dir, &input("abc-123", "auto"));
        assert!(files.auto());
        hook(&dir, &input("abc-123", "default"));
        assert!(!files.auto(), "the user left auto mode");
        hook(&dir, &input("abc-123", "bypassPermissions"));
        assert!(files.auto());
        for mode in ["acceptEdits", "plan", "dontAsk", ""] {
            hook(&dir, &input("abc-123", mode));
            assert!(!files.auto(), "{mode} asks");
        }
    }

    #[test]
    fn after_clear_the_new_conversations_mode_reaches_the_same_front() {
        // `/clear`: a new session id in the hook, the old one in the front's environment (#505).
        let dir = scratch("clear");
        let files = front(&dir, Some("old"));
        hook(&dir, &input("old", "default"));
        assert!(!files.auto());
        hook(&dir, &input("new", "auto"));
        assert!(files.auto(), "the process's file, written last, says auto");
        hook(&dir, &input("new", "default"));
        assert!(!files.auto(), "and then that it left auto mode");
    }

    #[test]
    fn a_hook_under_another_claude_code_writes_only_its_own() {
        let dir = scratch("others");
        let files = front(&dir, None);
        record(&dir, &input("theirs", "auto"), &[778, 9999, 1]);
        assert!(!files.auto(), "another process's hook");
        assert!(!process_file(&dir, 9999).exists(), "makes no process file of its own");
        assert!(!Files::of(&dir, None, None).auto(), "a front that knows neither asks");
        hook(&dir, &input("theirs", "auto"));
        assert!(files.auto(), "one under this front's Claude Code");
        // A dead front's file under the pid a shell between has now is passed over.
        Files::of(&dir, None, Some(777)).register_as(50_001);
        hook(&dir, &input("theirs", "default"));
        assert!(!files.auto(), "this front heard it left auto mode");
        // A Claude Code started from this one's shell (777) writes its own front's file only.
        let inner = front_under(&dir, None, 777, 50_002);
        hook(&dir, &input("inner", "auto"));
        assert!(inner.auto() && !files.auto(), "the inner one's auto mode isn't the outer one's");
        assert!(!front(&dir, None).auto(), "a front that starts later begins with no mode");
    }

    #[test]
    fn the_plugins_hook_matches_its_own_servers_tools() {
        // Claude Code names a plugin's MCP tools mcp__plugin_<plugin>_<server>__<tool>.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../claude-plugin");
        let read = |file: &str| serde_json::from_str::<Value>(&std::fs::read_to_string(root.join(file)).unwrap()).unwrap();
        let plugin = read(".claude-plugin/plugin.json")["name"].as_str().unwrap().to_owned();
        let servers = read(".mcp.json")["mcpServers"].as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(servers.len(), 1, "{servers:?}");
        let hooks = read("hooks/hooks.json");
        let matcher = hooks["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(matcher, format!("mcp__plugin_{plugin}_{}__.*", servers[0]));
        assert!(hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("--hook"));
    }

    #[test]
    fn another_conversations_mode_isnt_this_ones() {
        let dir = scratch("other");
        record(&dir, &input("other", "auto"), &[]);
        assert!(!Files::of(&dir, Some("mine"), None).auto());
        assert!(!Files::of(&dir, None, None).auto(), "a front that doesn't know its conversation asks");
    }

    #[test]
    fn an_old_mode_doesnt_count() {
        let dir = scratch("old");
        let files = front(&dir, Some("abc"));
        hook(&dir, &input("abc", "auto"));
        let old = SystemTime::now() - FRESH - Duration::from_secs(5);
        for file in [dir.join("abc"), process_file(&dir, 4242)] {
            std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        }
        assert!(!files.auto(), "a hook that stopped running leaves no auto mode behind");
    }

    #[test]
    fn what_cant_name_a_file_or_isnt_hook_input_is_left_alone() {
        let dir = scratch("bad");
        for bad in [input("../escape", "auto"), input("a/b", "auto"), input("", "auto"), "not json".to_owned(), "{}".to_owned()] {
            record(&dir, &bad, &[]);
        }
        assert_eq!(std::fs::read_dir(&dir).map_or(0, |d| d.count()), 0);
        let files = front(&dir, Some("abc"));
        hook(&dir, &input("abc", "auto\nmore"));
        assert!(!files.auto(), "a mode that isn't a word is none");
    }
}

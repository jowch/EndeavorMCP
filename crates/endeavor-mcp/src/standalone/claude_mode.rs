//! Claude Code's own permission mode, for the front it started. In auto mode the user has said not to
//! be asked, so a Julia run there adds a new package without the install card, as in the app's Auto
//! mode (#397). Claude Code tells an MCP server nothing of its mode, but it gives it to its hooks: the
//! plugin's PreToolUse hook (`endeavor hook`, run before each of Endeavor's tool calls) writes the
//! conversation's mode to a file named by the conversation, and the front, which knows its
//! conversation from `CLAUDE_CODE_SESSION_ID`, reads it as it passes each call on
//! (`X-Endeavor-Mode: auto`, `mcp::Caller::auto_mode`).
//!
//! Anything missing, unreadable or old is not auto, so the user is asked as before: a hook that didn't
//! run, a client that isn't Claude Code, a conversation whose id changed. The hook writes before every
//! call, so a file written more than `FRESH` ago doesn't say what the mode is now.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// How long after the hook wrote it a mode still counts. The hook runs just before the call, and in
/// auto mode only Claude Code's own check of the call comes between.
const FRESH: Duration = Duration::from_secs(120);

/// A conversation's file is removed this long after it was last written.
const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// The modes in which the user isn't asked: auto, and bypassPermissions, which asks for nothing.
fn skips_asking(mode: &str) -> bool {
    matches!(mode, "auto" | "bypassPermissions")
}

/// A conversation id that can name a file: what `session_key` takes too.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Where the front reads its conversation's mode: None when it doesn't know its conversation.
pub(super) fn file_for(dir: &Path, conversation: Option<&str>) -> Option<PathBuf> {
    let id = conversation?.trim();
    valid_id(id).then(|| dir.join(id))
}

/// Whether the mode in `file` (from `file_for`) skips asking, as the hook wrote it lately.
pub(super) fn auto(file: Option<&Path>) -> bool {
    let Some(file) = file else { return false };
    let fresh = std::fs::metadata(file).and_then(|m| m.modified()).is_ok_and(|written| SystemTime::now().duration_since(written).is_ok_and(|age| age <= FRESH));
    fresh && std::fs::read_to_string(file).is_ok_and(|mode| skips_asking(mode.trim()))
}

/// `endeavor hook`: Claude Code's PreToolUse hook input on stdin. Writes the conversation's mode and
/// prints nothing, so the call goes on as it would have. It never fails the call: what it can't read or
/// write is left alone.
pub fn hook_main() -> ! {
    let mut input = String::new();
    let _ = std::io::stdin().take(4 << 20).read_to_string(&mut input);
    record(&crate::paths::Env::here().claude_modes_dir(), &input);
    std::process::exit(0)
}

/// Write the mode in the hook input `input` to the conversation's file in `dir`.
pub(super) fn record(dir: &Path, input: &str) {
    let Ok(input) = serde_json::from_str::<Value>(input) else { return };
    let Some(file) = file_for(dir, input["session_id"].as_str()) else { return };
    let mode = input["permission_mode"].as_str().unwrap_or_default();
    let mode = if mode.len() <= 32 && mode.chars().all(|c| c.is_ascii_alphanumeric()) { mode } else { "" };
    if crate::make_state_dir(dir).is_err() {
        return;
    }
    let new = !file.exists();
    // Written whole and then renamed, so the front never reads half a file.
    let partial = dir.join(format!(".{}.{}", file.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let written = crate::owner_only(std::fs::OpenOptions::new().write(true).create(true).truncate(true)).open(&partial).and_then(|mut f| std::io::Write::write_all(&mut f, mode.as_bytes()));
    if written.is_err() || std::fs::rename(&partial, &file).is_err() {
        let _ = std::fs::remove_file(&partial);
        return;
    }
    if new {
        forget_old(dir);
    }
}

/// Remove the files of conversations not seen for `KEPT`.
fn forget_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|written| SystemTime::now().duration_since(written).is_ok_and(|age| age > KEPT));
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

    #[test]
    fn the_hooks_mode_is_the_conversations_until_it_changes() {
        let dir = scratch("changes");
        let file = file_for(&dir, Some("abc-123"));
        assert!(!auto(file.as_deref()), "no hook has run");
        record(&dir, &input("abc-123", "auto"));
        assert!(auto(file.as_deref()));
        record(&dir, &input("abc-123", "default"));
        assert!(!auto(file.as_deref()), "the user left auto mode");
        record(&dir, &input("abc-123", "bypassPermissions"));
        assert!(auto(file.as_deref()));
        for mode in ["acceptEdits", "plan", "dontAsk", ""] {
            record(&dir, &input("abc-123", mode));
            assert!(!auto(file.as_deref()), "{mode} asks");
        }
    }

    #[test]
    fn another_conversations_mode_isnt_this_ones() {
        let dir = scratch("other");
        record(&dir, &input("other", "auto"));
        assert!(!auto(file_for(&dir, Some("mine")).as_deref()));
        assert!(!auto(file_for(&dir, None).as_deref()), "a front that doesn't know its conversation asks");
    }

    #[test]
    fn an_old_mode_doesnt_count() {
        let dir = scratch("old");
        record(&dir, &input("abc", "auto"));
        let file = file_for(&dir, Some("abc")).unwrap();
        let old = SystemTime::now() - FRESH - Duration::from_secs(5);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
        assert!(!auto(Some(&file)), "a hook that stopped running leaves no auto mode behind");
    }

    #[test]
    fn what_cant_name_a_file_or_isnt_hook_input_is_left_alone() {
        let dir = scratch("bad");
        for bad in [input("../escape", "auto"), input("a/b", "auto"), input("", "auto"), "not json".to_owned(), "{}".to_owned()] {
            record(&dir, &bad);
        }
        assert_eq!(std::fs::read_dir(&dir).map_or(0, |d| d.count()), 0);
        record(&dir, &input("abc", "auto\nmore"));
        assert!(!auto(file_for(&dir, Some("abc")).as_deref()), "a mode that isn't a word is none");
    }
}

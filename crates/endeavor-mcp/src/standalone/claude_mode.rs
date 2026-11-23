use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

const FRESH: Duration = Duration::from_secs(30);

const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

#[cfg(unix)]
const ANCESTORS: usize = 8;

fn skips_asking(mode: &str) -> bool {
    matches!(mode, "auto" | "bypassPermissions")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn conversation_file(dir: &Path, conversation: Option<&str>) -> Option<PathBuf> {
    let id = conversation?.trim();
    valid_id(id).then(|| dir.join(id))
}

fn process_file(dir: &Path, pid: u32) -> PathBuf {
    dir.join("by-process").join(pid.to_string())
}

fn front_file(process_file: &Path) -> PathBuf {
    process_file.with_extension("front")
}

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
    static FRONTS_FOR_TEST: std::cell::RefCell<std::collections::HashMap<u32, u32>> = Default::default();
}

#[derive(Default)]
pub(super) struct Files {
    conversation: Option<PathBuf>,
    process: Option<PathBuf>,
}

impl Files {
    pub(super) fn of(dir: &Path, conversation: Option<&str>, parent: Option<u32>) -> Files {
        Files { conversation: conversation_file(dir, conversation), process: parent.filter(|&pid| pid > 1).map(|pid| process_file(dir, pid)) }
    }

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

    pub(super) fn auto(&self) -> bool {
        let written = |file: &Path| std::fs::metadata(file).and_then(|m| m.modified()).ok().map(|at| (at, file.to_owned()));
        let last = [&self.conversation, &self.process].into_iter().flatten().filter_map(|f| written(f)).max_by_key(|(at, _)| *at);
        let Some((at, file)) = last else { return false };
        let fresh = SystemTime::now().duration_since(at).is_ok_and(|age| age <= FRESH);
        fresh && std::fs::read_to_string(file).is_ok_and(|mode| skips_asking(mode.trim()))
    }
}

pub fn hook_main() -> ! {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    record(&crate::paths::Env::here().claude_modes_dir(), &input, &ancestors());
    std::process::exit(0)
}

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

pub(super) fn record(dir: &Path, input: &str, ancestors: &[u32]) {
    let Ok(input) = serde_json::from_str::<Value>(input) else { return };
    let mode = input["permission_mode"].as_str().unwrap_or_default();
    let mode = if mode.len() <= 32 && mode.chars().all(|c| c.is_ascii_alphanumeric()) { mode } else { "" };
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

    fn front(dir: &Path, conversation: Option<&str>) -> Files {
        front_under(dir, conversation, 4242, 50_000)
    }

    fn front_under(dir: &Path, conversation: Option<&str>, parent: u32, pid: u32) -> Files {
        let files = Files::of(dir, conversation, Some(parent));
        files.register_as(pid);
        FRONTS_FOR_TEST.with(|f| f.borrow_mut().insert(pid, parent));
        files
    }

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
        Files::of(&dir, None, Some(777)).register_as(50_001);
        hook(&dir, &input("theirs", "default"));
        assert!(!files.auto(), "this front heard it left auto mode");
        let inner = front_under(&dir, None, 777, 50_002);
        hook(&dir, &input("inner", "auto"));
        assert!(inner.auto() && !files.auto(), "the inner one's auto mode isn't the outer one's");
        assert!(!front(&dir, None).auto(), "a front that starts later begins with no mode");
    }

    #[test]
    fn the_plugins_hook_matches_its_own_servers_tools() {
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

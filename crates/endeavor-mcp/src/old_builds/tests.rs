use super::*;

use std::cell::Cell;

const DAY: Duration = Duration::from_secs(24 * 3600);

/// Held by a test that starts a process and by one that lets go of a lease: a process started while
/// the lease is held has its file, and with it the lock, until it runs its program.
static SPAWNS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("old-builds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A server build `name` in `base`, as the bootstrap script installs it; with a lease marker if `marked`.
fn server_build(base: &Path, name: &str, marked: bool) -> PathBuf {
    let folder = base.join(name);
    std::fs::create_dir_all(folder.join("runtime")).unwrap();
    std::fs::write(folder.join("endeavor"), "").unwrap();
    std::fs::write(folder.join("runtime/boot.jl"), "").unwrap();
    if marked {
        std::fs::write(folder.join(standalone::IN_USE), "").unwrap();
    }
    folder
}

fn plugin_build(base: &Path, key: &str) -> PathBuf {
    let folder = base.join(key);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("endeavor"), "").unwrap();
    folder
}

fn listing(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(base).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

fn nothing_runs() -> Option<Vec<String>> {
    Some(Vec::new())
}

#[test]
fn a_server_build_nothing_used_for_a_week_goes_and_everything_else_stays() {
    let base = scratch("server");
    let place = Place::Server { base: base.clone(), own: "0.1.0-own".into() };
    server_build(&base, "0.1.0-own", true);
    server_build(&base, "0.1.0-old", true);
    server_build(&base, "0.1.0-unmarked", false);
    // Folders that aren't builds share the root: the depot, `serve`'s state, the helpers' cache.
    for other in ["depot/packages", "serve", "helpers/abc/linux-x86_64"] {
        std::fs::create_dir_all(base.join(other)).unwrap();
    }
    std::fs::create_dir_all(base.join("0.1.0-cut.part.123")).unwrap();
    std::fs::create_dir_all(base.join("0.1.0-gone.removing.9")).unwrap();

    let removed = remove_unused(&place, SystemTime::now() + DAY / 2, &nothing_runs);
    assert!(removed.is_empty(), "{removed:?}");
    assert_eq!(listing(&base), ["0.1.0-cut.part.123", "0.1.0-old", "0.1.0-own", "0.1.0-unmarked", "depot", "helpers", "serve"], "a folder being removed is finished; an install may still be writing");
    let removed = remove_unused(&place, SystemTime::now() + 2 * DAY, &nothing_runs);
    assert!(removed.is_empty(), "two days isn't enough: {removed:?}");
    assert!(!base.join("0.1.0-cut.part.123").exists(), "a half-written install a day old goes");

    let mut removed = remove_unused(&place, SystemTime::now() + 8 * DAY, &nothing_runs);
    removed.sort();
    assert_eq!(removed, ["0.1.0-old", "0.1.0-unmarked"]);
    assert_eq!(listing(&base), ["0.1.0-own", "depot", "helpers", "serve"]);
}

#[test]
fn a_build_someone_holds_or_used_lately_stays() {
    let base = scratch("held");
    let place = Place::Server { base: base.clone(), own: "0.1.0-own".into() };
    let held = server_build(&base, "0.1.0-held", true);
    let used = server_build(&base, "0.1.0-used", true);
    let _spawns = SPAWNS.lock().unwrap_or_else(|e| e.into_inner());
    let lease = standalone::lease(&held).unwrap();
    let later = SystemTime::now() + 8 * DAY;
    std::fs::File::options().write(true).open(used.join(standalone::IN_USE)).unwrap().set_modified(later - DAY).unwrap();

    assert!(remove_unused(&place, later, &nothing_runs).is_empty());
    drop(lease);
    assert_eq!(remove_unused(&place, later, &nothing_runs), ["0.1.0-held"]);
}

#[test]
fn a_build_a_process_or_a_job_names_stays_and_not_knowing_keeps_them_all() {
    let base = scratch("named");
    let place = Place::Server { base: base.clone(), own: "0.1.0-own".into() };
    let older = server_build(&base, "0.1.0-a", true);
    server_build(&base, "0.1.0-ab", true);
    let later = SystemTime::now() + 8 * DAY;

    assert!(remove_unused(&place, later, &|| None).is_empty(), "can't tell");
    let julia = format!("julia\n--project={}/runtime\n{}/runtime/boot.jl", older.display(), older.display());
    assert_eq!(remove_unused(&place, later, &|| Some(vec![julia.clone()])), ["0.1.0-ab"], "a name that only starts the same isn't the build's");
    let script = format!("#!/bin/sh\nexec '{}/endeavor' node-start --state-dir '/home/ada/.local/state/endeavor/cluster'\n", older.display());
    assert!(remove_unused(&place, later, &|| Some(vec![script.clone()])).is_empty(), "a job script");
    assert!(base.join("0.1.0-a").is_dir());
}

#[test]
fn what_runs_is_asked_only_when_a_build_is_old_enough_to_go() {
    let base = scratch("lazy");
    let place = Place::Server { base: base.clone(), own: "0.1.0-own".into() };
    server_build(&base, "0.1.0-a", true);
    let asked = Cell::new(0);
    let ask = || {
        asked.set(asked.get() + 1);
        Some(Vec::new())
    };
    remove_unused(&place, SystemTime::now(), &ask);
    assert_eq!(asked.get(), 0);
    server_build(&base, "0.1.0-b", true);
    remove_unused(&place, SystemTime::now() + 8 * DAY, &ask);
    assert_eq!(asked.get(), 1, "once for all of them");
}

#[test]
fn the_plugin_keeps_the_newest_it_fetched() {
    let base = scratch("plugin");
    let place = Place::Plugin { base: base.clone(), own: "aaaa".into() };
    for key in ["aaaa", "bbbb", "cccc"] {
        plugin_build(&base, key);
    }
    std::fs::write(base.join(".newest"), "cccc\n").unwrap();
    std::fs::create_dir_all(base.join(".lock")).unwrap();
    assert_eq!(remove_unused(&place, SystemTime::now() + 8 * DAY, &nothing_runs), ["bbbb"]);
    assert_eq!(listing(&base), [".lock", ".newest", "aaaa", "cccc"]);
}

#[test]
fn a_binary_is_placed_by_where_it_runs_from() {
    let dir = scratch("place");
    let plugin_bin = dir.join("share/endeavor/bin");
    let exe = plugin_build(&plugin_bin, "aaaa").join("endeavor");
    assert_eq!(Place::of(&exe, &plugin_bin, "0.1.0-own"), Some(Place::Plugin { base: plugin_bin.clone(), own: "aaaa".into() }));
    let from = dir.join("share/endeavor/bin-from/123-45");
    let exe = plugin_build(&from, "bbbb").join("endeavor");
    assert_eq!(Place::of(&exe, &plugin_bin, "0.1.0-own"), Some(Place::Plugin { base: from, own: "bbbb".into() }));

    let root = dir.join("cache/endeavor");
    let exe = server_build(&root, "0.1.0-own", false).join("endeavor");
    assert_eq!(Place::of(&exe, &plugin_bin, "0.1.0-own"), Some(Place::Server { base: root.clone(), own: "0.1.0-own".into() }));
    assert_eq!(Place::of(&exe, &plugin_bin, "0.1.0-other"), None, "a folder named for another build isn't this one's");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin/endeavor"), "").unwrap();
    assert_eq!(Place::of(&dir.join("bin/endeavor"), &plugin_bin, "0.1.0-own"), None, "installed anywhere else");
}

#[test]
fn holding_its_own_folder_marks_it() {
    let base = scratch("hold");
    server_build(&base, "0.1.0-own", false);
    hold_own_folder(Some(&Place::Server { base: base.clone(), own: "0.1.0-own".into() }));
    let marker = std::fs::File::open(base.join("0.1.0-own").join(standalone::IN_USE)).unwrap();
    assert!(marker.try_lock().is_err(), "held");
}

#[cfg(unix)]
#[test]
fn a_running_process_that_names_a_file_in_a_folder_is_seen() {
    let base = scratch("process");
    let folder = server_build(&base, "0.1.0-run", true);
    let _spawns = SPAWNS.lock().unwrap_or_else(|e| e.into_inner());
    // Naming a file in the folder, as Julia names the runtime's boot.jl.
    let mut child = std::process::Command::new("sh").args(["-c", "sleep 30; :", "sh"]).arg(folder.join("runtime/boot.jl")).spawn().unwrap();
    // The process may not show its own command line at once.
    let started = std::time::Instant::now();
    let mut seen = false;
    while !seen && started.elapsed() < Duration::from_secs(5) {
        seen = names(&processes().unwrap(), &folder);
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(seen, "{folder:?} isn't among the processes");
}

/// A `runtime.json` in `folder` for a runtime on `node` with `pid`, from `build` if one is given.
fn record(folder: &Path, node: &str, pid: u32, build: Option<&str>) {
    std::fs::create_dir_all(folder).unwrap();
    // With this process's start time, which Windows needs to count a pid as alive; for another pid it doesn't match, as for a process that ended.
    let mut state = serde_json::json!({ "launcher": "process", "node": node, "pid": pid, "token": "t", "started": crate::own_start_time() });
    if let Some(build) = build {
        state["build"] = build.into();
    }
    std::fs::write(folder.join("runtime.json"), state.to_string()).unwrap();
}

#[test]
fn a_build_a_runtime_record_names_stays_wherever_the_runtime_runs() {
    let dir = scratch("records");
    let base = dir.join("cache/endeavor");
    let place = Place::Server { base: base.clone(), own: "0.1.0-own".into() };
    for name in ["0.1.0-own", "0.1.0-other-node", "0.1.0-here", "0.1.0-gone"] {
        server_build(&base, name, false);
    }
    let serve = dir.join("state/endeavor/serve");
    // A runtime on another login node, which this one can't see; one alive here; one here that ended without removing its record.
    record(&serve.join("login2"), "login2", 4242, Some("0.1.0-other-node"));
    record(&base.join("cluster-1"), "login1", std::process::id(), Some("0.1.0-here"));
    record(&serve.join("login1"), "login1", 0, Some("0.1.0-gone"));
    let folders = vec![serve.join("login1"), serve.join("login2"), base.join("cluster-1"), dir.join("state/endeavor/cluster")];
    let texts = recorded(&folders, &base, "login1").unwrap();
    let mut removed = remove_unused(&place, SystemTime::now() + 8 * DAY, &|| Some(texts.clone()));
    removed.sort();
    assert_eq!(removed, ["0.1.0-gone"]);

    // A record from before builds were recorded may be any of them.
    record(&serve.join("login3"), "login3", 4242, None);
    assert_eq!(recorded(&[serve.join("login3")], &base, "login1"), None);
}

#[test]
fn the_record_folders_are_the_roots_folders_each_nodes_and_the_clusters() {
    let dir = scratch("record-folders");
    let base = dir.join("cache/endeavor");
    std::fs::create_dir_all(base.join("cluster-1")).unwrap();
    let env = paths::Env { home: dir.clone(), node: "login1".into(), ..paths::Env::default() };
    std::fs::create_dir_all(env.state_dir().with_file_name("login2")).unwrap();
    let folders = record_folders(&base, &env);
    for folder in [base.join("cluster-1"), env.state_dir().with_file_name("login2"), env.cluster_state_dir()] {
        assert!(folders.contains(&folder), "{folder:?} not in {folders:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_marker_whose_folder_was_moved_away_is_told_from_one_still_there() {
    let base = scratch("moved");
    let folder = server_build(&base, "0.1.0-a", true);
    let path = folder.join(standalone::IN_USE);
    let marker = std::fs::File::open(&path).unwrap();
    assert!(same_file(&marker, &path));
    std::fs::rename(&folder, base.join("0.1.0-a.removing.1")).unwrap();
    assert!(!same_file(&marker, &path), "gone");
    server_build(&base, "0.1.0-a", true);
    assert!(!same_file(&marker, &path), "installed again: another file");
}

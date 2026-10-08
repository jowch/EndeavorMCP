//! `scripts/endeavor-mcp.sh`, the plugin's MCP command, run as an agent runs
//! it, against a release that is a folder under `target/tmp`
//! (`ENDEAVOR_RELEASE_URL=file://…`), with `HOME` and the XDG variables there
//! too. The "binary" is a script that prints its arguments and a JSON line.
//! `ENDEAVOR_TEST_SH` names the shell that runs the scripts (default `sh`).
//! Because `ENDEAVOR_RELEASE_URL` is set, the launcher keeps its binaries in
//! `bin-from/<checksum of the URL>/`, which `Place::data` names.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";
const OTHER: &str = "ba9876543210";
const BIN: &[u8] = b"#!/bin/sh\necho \"args: $*\"\necho '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\n";
const MANUAL: &str = "endeavor-mcp.sh\" --fetch-only";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn platform() -> String {
    let os = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts")
}

struct Place {
    dir: PathBuf,
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Place {
    /// A release with nothing on it, and a plugin folder holding the scripts.
    fn new(name: &str) -> Place {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("launcher-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["release", "home", "plugin/launch", "fake"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for file in ["endeavor-mcp.sh", "install.sh", "release-key"] {
            std::fs::copy(scripts().join(file), dir.join("plugin/launch").join(file)).unwrap();
        }
        Place { dir }
    }

    /// Publish `bin` as build `key` for `platform`, and `LATEST` as `latest`.
    fn publish(&self, key: &str, platform: &str, suffix: &str, bin: &[u8], sum: &str) {
        let name = format!("endeavor-{key}-{platform}{suffix}");
        std::fs::write(self.dir.join(format!("release/endeavor-{key}.sha256")), format!("{}  endeavor-{key}-other\n{sum}  {name}\n", sha(b"other"))).unwrap();
        std::fs::write(self.dir.join("release").join(name), bin).unwrap();
    }

    fn release(&self, key: &str) {
        self.publish(key, &platform(), "", BIN, &sha(BIN));
        std::fs::write(self.dir.join("release/LATEST"), format!("{key}\n")).unwrap();
    }

    fn pin(&self, key: &str) {
        std::fs::write(self.dir.join("plugin/launch/release-key"), format!("{key}\n")).unwrap();
    }

    fn data(&self) -> PathBuf {
        self.data_under(&self.dir.join("home/.local/share"))
    }

    fn data_under(&self, share: &Path) -> PathBuf {
        let mut cksum = Command::new("cksum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        cksum.stdin.take().unwrap().write_all(self.url().as_bytes()).unwrap();
        let out = cksum.wait_with_output().unwrap();
        share.join("endeavor/bin-from").join(String::from_utf8(out.stdout).unwrap().trim().replace(' ', "-"))
    }

    /// The folder a start with no `ENDEAVOR_RELEASE_URL` uses.
    fn default_data(&self) -> PathBuf {
        self.dir.join("home/.local/share/endeavor/bin")
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.join("release").display().to_string().replace(' ', "%20"))
    }

    fn fake(&self, name: &str, body: &str) {
        let path = self.dir.join("fake").join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, script: &str, args: &[&str], url: &str) -> Command {
        let shell = std::env::var("ENDEAVOR_TEST_SH").unwrap_or_else(|_| "sh".into());
        let path = format!("{}:{}", self.dir.join("fake").display(), std::env::var("PATH").unwrap());
        let mut words = shell.split_whitespace();
        let mut command = Command::new(words.next().unwrap());
        command
            .args(words)
            .arg(self.dir.join("plugin/launch").join(script))
            .args(args)
            .current_dir(&self.dir)
            .env("HOME", self.dir.join("home"))
            .env("XDG_DATA_HOME", self.dir.join("home/.local/share"))
            .env("XDG_CACHE_HOME", self.dir.join("home/.cache"))
            .env("XDG_CONFIG_HOME", self.dir.join("home/.config"))
            .env("XDG_STATE_HOME", self.dir.join("home/.local/state"))
            .env("PATH", path)
            .env("ENDEAVOR_RELEASE_URL", url)
            .env_remove("ENDEAVOR_BIN")
            .env_remove("ENDEAVOR_INSTALL_DIR");
        command
    }

    fn launch(&self, args: &[&str]) -> Run {
        self.finish(self.command("endeavor-mcp.sh", args, &self.url()).stdin(Stdio::null()).output().unwrap())
    }

    fn launch_with(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut command = self.command("endeavor-mcp.sh", args, &self.url());
        for (name, value) in env {
            command.env(name, value);
        }
        self.finish(command.stdin(Stdio::null()).output().unwrap())
    }

    /// The same release, with a `curl` that fails ahead of the real one.
    fn offline(&self, args: &[&str]) -> Run {
        let down = self.dir.join("offline");
        std::fs::create_dir_all(&down).unwrap();
        let curl = down.join("curl");
        std::fs::write(&curl, format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 7\n", self.dir.join("fake/curl.log").display())).unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}:{}", down.display(), self.dir.join("fake").display(), std::env::var("PATH").unwrap());
        self.launch_with(args, &[("PATH", &path)])
    }

    fn install(&self, args: &[&str]) -> Run {
        self.finish(self.command("install.sh", args, &self.url()).stdin(Stdio::null()).output().unwrap())
    }

    fn finish(&self, out: Output) -> Run {
        Run { ok: out.status.success(), stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
    }

    /// A `curl` that logs its arguments to fake/curl.log, then runs the real one.
    fn logging_curl(&self, before: &str) {
        let real = std::env::var("PATH").unwrap().split(':').map(|d| Path::new(d).join("curl")).find(|p| p.exists()).expect("curl");
        self.fake("curl", &format!("#!/bin/sh\necho \"$*\" >> '{}'\n{before}\nexec {} \"$@\"\n", self.dir.join("fake/curl.log").display(), real.display()));
    }

    fn downloads_of_the_binary(&self) -> usize {
        std::fs::read_to_string(self.dir.join("fake/curl.log")).unwrap_or_default().lines().filter(|l| l.contains(&format!("endeavor-{KEY}-{}", platform())) && !l.contains("sha256")).count()
    }
}

fn expected(args: &str) -> String {
    format!("args: mcp{}{args}\n{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}\n", if args.is_empty() { "" } else { " " })
}

#[test]
fn it_fetches_the_build_then_runs_it_and_stdout_holds_only_what_it_printed() {
    let place = Place::new("fetch");
    place.release(KEY);
    let run = place.launch(&["--skills", "plugin", "--folder", "/some project"]);
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout, expected("--skills plugin --folder /some project"));
    let bin = place.data().join(KEY).join("endeavor");
    assert_eq!(std::fs::read(&bin).unwrap(), BIN);
    assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777, 0o755);
    let mut left: Vec<String> = std::fs::read_dir(place.data()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, [".checked", ".newest", KEY], "no lock or temporary file is left");
}

#[test]
fn the_second_start_downloads_nothing_even_with_the_network_gone() {
    let place = Place::new("second");
    place.logging_curl("");
    place.release(KEY);
    assert!(place.launch(&[]).ok);
    assert_eq!(place.downloads_of_the_binary(), 1);
    let again = place.launch(&["--folder", "x"]);
    assert!(again.ok && again.stdout == expected("--folder x"), "{}", again.stderr);
    let offline = place.offline(&[]);
    assert!(offline.ok && offline.stdout == expected(""), "{}", offline.stderr);
    assert_eq!(place.downloads_of_the_binary(), 1);
}

#[test]
fn a_pinned_key_is_fetched_whatever_latest_says_and_never_replaced() {
    let place = Place::new("pinned");
    place.release(KEY);
    place.publish(OTHER, &platform(), "", BIN, &sha(BIN));
    place.pin(OTHER);
    let run = place.launch(&[]);
    assert!(run.ok, "{}", run.stderr);
    assert!(place.data().join(OTHER).join("endeavor").exists() && !place.data().join(KEY).exists());
    assert!(!place.data().join(".newest").exists(), "a pinned fetch doesn't change what unpinned starts use");
    assert!(place.offline(&[]).ok);
}

#[test]
fn unpinned_it_runs_the_newest_it_has_and_fetch_only_looks_for_a_newer_one() {
    let place = Place::new("newest");
    place.release(KEY);
    assert!(place.launch(&[]).ok);
    place.publish(OTHER, &platform(), "", BIN, &sha(BIN));
    std::fs::write(place.dir.join("release/LATEST"), format!("{OTHER}\n")).unwrap();
    assert!(place.launch(&[]).ok);
    assert!(!place.data().join(OTHER).exists(), "a start doesn't ask the network when it has a build");
    let fetched = place.launch(&["--fetch-only"]);
    assert!(fetched.ok && fetched.stdout.is_empty() && fetched.stderr.is_empty(), "{fetched:?}", fetched = (fetched.stdout, fetched.stderr));
    assert!(place.data().join(OTHER).join("endeavor").exists());
    assert_eq!(std::fs::read_to_string(place.data().join(".newest")).unwrap().trim(), OTHER);
    // Network down: fetch-only keeps what it has.
    assert!(place.offline(&["--fetch-only"]).ok);
    assert_eq!(place.launch(&[]).stdout, expected(""));
}

#[test]
fn with_no_network_and_no_binary_it_exits_non_zero_with_one_plain_line() {
    let place = Place::new("down");
    let run = place.offline(&["--skills", "plugin"]);
    assert!(!run.ok);
    assert_eq!(run.stdout, "");
    let last = run.stderr.lines().last().unwrap();
    assert!(last.starts_with("endeavor: couldn't get endeavor") && last.contains(MANUAL), "{}", run.stderr);
    assert!(!place.data().join(".lock").exists());
}

#[test]
fn a_download_that_fails_its_checksum_is_refused_and_leaves_no_binary() {
    let place = Place::new("checksum");
    place.publish(KEY, &platform(), "", BIN, &sha(b"something else"));
    std::fs::write(place.dir.join("release/LATEST"), KEY).unwrap();
    let run = place.launch(&[]);
    assert!(!run.ok && run.stdout.is_empty());
    assert!(run.stderr.contains("doesn't match its checksum") && run.stderr.contains(MANUAL), "{}", run.stderr);
    assert!(!place.data().join(KEY).join("endeavor").exists());
}

#[test]
fn two_starts_at_once_download_once_and_both_run_a_complete_binary() {
    let place = Place::new("twice");
    place.logging_curl("sleep 1");
    place.release(KEY);
    let start = || place.command("endeavor-mcp.sh", &["--folder", "p"], &place.url()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let (a, b) = (start(), start());
    for out in [a.wait_with_output().unwrap(), b.wait_with_output().unwrap()] {
        let run = place.finish(out);
        assert!(run.ok, "{}", run.stderr);
        assert_eq!(run.stdout, expected("--folder p"));
    }
    assert_eq!(place.downloads_of_the_binary(), 1);
    assert_eq!(std::fs::read(place.data().join(KEY).join("endeavor")).unwrap(), BIN);
}

#[test]
fn a_download_cut_short_leaves_no_binary_and_the_next_start_gets_it_cleanly() {
    let place = Place::new("cut");
    place.release(KEY);
    // The asset's download writes half a file, and the install script dies.
    place.logging_curl(&format!("case \"$*\" in *{}-{}*) head -c 10 '{}/release/endeavor-{KEY}-{}' > \"$(echo \"$*\" | sed 's/.*-o \\([^ ]*\\) .*/\\1/')\"; kill -9 $PPID;; esac", KEY, platform(), place.dir.display(), platform()));
    let run = place.launch(&[]);
    assert!(!run.ok && run.stdout.is_empty());
    assert!(!place.data().join(KEY).join("endeavor").exists(), "{}", run.stderr);
    assert!(!place.data().join(".lock").exists());
    let again = place.launch_with(&[], &[]);
    assert!(!again.ok);
    std::fs::remove_file(place.dir.join("fake/curl")).unwrap();
    let ok = place.launch(&[]);
    assert!(ok.ok, "{}", ok.stderr);
    assert_eq!(ok.stdout, expected(""));
    assert_eq!(std::fs::read(place.data().join(KEY).join("endeavor")).unwrap(), BIN);
}

#[test]
fn a_lock_left_by_a_dead_start_is_taken_over() {
    let place = Place::new("stale");
    place.release(KEY);
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let lock = place.data().join(".lock");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), format!("{dead}\n")).unwrap();
    let run = place.launch(&[]);
    assert!(run.ok, "{}", run.stderr);
    assert!(!lock.exists());
}

#[test]
fn fetch_only_gets_the_binary_and_runs_nothing() {
    let place = Place::new("fetchonly");
    place.release(KEY);
    place.pin(KEY);
    let run = place.launch(&["--fetch-only"]);
    assert!(run.ok && run.stdout.is_empty() && run.stderr.is_empty(), "{}", run.stderr);
    assert!(place.data().join(KEY).join("endeavor").exists());
    let again = place.offline(&["--fetch-only"]);
    assert!(again.ok && again.stdout.is_empty());
}

#[test]
fn the_developer_override_runs_that_binary_without_touching_the_network_or_the_data_folder() {
    let place = Place::new("override");
    let mine = place.dir.join("mine");
    std::fs::write(&mine, BIN).unwrap();
    std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("ENDEAVOR_BIN", mine.to_str().unwrap())];
    let run = place.launch_with(&["--folder", "p"], &env);
    assert!(run.ok && run.stdout == expected("--folder p"), "{}", run.stderr);
    let only = place.launch_with(&["--fetch-only"], &env);
    assert!(only.ok && only.stdout.is_empty());
    assert!(!place.dir.join("home/.local").exists());
    let missing = place.launch_with(&[], &[("ENDEAVOR_BIN", "/nowhere/endeavor")]);
    assert!(!missing.ok && missing.stdout.is_empty() && missing.stderr.contains("ENDEAVOR_BIN"), "{}", missing.stderr);
}

#[test]
fn install_sh_takes_a_key_and_with_quiet_writes_nothing_to_stdout() {
    let place = Place::new("key");
    place.release(KEY);
    place.publish(OTHER, &platform(), "", BIN, &sha(BIN));
    let dir = place.dir.join("bin");
    let run = place.install(&["--key", OTHER, "--dir", dir.to_str().unwrap()]);
    assert!(run.ok, "{}", run.stderr);
    assert!(run.stdout.contains(&format!("build {OTHER}")), "{}", run.stdout);
    assert_eq!(std::fs::read(dir.join("endeavor")).unwrap(), BIN);

    let into = place.dir.join("into");
    let quiet = place.install(&["--quiet", "--key", KEY, "--into", into.to_str().unwrap()]);
    assert!(quiet.ok && quiet.stdout.is_empty(), "{}", quiet.stdout);
    assert!(into.join(KEY).join("endeavor").exists() && !into.join(".newest").exists());

    let bad = place.install(&["--key", "../x"]);
    assert!(!bad.ok && bad.stderr.contains("isn't a build's key"), "{}", bad.stderr);
    let unknown = place.install(&["--key", "ffffffffffff", "--dir", dir.to_str().unwrap()]);
    assert!(!unknown.ok && unknown.stderr.contains("couldn't download"), "{}", unknown.stderr);
}

#[test]
fn git_bash_on_windows_gets_the_exe_for_windows() {
    let place = Place::new("mingw");
    place.fake("uname", "#!/bin/sh\n[ \"$1\" = -s ] && echo MINGW64_NT-10.0-22631 || echo x86_64\n");
    place.publish(KEY, "windows-x86_64", ".exe", BIN, &sha(BIN));
    std::fs::write(place.dir.join("release/LATEST"), KEY).unwrap();
    let dir = place.dir.join("bin");
    let run = place.install(&["--dir", dir.to_str().unwrap()]);
    assert!(run.ok, "{}", run.stderr);
    assert!(run.stdout.contains(&format!("Installed endeavor (build {KEY}, windows-x86_64)")) && dir.join("endeavor.exe").exists(), "{}", run.stdout);

    let launched = place.launch(&["--folder", "p"]);
    assert!(launched.ok && launched.stdout == expected("--folder p"), "{}", launched.stderr);
    assert!(place.data().join(KEY).join("endeavor.exe").exists());
}

const DEFAULT_BIN: &[u8] = b"#!/bin/sh\necho 'default build'\n";

fn real(name: &str) -> PathBuf {
    std::env::var("PATH").unwrap().split(':').map(|d| Path::new(d).join(name)).find(|p| p.exists()).unwrap_or_else(|| panic!("{name} isn't on the PATH"))
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < until, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: &str) -> bool {
    // An ended process whose parent is gone stays a zombie until PID 1 reaps
    // it, and `kill -0` still finds a zombie. Containers whose PID 1 doesn't
    // reap (the cloud VMs' doesn't) keep them, so on Linux a zombie is dead.
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('Z'))
    {
        return false;
    }
    Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
}

impl Place {
    fn set_mode(&self, path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A folder of links to the tools the scripts use, without `without`, and a PATH of it.
    fn path_without(&self, without: &[&str]) -> String {
        let dir = self.dir.join("tools");
        std::fs::create_dir_all(&dir).unwrap();
        for tool in ["sh", "uname", "head", "tr", "cut", "awk", "mkdir", "rm", "mv", "cp", "mktemp", "chmod", "sha256sum", "shasum", "sleep", "cat", "find", "dirname", "cksum", "touch", "sed", "kill", "id", "dash", "bash", "busybox"] {
            if without.contains(&tool) {
                continue;
            }
            if let Some(from) = std::env::var("PATH").unwrap().split(':').map(|d| Path::new(d).join(tool)).find(|p| p.exists()) {
                let _ = std::os::unix::fs::symlink(from, dir.join(tool));
            }
        }
        format!("{}:{}", self.dir.join("fake").display(), dir.display())
    }

    /// A `wget` that serves `file://` URLs, for `install.sh` when there is no `curl`.
    fn fake_wget(&self, slow: &str) {
        self.fake(
            "wget",
            &format!(
                "#!/bin/sh\nwhile [ $# -gt 1 ]; do case $1 in -O) out=$2; shift;; esac; shift; done\nurl=$1\ncase $url in {slow}) echo $$ >> '{pids}'; exec '{sleep}' 30;; esac\nif [ \"$out\" = - ]; then cat \"${{url#file://}}\"; else cp \"${{url#file://}}\" \"$out\"; fi\n",
                pids = self.dir.join("fake/wget.pids").display(),
                sleep = real("sleep").display(),
            ),
        );
    }
}

#[test]
fn a_lock_whose_pid_is_alive_but_old_is_taken_over() {
    let place = Place::new("oldlock");
    place.release(KEY);
    let mut unrelated = Command::new("sleep").arg("60").spawn().unwrap();
    let lock = place.data().join(".lock");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), format!("{}\n", unrelated.id())).unwrap();
    assert!(Command::new("touch").args(["-t", "202001010000"]).arg(&lock).status().unwrap().success());
    let started = Instant::now();
    let run = place.launch(&[]);
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    assert!(run.ok && run.stdout == expected(""), "{}", run.stderr);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(!lock.exists());
}

#[test]
fn starts_that_find_one_stale_lock_download_once_and_all_run() {
    let place = Place::new("stalerace");
    place.logging_curl("sleep 1");
    place.release(KEY);
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let lock = place.data().join(".lock");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), format!("{dead}\n")).unwrap();
    let start = || place.command("endeavor-mcp.sh", &[], &place.url()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let starts = [start(), start()];
    for child in starts {
        let run = place.finish(child.wait_with_output().unwrap());
        assert!(run.ok && run.stdout == expected(""), "{}", run.stderr);
    }
    assert_eq!(place.downloads_of_the_binary(), 1);
    let left: Vec<_> = std::fs::read_dir(place.data()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".lock")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_folder_that_cant_be_written_fails_at_once_naming_it() {
    let place = Place::new("unwritable");
    place.release(KEY);
    place.fake("mkdir", &format!("#!/bin/sh\ncase \"$*\" in *.lock) echo \"mkdir: cannot create directory: Permission denied\" >&2; exit 1;; esac\nexec {} \"$@\"\n", real("mkdir").display()));
    let started = Instant::now();
    let run = place.launch(&[]);
    assert!(!run.ok && run.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    let last = run.stderr.lines().last().unwrap();
    assert!(last.starts_with("endeavor: can't make") && last.contains(&place.data().display().to_string()) && last.contains("Permission denied"), "{}", run.stderr);
}

#[test]
fn a_release_set_by_the_variable_never_uses_or_changes_the_default_folder() {
    let place = Place::new("default");
    place.release(KEY);
    let default = place.default_data();
    std::fs::create_dir_all(default.join(OTHER)).unwrap();
    std::fs::write(default.join(".newest"), format!("{OTHER}\n")).unwrap();
    std::fs::write(default.join(".checked"), "").unwrap();
    let bin = default.join(OTHER).join("endeavor");
    std::fs::write(&bin, DEFAULT_BIN).unwrap();
    place.set_mode(&bin, 0o755);
    let before = std::fs::read_dir(&default).unwrap().count();

    let run = place.launch(&["--folder", "p"]);
    assert!(run.ok && run.stdout == expected("--folder p"), "{}", run.stderr);
    assert!(place.data().join(KEY).join("endeavor").exists());
    assert_eq!(std::fs::read_dir(&default).unwrap().count(), before);
    assert_eq!(std::fs::read_to_string(default.join(".newest")).unwrap().trim(), OTHER);
    // The same override with the network down finds nothing in the default folder.
    let other = place.launch_with(&[], &[("ENDEAVOR_RELEASE_URL", "file:///nowhere")]);
    assert!(!other.ok && other.stdout.is_empty());

    // Without the variable the default folder is what runs, and nothing is asked of the network.
    let mut command = place.command("endeavor-mcp.sh", &[], "unused");
    let out = command.env_remove("ENDEAVOR_RELEASE_URL").stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success() && out.stdout == b"default build\n", "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_newest_file_that_isnt_a_key_is_ignored() {
    let place = Place::new("badnewest");
    place.release(KEY);
    let evil = place.data().join("../evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(evil.join("endeavor"), b"#!/bin/sh\necho evil\n").unwrap();
    place.set_mode(&evil.join("endeavor"), 0o755);
    std::fs::write(place.data().join(".newest"), "../evil\n").unwrap();
    let run = place.launch(&[]);
    assert!(run.ok && run.stdout == expected(""), "{}{}", run.stdout, run.stderr);
    assert_eq!(std::fs::read_to_string(place.data().join(".newest")).unwrap().trim(), KEY);
}

#[test]
fn a_relative_xdg_data_home_is_ignored_and_a_relative_home_is_refused() {
    let place = Place::new("relative");
    place.release(KEY);
    let run = place.launch_with(&[], &[("XDG_DATA_HOME", "rel/share")]);
    assert!(run.ok && run.stdout == expected(""), "{}", run.stderr);
    assert!(place.data().join(KEY).join("endeavor").exists() && !place.dir.join("rel").exists());
    let none = place.launch_with(&[], &[("XDG_DATA_HOME", ""), ("HOME", "home")]);
    assert!(!none.ok && none.stdout.is_empty());
    assert!(none.stderr.lines().last().unwrap().starts_with("endeavor: HOME isn't set to an absolute path"), "{}", none.stderr);
    assert!(!place.dir.join("home/.local/share/endeavor/home").exists() && !place.dir.join(".local").exists());
}

#[test]
fn the_launcher_keeps_its_binaries_where_the_paths_module_says() {
    let cases: [(&str, Option<&str>); 4] = [("absolute", Some("home/xdg-data")), ("unset", None), ("relative", Some("rel/share")), ("empty", Some(""))];
    for (name, xdg) in cases {
        let place = Place::new(&format!("paths-{name}"));
        place.pin("");
        place.fake("curl", "#!/bin/sh\nexit 7\n");
        let xdg = xdg.map(|x| if x.starts_with("home/") { place.dir.join(x).display().to_string() } else { x.to_owned() });
        let home = place.dir.join("home").display().to_string();
        let read = |var: &str| match var {
            "HOME" => Some(home.clone()),
            "XDG_DATA_HOME" => xdg.clone(),
            _ => None,
        };
        let base = endeavor_mcp::paths::Env::from_vars(&read).plugin_bin();
        assert!(base.starts_with(&place.dir), "{base:?}");
        std::fs::create_dir_all(base.join(KEY)).unwrap();
        std::fs::write(base.join(".newest"), format!("{KEY}\n")).unwrap();
        std::fs::write(base.join(".checked"), "").unwrap();
        std::fs::write(base.join(KEY).join("endeavor"), format!("#!/bin/sh\necho 'found in {name}'\n")).unwrap();
        place.set_mode(&base.join(KEY).join("endeavor"), 0o755);

        let mut command = place.command("endeavor-mcp.sh", &[], "unused");
        command.env_remove("ENDEAVOR_RELEASE_URL").env_remove("XDG_DATA_HOME");
        if let Some(xdg) = &xdg {
            command.env("XDG_DATA_HOME", xdg);
        }
        let out = command.stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success() && out.stdout == format!("found in {name}\n").as_bytes(), "{name}: {base:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn nothing_but_the_binary_reaches_stdout_even_when_the_tools_print() {
    let place = Place::new("noise");
    place.release(KEY);
    // uname, head, tr, cut, cat, cksum, mktemp, sha256sum and dirname are left alone: the
    // scripts read their output. curl prints only when it saves to a file.
    for tool in ["find", "mkdir", "mv", "rm", "touch", "sleep", "chmod", "cp"] {
        place.fake(tool, &format!("#!/bin/sh\necho noise-{tool}\nexec {} \"$@\"\n", real(tool).display()));
    }
    place.fake("curl", &format!("#!/bin/sh\ncase \"$*\" in *\" -o \"*) echo noise-curl;; esac\nexec {} \"$@\"\n", real("curl").display()));
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let lock = place.data().join(".lock");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), format!("{dead}\n")).unwrap();
    let run = place.launch(&["--folder", "p"]);
    assert!(run.ok && run.stdout == expected("--folder p"), "{}{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("noise-"), "the noise went to stderr: {}", run.stderr);
    let again = place.launch(&["--folder", "p"]);
    assert!(again.ok && again.stdout == expected("--folder p"), "{}{}", again.stdout, again.stderr);
    let fetch_only = place.launch(&["--fetch-only"]);
    assert!(fetch_only.ok && fetch_only.stdout.is_empty());
}

#[test]
fn spaces_in_home_the_data_folder_and_the_plugin_folder_are_fine() {
    let place = Place::new("with spaces");
    place.release(KEY);
    assert!(place.dir.display().to_string().contains(' '));
    let share = place.dir.join("my data/share folder");
    let env = [("XDG_DATA_HOME", share.to_str().unwrap())];
    let run = place.launch_with(&["--folder", "my project"], &env);
    assert!(run.ok && run.stdout == expected("--folder my project"), "{}", run.stderr);
    let data = place.data_under(&share);
    assert!(data.join(KEY).join("endeavor").exists());
    // A stale lock in it is taken over, and the default place under HOME works too.
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    std::fs::remove_dir_all(data.join(KEY)).unwrap();
    std::fs::create_dir_all(data.join(".lock")).unwrap();
    std::fs::write(data.join(".lock/pid"), format!("{dead}\n")).unwrap();
    let again = place.launch_with(&[], &env);
    assert!(again.ok && again.stdout == expected(""), "{}", again.stderr);
    let home = place.launch(&[]);
    assert!(home.ok && home.stdout == expected(""), "{}", home.stderr);
    assert!(place.data().join(KEY).join("endeavor").exists());
}

#[test]
fn unpinned_it_looks_for_a_newer_build_in_the_background_once_a_day() {
    let place = Place::new("background");
    place.release(KEY);
    assert!(place.launch(&[]).ok);
    assert!(place.data().join(".checked").exists());
    place.publish(OTHER, &platform(), "", BIN, &sha(BIN));
    std::fs::write(place.dir.join("release/LATEST"), format!("{OTHER}\n")).unwrap();
    let newest = || std::fs::read_to_string(place.data().join(".newest")).unwrap_or_default().trim().to_owned();

    // Checked today: nothing is asked.
    assert!(place.launch(&[]).ok);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(newest(), KEY);

    // A day later the start isn't delayed by the check, and the next start uses what it found.
    std::fs::remove_file(place.data().join(".checked")).unwrap();
    place.logging_curl("sleep 2");
    let started = Instant::now();
    let run = place.launch(&["--folder", "p"]);
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    assert!(run.ok && run.stdout == expected("--folder p") && run.stderr.is_empty(), "{}", run.stderr);
    wait_for("the background check", || newest() == OTHER && !place.data().join(".lock").exists());
    assert!(place.data().join(OTHER).join("endeavor").exists() && place.data().join(".checked").exists());
    assert_eq!(place.launch(&[]).stdout, expected(""));

    // Pinned, it never looks.
    let pinned = Place::new("background-pinned");
    pinned.release(KEY);
    pinned.pin(KEY);
    assert!(pinned.launch(&[]).ok && pinned.launch(&[]).ok);
    assert!(!pinned.data().join(".checked").exists());
}

#[test]
fn wget_without_timeout_is_ended_by_a_watchdog_that_leaves_nothing_behind() {
    let place = Place::new("watchdog");
    place.release(KEY);
    let dir = place.dir.join("bin");
    let path = place.path_without(&["curl", "timeout", "wget"]);
    // The watchdog's 900 seconds are 1 here; every sleep is logged.
    place.fake("sleep", &format!("#!/bin/sh\necho $$ >> '{}'\n[ \"$1\" = 900 ] && set -- 1\nexec {} \"$@\"\n", place.dir.join("fake/sleep.pids").display(), real("sleep").display()));
    place.fake_wget("*endeavor-0123456789ab-linux*|*endeavor-0123456789ab-darwin*");
    let url = place.url();
    let run = |args: &[&str]| place.finish(place.command("install.sh", args, &url).env("PATH", &path).stdin(Stdio::null()).output().unwrap());

    let started = Instant::now();
    let slow = run(&["--key", KEY, "--dir", dir.to_str().unwrap()]);
    assert!(!slow.ok && slow.stderr.contains("couldn't download"), "{}", slow.stderr);
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    for file in ["fake/wget.pids", "fake/sleep.pids"] {
        for pid in std::fs::read_to_string(place.dir.join(file)).unwrap().lines() {
            assert!(!alive(pid), "{file}: {pid} is still running");
        }
    }

    let fast = place.dir.join("fast");
    std::fs::remove_file(place.dir.join("fake/wget")).unwrap();
    place.fake_wget("nothing");
    let ok = run(&["--key", KEY, "--dir", fast.to_str().unwrap()]);
    assert!(ok.ok && fast.join("endeavor").exists(), "{}", ok.stderr);
    for pid in std::fs::read_to_string(place.dir.join("fake/sleep.pids")).unwrap().lines() {
        assert!(!alive(pid), "a watchdog's sleep {pid} is still running");
    }
}

#[test]
fn wget_runs_under_timeout_when_there_is_one() {
    let place = Place::new("timeout");
    place.release(KEY);
    let path = place.path_without(&["curl", "timeout", "wget"]);
    place.fake("timeout", &format!("#!/bin/sh\necho \"$*\" >> '{}'\nshift\nexec \"$@\"\n", place.dir.join("fake/timeout.log").display()));
    place.fake_wget("nothing");
    let dir = place.dir.join("bin");
    let out = place.command("install.sh", &["--key", KEY, "--dir", dir.to_str().unwrap()], &place.url()).env("PATH", &path).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let log = std::fs::read_to_string(place.dir.join("fake/timeout.log")).unwrap();
    assert!(log.lines().all(|l| l.starts_with("900 wget") || l.starts_with("60 wget")) && log.contains("900 wget"), "{log}");
}

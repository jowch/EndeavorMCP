//! `scripts/endeavor-mcp.sh`, the plugin's MCP command, run as an agent runs
//! it, against a release that is a folder under `target/tmp`
//! (`ENDEAVOR_RELEASE_URL=file://…`), with `HOME` and the XDG variables there
//! too. The "binary" is a script that prints its arguments and a JSON line.
//! `ENDEAVOR_TEST_SH` names the shell that runs the scripts (default `sh`).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";
const OTHER: &str = "ba9876543210";
const BIN: &[u8] = b"#!/bin/sh\necho \"args: $*\"\necho '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\n";
const MANUAL: &str = "install.sh | sh";

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
        self.dir.join("home/.local/share/endeavor/bin")
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.join("release").display())
    }

    fn fake(&self, name: &str, body: &str) {
        let path = self.dir.join("fake").join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, script: &str, args: &[&str], url: &str) -> Command {
        let shell = std::env::var("ENDEAVOR_TEST_SH").unwrap_or_else(|_| "sh".into());
        let path = format!("{}:{}", self.dir.join("fake").display(), std::env::var("PATH").unwrap());
        let mut command = Command::new(shell);
        command
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

    fn offline(&self, args: &[&str]) -> Run {
        self.finish(self.command("endeavor-mcp.sh", args, "file:///nowhere").stdin(Stdio::null()).output().unwrap())
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
    assert_eq!(left, [".newest", KEY], "no lock or temporary folder is left");
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

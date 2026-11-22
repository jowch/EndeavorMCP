//! Finding the julia to start the runtime with: a path the user gave, the one a
//! shell line of theirs sets up (`module load julia`), the one on their login
//! shell's PATH, or else Endeavor's own.
//!
//! Endeavor's own Julia is the pinned version, `JULIA_VERSION`. Where the computer
//! has juliaup, it is juliaup's channel for that version, which Endeavor adds
//! (`juliaup`); otherwise Endeavor downloads it into its cache folder and checks it
//! here. On Windows juliaup is the only way, and Endeavor installs juliaup first
//! when there is none. `own_installed`, `install_own` and `remove_own` are for the
//! app's Settings, without a runtime.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The oldest Julia the runtime starts on: its Manifest.toml is resolved on 1.12, and 1.11's
/// Pkg can't instantiate it (#370). `--julia auto` passes over an older one for Endeavor's own.
/// The app reads it from here (`min_version`), so this is the one place to change it.
const MIN_JULIA: (u32, u32) = (1, 12);

/// Endeavor's own Julia, the tested version, pinned with the official tarballs'
/// SHA-256 and size. The app reads it from here; bump it with the cloud script
/// (scripts/cloud-setup.sh and Endeavor's copy) and e2e.yml's `version`.
pub const JULIA_VERSION: &str = "1.12.6";
/// About how much Julia's Windows download is (its x64 zip), for the consent question there.
const WINDOWS_SIZE: u64 = 300_000_000;
const TARBALLS: [(&str, &str, &str, &str, u64); 4] = [
    (
        "Linux",
        "x86_64",
        "https://julialang-s3.julialang.org/bin/linux/x64/1.12/julia-1.12.6-linux-x86_64.tar.gz",
        "bbabf3bef19421a9dbd24a767d807606ab85e444323b5a1c73ffe293fa3d079a",
        289_794_236,
    ),
    (
        "Linux",
        "aarch64",
        "https://julialang-s3.julialang.org/bin/linux/aarch64/1.12/julia-1.12.6-linux-aarch64.tar.gz",
        "029b93b857bd0ffd627f9a8580d3bbaa63daf008d7b7aed02fbceb8fd57c4899",
        306_918_080,
    ),
    (
        "Darwin",
        "aarch64",
        "https://julialang-s3.julialang.org/bin/mac/aarch64/1.12/julia-1.12.6-macaarch64.tar.gz",
        "277d82fbd2eda99d0963b3e41f3dc979d7486f181399f8430fb637318ccd6a31",
        231_027_185,
    ),
    (
        "Darwin",
        "x86_64",
        "https://julialang-s3.julialang.org/bin/mac/x64/1.12/julia-1.12.6-mac64.tar.gz",
        "1a70b7c606d6bac38a246e722369e5b30914dccf9378499d2712fb3bd282642c",
        271_518_180,
    ),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// `--julia PATH`
    Path(String),
    /// `--julia auto`: the login shell's (on Windows the PATH's), else Endeavor's own (not on Windows).
    Auto,
    /// `--julia-shell LINE`: what `LINE` puts on the login shell's PATH.
    Shell(String),
    /// `--julia own`: Endeavor's own Julia only, the pinned version, never the PATH's.
    Own,
}

impl Source {
    /// The VALUE of `--julia VALUE` that means this, or None for a shell line (`--julia-shell`).
    pub fn value(&self) -> Option<String> {
        match self {
            Source::Path(path) => Some(path.clone()),
            Source::Auto => Some("auto".into()),
            Source::Own => Some("own".into()),
            Source::Shell(_) => None,
        }
    }

    /// What `--julia VALUE` means: `auto`, `own`, or a path (a julia called `own` is `./own`).
    pub fn from_value(value: String) -> Source {
        match value.as_str() {
            "auto" => Source::Auto,
            "own" => Source::Own,
            _ => Source::Path(value),
        }
    }
}

/// Why Julia wasn't found.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// None was found and `find`'s `download` was false: nothing was downloaded.
    /// The download that was not allowed.
    Missing(wire::Item),
    Failed(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Failure {
        Failure::Failed(message)
    }
}

impl Failure {
    /// The words for an error, whichever it is.
    pub fn message(self) -> String {
        match self {
            Failure::Missing(item) => format!("Julia wasn't found on this machine. Endeavor can download its own copy: {item}."),
            Failure::Failed(message) => message,
        }
    }
}

/// The julia binary and its version ("1.12.6"). `progress` hears about a
/// download, which only happens when `download` is true.
pub fn find(source: &Source, download: bool, progress: &mut dyn FnMut(String)) -> Result<(String, String), Failure> {
    find_whose(source, download, progress).map(|(path, version, _)| (path, version))
}

/// `find`, and whether the Julia is Endeavor's own (`--julia own`, or `auto` with none of the
/// user's), so the runtime can say which one it started.
pub fn find_whose(source: &Source, download: bool, progress: &mut dyn FnMut(String)) -> Result<(String, String, bool), Failure> {
    let endeavors = std::cell::Cell::new(false);
    let (path, version) = found(source, download, &endeavors, progress)?;
    Ok((real_julia(path), version, endeavors.get()))
}

fn found(source: &Source, download: bool, endeavors: &std::cell::Cell<bool>, progress: &mut dyn FnMut(String)) -> Result<(String, String), Failure> {
    match source {
        Source::Path(path) => {
            let path = expand_home(path);
            let version = checked_version(&path, &format!("Julia wasn't found at {path}."))?;
            Ok((path, version))
        }
        Source::Shell(line) => {
            let (path, err, ended_early) = login_shell_julia(Some(line));
            // Ended well with no path: the lookup didn't run, or `julia` is a shell function or alias.
            if ended_early && line.contains('#') {
                return Err(format!("Running `{line}` in a login shell ended before it looked for julia, because a `#` in it comments out the rest. Take the comment out.").into());
            }
            let path = path.ok_or_else(|| {
                let why = err.lines().rev().find(|l| !l.trim().is_empty()).map(|l| format!(" ({})", l.trim())).unwrap_or_default();
                format!("Running `{line}` in a login shell didn't put julia on the PATH{why}.")
            })?;
            let version = checked_version(&path, &format!("`{line}` gave {path}, which doesn't run."))?;
            Ok((path, version))
        }
        Source::Auto => {
            if let Some(path) = users_path_julia()
                && let Ok(version) = checked_version(&path, &format!("Found {path}, but it doesn't run."))
            {
                return Ok((path, version));
            }
            endeavors.set(true);
            let path = own_julia(download, progress)?;
            let version = checked_version(&path, "Endeavor's Julia doesn't run on this machine.")?;
            Ok((path, version))
        }
        Source::Own => {
            endeavors.set(true);
            let path = own_julia(download, progress)?;
            let version = checked_version(&path, "Endeavor's Julia doesn't run on this machine.")?;
            if version != JULIA_VERSION {
                return Err(format!("Endeavor's Julia should be {JULIA_VERSION}, but {path} is {version}.").into());
            }
            Ok((path, version))
        }
    }
}

/// The user's own Julia, which `--julia auto` runs before Endeavor's: the login shell's `julia` (on
/// Windows the PATH's), with its version, or why it can't be used (too old, doesn't run), in which
/// case `auto` runs Endeavor's. None when there is none.
pub fn users_julia() -> Option<Result<(String, String), String>> {
    let path = users_path_julia()?;
    Some(checked_version(&path, &format!("Found {path}, but it doesn't run.")).map(|version| (real_julia(path), version)))
}

/// Which Julia the app's Settings ask for on this computer. The library decides what that runs
/// (`choose`), so the app and a server pick Julia the same way.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Preference {
    /// The user's own Julia when they have one new enough, else Endeavor's (`--julia auto`).
    #[default]
    Auto,
    /// Endeavor's own Julia, the pinned version, even when the user has one (`--julia own`).
    Endeavors,
    /// This julia, while it runs and is new enough; else as `Auto`.
    Path(PathBuf),
}

/// What a julia the user chose turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub enum Chosen {
    /// It runs and is new enough; its version, e.g. "1.12.6".
    Usable(String),
    /// Nothing is at the path any more.
    Gone,
    /// Something is there, but it didn't answer as Julia does.
    NotJulia,
    /// Julia, but older than Endeavor needs; its version.
    TooOld(String),
}

/// Run the chosen file once to read its version (blocking; Julia answers in well under a second).
pub fn check_chosen(path: &Path) -> Chosen {
    if !path.is_file() {
        return Chosen::Gone;
    }
    let mut command = Command::new(path);
    crate::client::no_window(&mut command);
    match command.arg("--version").stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => chosen_from_version(&String::from_utf8_lossy(&out.stdout)),
        _ => Chosen::NotJulia,
    }
}

/// `julia --version`'s answer, as a check of the chosen file.
fn chosen_from_version(text: &str) -> Chosen {
    let Some(version) = text.trim().strip_prefix("julia version ") else { return Chosen::NotJulia };
    match parse_version(version) {
        Some(v) if v < MIN_JULIA => Chosen::TooOld(version.to_owned()),
        Some(_) => Chosen::Usable(version.to_owned()),
        None => Chosen::NotJulia,
    }
}

/// The oldest Julia Endeavor runs, for saying so ("1.12").
pub fn min_version() -> String {
    format!("{}.{}", MIN_JULIA.0, MIN_JULIA.1)
}

/// The `--julia` a preference gives the runtime: a chosen julia that can't be used counts as
/// `Auto`. `chosen` is `check_chosen`'s answer for a `Path`, when it has been asked.
pub fn choose(preference: &Preference, chosen: Option<&Chosen>) -> Source {
    match preference {
        Preference::Auto => Source::Auto,
        Preference::Endeavors => Source::Own,
        Preference::Path(path) => match chosen.cloned().unwrap_or_else(|| check_chosen(path)) {
            Chosen::Usable(_) => Source::Path(path.display().to_string()),
            _ => Source::Auto,
        },
    }
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
        _ => path.to_owned(),
    }
}

/// The user's own julia on the PATH (`path_julia`). When that is juliaup's launcher and juliaup has no
/// channel yet, there is none: the launcher's first run would install juliaup's latest Julia and make
/// it the default, which the person never chose and Endeavor would then count as theirs (#512). So
/// juliaup is asked first, which installs nothing; Endeavor's channel, added next, becomes its default.
/// Otherwise the launcher is run as before, so `JULIAUP_CHANNEL` and `juliaup override` still choose.
fn users_path_julia() -> Option<String> {
    let julia = path_julia()?;
    if is_real_julia(Path::new(&julia)) {
        return Some(julia);
    }
    let listed = crate::juliaup::find().and_then(|juliaup| crate::juliaup::listing(&juliaup));
    launchers_julia(julia, listed.as_deref())
}

/// What `users_path_julia` takes for juliaup's launcher `julia`, given juliaup's listing: none before
/// juliaup has a channel, else the launcher.
fn launchers_julia(julia: String, listed: Option<&str>) -> Option<String> {
    match listed.and_then(crate::juliaup::default_julia) {
        Some(None) => None,
        _ => Some(julia),
    }
}

/// The julia on the login shell's PATH.
#[cfg(unix)]
fn path_julia() -> Option<String> {
    login_shell_julia(None).0
}

/// The first `julia.exe` on the PATH: Windows has no login shell. juliaup from
/// the Microsoft Store puts an app alias there, which runs like any julia.exe.
#[cfg(windows)]
fn path_julia() -> Option<String> {
    julia_in(&std::env::var_os("PATH")?)
}

#[cfg(windows)]
fn julia_in(path: &std::ffi::OsStr) -> Option<String> {
    std::env::split_paths(path).map(|dir| dir.join("julia.exe")).find(|exe| exe.is_file()).map(|exe| exe.display().to_string())
}

/// juliaup's `julia` (its launcher, or on Windows the Store app's alias) only
/// starts the real one, from the channel juliaup picks when it starts. Started
/// through it, the version checked and the Julia run can differ, and on
/// Windows that launcher and the Julia under it ran outside the core's job, so
/// ending the core left Julia running (EndeavorMCP #55). Start the `julia` in
/// that Julia's own `Sys.BINDIR` instead (only for juliaup's launcher: see
/// `is_real_julia`). If asking
/// fails, the path as found.
fn real_julia(julia: String) -> String {
    if is_real_julia(Path::new(&julia)) {
        return julia;
    }
    let mut command = Command::new(&julia);
    crate::client::no_window(&mut command);
    let output = command.args(["--startup-file=no", "--history-file=no", "-e", "print(Sys.BINDIR)"]).stdin(Stdio::null()).stderr(Stdio::null()).output();
    // Started through the launcher, the Julia run can differ from the one checked (#55): say so in the log.
    let as_found = |why: String| {
        eprintln!("Couldn't find the Julia behind {julia} ({why}), so it is started as found.");
        julia.clone()
    };
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(output) => return as_found(format!("it ended with {}", output.status)),
        Err(e) => return as_found(e.to_string()),
    };
    match bindir_julia(&String::from_utf8_lossy(&output.stdout)) {
        Some(exe) if exe.is_file() => exe.display().to_string(),
        Some(exe) => as_found(format!("{} isn't there", exe.display())),
        None => as_found("it printed no Sys.BINDIR".to_owned()),
    }
}

/// Whether `julia` is the one to start as it is. On Windows: libjulia.dll beside it (anything else
/// may be juliaup's launcher or alias). Elsewhere, anything but juliaup's launcher, which the `julia`
/// on the PATH links to (`julialauncher`): a wrapper script someone put there is left alone, not run
/// to ask where its Julia is.
fn is_real_julia(julia: &Path) -> bool {
    let julia = std::fs::canonicalize(julia).unwrap_or_else(|_| julia.to_path_buf());
    if cfg!(windows) {
        return julia.parent().is_some_and(|dir| dir.join("libjulia.dll").is_file());
    }
    !julia.file_name().is_some_and(|name| name.to_string_lossy().starts_with("julialauncher"))
}

/// The julia in the `Sys.BINDIR` Julia printed (the last line: a startup
/// message may come first).
fn bindir_julia(printed: &str) -> Option<std::path::PathBuf> {
    let bindir = printed.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    Some(Path::new(bindir).join(if cfg!(windows) { "julia.exe" } else { "julia" }))
}

/// The julia the user's login shell finds, after `line` when there is one: the
/// last absolute path it prints (profiles can print other things first), what
/// it wrote to stderr, and whether it ended well without printing a path.
/// `command -v` fails when it finds nothing, so that means the lookup never ran:
/// a `#` in the line commented it out.
fn login_shell_julia(line: Option<&str>) -> (Option<String>, String, bool) {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let Ok(output) = lookup_command(&shell, line).stdin(Stdio::null()).output() else {
        return (None, format!("couldn't run {shell}"), false);
    };
    looked_up(&output)
}

/// `login_shell_julia`'s answer from the shell's output.
fn looked_up(output: &std::process::Output) -> (Option<String>, String, bool) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.lines().rev().map(str::trim).find(|l| l.starts_with('/')).map(str::to_owned);
    let ended_early = output.status.success() && path.is_none();
    (path.filter(|_| output.status.success()), String::from_utf8_lossy(&output.stderr).into_owned(), ended_early)
}

/// `shell` asked for julia's path, after `line`. csh and tcsh take `-l` only as their one flag, so they
/// run a plain `-c`, which still reads ~/.cshrc or ~/.tcshrc, where module setup goes, and sh looks
/// julia up on the PATH they leave (as R's setting does: `r::Source::command`).
fn lookup_command(shell: &str, line: Option<&str>) -> Command {
    let mut command = Command::new(shell);
    if crate::r::is_csh(shell) {
        let line = line.map_or(String::new(), |line| format!("{} >/dev/null; ", line.replace('\n', "; ")));
        command.args(["-c", &format!("{line}exec /bin/sh -c 'command -v julia'")]);
    } else {
        let line = line.map_or(String::new(), |line| format!("{line} >/dev/null && "));
        command.args(["-lc", &format!("{line}command -v julia")]);
    }
    command
}

/// `julia --version`, if it's new enough.
fn checked_version(julia: &str, missing: &str) -> Result<String, String> {
    let mut command = Command::new(julia);
    crate::client::no_window(&mut command);
    let output = command.arg("--version").stdin(Stdio::null()).output().map_err(|_| missing.to_owned())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.trim().rsplit(' ').next().unwrap_or_default().to_owned();
    match parse_version(&version) {
        Some(v) if v >= MIN_JULIA => Ok(version),
        Some(_) => Err(format!("Endeavor needs Julia {}.{} or newer; {julia} is {version}.", MIN_JULIA.0, MIN_JULIA.1)),
        None => Err(format!("{julia} --version said {:?}, not a Julia version.", text.trim())),
    }
}

/// "1.12.6" -> (1, 12)
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub(crate) fn uname(flag: &str) -> String {
    Command::new("uname").arg(flag).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

/// Endeavor's own Julia as it is installed on this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnJulia {
    /// Its `julia`.
    pub julia: PathBuf,
    pub from: OwnFrom,
}

/// Where Endeavor's own Julia came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OwnFrom {
    /// Endeavor downloaded it into its cache folder (`~/.cache/endeavor/julia-<version>`).
    Download,
    /// It is juliaup's channel for the pinned version. `added`: Endeavor added the channel, so
    /// removing it is Endeavor's to do; otherwise the person added it, and it stays theirs.
    /// `default`: it is juliaup's default (as the first channel of a juliaup that had none), which
    /// juliaup won't remove, so `remove_own` says what to do instead.
    Juliaup { added: bool, default: bool },
}

/// Endeavor's own Julia if it is installed; nothing is downloaded. Asks juliaup, if there is one,
/// which takes a moment.
pub fn own_installed() -> Option<OwnJulia> {
    if let Some(julia) = downloaded().filter(|julia| julia.exists()) {
        return Some(OwnJulia { julia, from: OwnFrom::Download });
    }
    let juliaup = crate::juliaup::find()?;
    let listed = crate::juliaup::listing(&juliaup);
    if let Some(mark) = added_mark() {
        forget_if_gone(listed.as_deref(), &mark);
    }
    let channel = crate::juliaup::channel(&listed?, JULIA_VERSION).filter(|channel| channel.julia.is_file())?;
    Some(OwnJulia { julia: channel.julia, from: OwnFrom::Juliaup { added: marked(), default: channel.default } })
}

/// Delete the mark that says Endeavor added juliaup's channel when juliaup's `listed` channels have
/// none for its Julia: the channel went some other way (`juliaup remove`), so one added later is the
/// person's, and Remove must leave it (#118). A juliaup that couldn't say (`None`), or said something
/// that isn't its listing, leaves the mark.
fn forget_if_gone(listed: Option<&str>, mark: &Path) {
    let read = |listed: &str| serde_json::from_str::<serde_json::Value>(listed.trim()).is_ok_and(|config| config.get("DefaultChannel").is_some());
    if listed.is_some_and(|listed| read(listed) && crate::juliaup::channel(listed, JULIA_VERSION).is_none()) {
        let _ = std::fs::remove_file(mark);
    }
}

/// Install Endeavor's own Julia unless it is installed: with juliaup when the computer has it (on
/// Windows, installing juliaup first when it hasn't), else by downloading it. `progress` hears each
/// step ("Downloading Julia 1.12.6… 42%").
pub fn install_own(progress: &mut dyn FnMut(String)) -> Result<OwnJulia, String> {
    if let Some(own) = own_installed() {
        return Ok(own);
    }
    let juliaup = match crate::juliaup::find() {
        Some(juliaup) => juliaup,
        #[cfg(windows)]
        None => crate::juliaup::install(progress)?,
        #[cfg(unix)]
        None => return download_own(progress),
    };
    // Endeavor adds the channel only if juliaup said it has none. When juliaup can't say, the channel
    // may already be the person's (and juliaup counts adding it again as done), so it isn't marked ours.
    let ours = crate::juliaup::listing(&juliaup).is_some_and(|listed| crate::juliaup::channel(&listed, JULIA_VERSION).is_none());
    let channel = match crate::juliaup::add(&juliaup, JULIA_VERSION, progress) {
        Ok(channel) => channel,
        // Elsewhere there is the checked download, so a juliaup that fails (a locked config, an old
        // juliaup, a mirror that's down) costs a second Julia rather than none.
        #[cfg(unix)]
        Err(why) => {
            progress(format!("{why} Downloading Endeavor's own Julia {JULIA_VERSION} instead."));
            return download_own(progress);
        }
        // A failed add was already retried once, but a failure like Windows' "Access is denied" can still pass later.
        #[cfg(windows)]
        Err(why) => return Err(format!("{why} Trying again may work.")),
    };
    if ours && let Some(mark) = added_mark() {
        let _ = std::fs::create_dir_all(mark.parent().unwrap());
        let _ = std::fs::write(&mark, "Endeavor added juliaup's channel for this Julia, and removes it when asked to remove its Julia.\n");
    }
    Ok(OwnJulia { julia: channel.julia, from: OwnFrom::Juliaup { added: marked(), default: channel.default } })
}

/// Download Endeavor's own Julia into its cache folder.
#[cfg(unix)]
fn download_own(progress: &mut dyn FnMut(String)) -> Result<OwnJulia, String> {
    let dir = download_dir().ok_or("HOME isn't set")?;
    let (url, sha256, size) = tarball()?;
    install(dir.parent().unwrap(), &dir, url, sha256, size, progress)?;
    Ok(OwnJulia { julia: dir.join("bin").join("julia"), from: OwnFrom::Download })
}

/// Remove Endeavor's own Julia: the one it downloaded, and juliaup's channel if Endeavor added it.
/// A channel the person added stays, so `own_installed` may still find one. Stop the runtimes that
/// use it first.
pub fn remove_own() -> Result<(), String> {
    if let Some(dir) = download_dir().filter(|dir| dir.exists()) {
        not_in_use(&dir)?;
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Couldn't remove {}: {e}", dir.display()))?;
    }
    if let Some(mark) = added_mark().filter(|mark| mark.exists()) {
        if let Some(juliaup) = crate::juliaup::find() {
            let listed = crate::juliaup::listing(&juliaup)
                .ok_or(format!("juliaup didn't say which Julia versions it has, so Endeavor's Julia {JULIA_VERSION} wasn't removed. Try again."))?;
            if let Some(channel) = crate::juliaup::channel(&listed, JULIA_VERSION) {
                if channel.default {
                    return Err(format!(
                        "juliaup uses Julia {JULIA_VERSION} as its default, so it stays. To remove it, make another Julia juliaup's default first (`juliaup add release`, then `juliaup default release`), then remove it again."
                    ));
                }
                // juliaup keeps one folder per Julia version for all its channels: when another channel
                // (`release`, say) is the same Julia, removing this one leaves the files, and a REPL of
                // the person's on that channel mustn't hold Remove up.
                if !another_channel_has(&listed, &channel.julia)
                    && let Some(dir) = channel.julia.parent().and_then(Path::parent)
                {
                    not_in_use(dir)?;
                }
                crate::juliaup::remove(&juliaup, JULIA_VERSION)?;
            }
        }
        let _ = std::fs::remove_file(mark);
    }
    Ok(())
}

/// Refuse while a process here runs a program from the Julia in `dir` (Julia, or a worker it started):
/// a runtime the app doesn't know about, started with `endeavor serve` or by the plugin, may run it,
/// and its notebooks break once it is gone (#117). The app stops its own runtime first, so its Julia
/// gets a few seconds to end.
fn not_in_use(dir: &Path) -> Result<(), String> {
    let until = std::time::Instant::now() + IN_USE_WAIT;
    while running_from(dir, crate::old_builds::processes().as_deref().unwrap_or_default()) {
        if std::time::Instant::now() >= until {
            return Err(format!(
                "Julia {JULIA_VERSION} is still running here, maybe for notebooks started with `endeavor serve` or the Endeavor plugin. Stop them (`endeavor stop`), and quit any other Julia {JULIA_VERSION}, then remove it again."
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

/// Whether a channel in juliaup's `listed` channels other than Endeavor's (`JULIA_VERSION`) runs `julia`.
fn another_channel_has(listed: &str, julia: &Path) -> bool {
    let Ok(config) = serde_json::from_str::<serde_json::Value>(listed.trim()) else { return false };
    let others = config["OtherChannels"].as_array().into_iter().flatten();
    std::iter::once(&config["DefaultChannel"]).chain(others).any(|c| c["Name"] != JULIA_VERSION && c["File"].as_str().is_some_and(|file| Path::new(file) == julia))
}

/// How long `not_in_use` waits for a Julia that is ending.
const IN_USE_WAIT: Duration = Duration::from_secs(5);

/// Whether one of the processes' `texts` (`old_builds::processes`: a command line, and on Linux each
/// argument and the binary on a line of its own) runs a program from `dir`.
fn running_from(dir: &Path, texts: &[String]) -> bool {
    let prefix = format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
    texts.iter().any(|text| text.lines().any(|line| line.starts_with(&prefix)))
}

/// Whether Endeavor added juliaup's channel for its Julia.
fn marked() -> bool {
    added_mark().is_some_and(|mark| mark.exists())
}

/// The `julia` Endeavor downloads into its cache folder.
fn downloaded() -> Option<PathBuf> {
    download_dir().map(|dir| dir.join("bin").join("julia"))
}

/// Where Endeavor downloads its Julia: `~/.cache/endeavor/julia-<version>`. Windows has none: Julia
/// there comes from juliaup.
fn download_dir() -> Option<PathBuf> {
    let env = crate::paths::Env::here();
    (cfg!(unix) && !env.home.as_os_str().is_empty()).then(|| env.server_root().join(format!("julia-{JULIA_VERSION}")))
}

/// The file that says Endeavor added juliaup's channel for its Julia, beside its cache folder's Julia.
fn added_mark() -> Option<PathBuf> {
    let env = crate::paths::Env::here();
    (!env.home.as_os_str().is_empty()).then(|| env.server_root().join(format!("julia-{JULIA_VERSION}.juliaup")))
}

/// The official tarball for this machine: its URL, SHA-256 and size.
pub fn tarball() -> Result<(&'static str, &'static str, u64), String> {
    let (os, arch) = (uname("-s"), uname("-m").replace("arm64", "aarch64"));
    let &(_, _, url, sha256, size) = TARBALLS
        .iter()
        .find(|t| t.0 == os && t.1 == arch)
        .ok_or_else(|| format!("No julia on this machine's PATH, and Endeavor has no Julia download for {os} {arch}. Set How to get Julia for this server."))?;
    Ok((url, sha256, size))
}

/// Endeavor's own Julia, getting it the first time when `download` allows; else what getting it
/// would download.
fn own_julia(download: bool, progress: &mut dyn FnMut(String)) -> Result<String, Failure> {
    if let Some(own) = own_installed() {
        return Ok(own.julia.display().to_string());
    }
    if !download {
        return Err(Failure::Missing(own_item()?));
    }
    Ok(install_own(progress)?.julia.display().to_string())
}

/// What getting Endeavor's own Julia downloads, and where it goes.
fn own_item() -> Result<wire::Item, String> {
    let item = |name: String, size: Option<u64>, place: Option<PathBuf>| wire::Item {
        kind: wire::KIND_RUNTIME.into(),
        name,
        size_mb: size.map(|size| size / 1_000_000),
        place: place.map(|place| place.display().to_string()),
    };
    if crate::juliaup::find().is_some() {
        return Ok(item(format!("Julia {JULIA_VERSION}, added to juliaup"), tarball().ok().map(|t| t.2), None));
    }
    if cfg!(windows) {
        return Ok(item(format!("juliaup (Julia's installer, from the Microsoft Store) and Julia {JULIA_VERSION} through it"), Some(WINDOWS_SIZE), None));
    }
    let dir = download_dir().ok_or("HOME isn't set")?;
    Ok(item(format!("Julia {JULIA_VERSION}"), Some(tarball()?.2), Some(dir)))
}

#[cfg_attr(windows, allow(dead_code))]
fn install(cache: &Path, dir: &Path, url: &str, sha256: &str, size: u64, progress: &mut dyn FnMut(String)) -> Result<(), String> {
    std::fs::create_dir_all(cache).map_err(|e| format!("Couldn't create {}: {e}", cache.display()))?;
    let top = format!("julia-{JULIA_VERSION}");
    let part = cache.join(format!("{top}.tar.gz.part"));
    if !has("curl") && !has("wget") {
        return Err("No julia on this machine's PATH, and neither curl nor wget to download one. Set How to get Julia for this server.".into());
    }
    download(&part, url, sha256, size, &format!("Julia {JULIA_VERSION}"), progress)?;
    progress(format!("Unpacking Julia {JULIA_VERSION}…"));
    // Unpacked beside the target, then renamed, so a half-unpacked Julia is never used.
    let staging = cache.join(format!("{top}.unpacking"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut untar = Command::new("tar");
    crate::host_tools::with_command_mask(untar.arg("-xzf").arg(&part).arg("-C").arg(&staging));
    let untar = untar.status().map_err(|e| e.to_string())?;
    if !untar.success() || !staging.join(&top).is_dir() {
        return Err(format!("Couldn't unpack Julia {JULIA_VERSION} ({untar})."));
    }
    std::fs::rename(staging.join(&top), dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&part);
    Ok(())
}

/// Download `url` into `part`, resuming what an earlier try left, and check it is the `size` bytes
/// with SHA-256 `sha256`; `name` is what a person calls it ("Julia 1.12.6"). A corrupt download is deleted.
pub(crate) fn download(part: &Path, url: &str, sha256: &str, size: u64, name: &str, progress: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut command = if has("curl") {
        // A download that stalls (under 1 kB/s for 5 minutes) fails, rather than wait for good; the next try resumes it.
        let mut curl = Command::new("curl");
        curl.args(["-fsSL", "--retry", "3", "--speed-limit", "1000", "--speed-time", "300", "-C", "-", "-o"]).arg(part).arg(url);
        curl
    } else if has("wget") {
        let mut wget = Command::new("wget");
        wget.args(["-q", "-c", "--read-timeout=300", "-O"]).arg(part).arg(url);
        wget
    } else {
        return Err(format!("Couldn't download {name}: there is neither curl nor wget."));
    };
    // Stopped with the core, not left downloading because the core's mask blocks Ctrl-C (#181).
    crate::host_tools::with_command_mask(&mut command);
    let mut download = command.stderr(Stdio::piped()).spawn()
    .map_err(|e| format!("Couldn't start the download of {name}: {e}"))?;
    let mut shown = u64::MAX;
    let status = loop {
        if let Some(status) = download.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        let percent = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0) * 100 / size;
        if percent != shown {
            shown = percent;
            progress(format!("Downloading {name}… {percent}%"));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    if !status.success() {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut download.stderr.take().unwrap(), &mut err);
        return Err(format!("Couldn't download {name} ({}). It resumes on the next try.", err.trim()));
    }
    progress(format!("Checking {name}…"));
    let got = sha256_of(part)?;
    if got != sha256 {
        let _ = std::fs::remove_file(part);
        return Err(format!("The {name} download was corrupt or tampered with (SHA-256 {got}); it was deleted."));
    }
    Ok(())
}

fn has(program: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {program}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let output = if has("sha256sum") {
        Command::new("sha256sum").arg(path).output()
    } else {
        Command::new("shasum").args(["-a", "256"]).arg(path).output()
    }
    .map_err(|e| format!("Couldn't check the download: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).split_whitespace().next().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unix paths; on Windows `processes` lists nothing, so the check never fires.
    #[cfg(unix)]
    #[test]
    fn a_julia_is_in_use_while_a_process_runs_a_program_from_its_folder() {
        let dir = Path::new("/home/ada/.cache/endeavor/julia-1.12.6");
        let texts = |lines: &[&str]| vec![lines.join("\n")];
        assert!(running_from(dir, &texts(&["/home/ada/.cache/endeavor/julia-1.12.6/bin/julia", "--project=/x"])));
        assert!(running_from(dir, &texts(&["/usr/bin/sh", "/home/ada/.cache/endeavor/julia-1.12.6/libexec/julia/worker"])));
        assert!(!running_from(dir, &texts(&["/home/ada/.cache/endeavor/julia-1.12.6.juliaup"])), "a name that only starts the same");
        assert!(!running_from(dir, &texts(&["/usr/bin/julia", "--version"])));
        assert!(!running_from(dir, &[]));
    }

    #[cfg(unix)]
    #[test]
    fn a_running_program_from_the_folder_is_seen_and_one_that_ended_is_not() {
        let dir = std::env::temp_dir().join(format!("endeavor-julia-in-use-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let sleep = ["/bin/sleep", "/usr/bin/sleep"].into_iter().map(Path::new).find(|p| p.exists()).expect("sleep");
        crate::test_exec::copy_executable(sleep, &dir.join("bin/julia"));
        let mut julia = std::process::Command::new(dir.join("bin/julia")).arg("30").spawn().unwrap();
        assert!(running_from(&dir, &crate::old_builds::processes().unwrap()));
        assert!(not_in_use(&dir).is_err_and(|why| why.contains("endeavor stop")));
        julia.kill().unwrap();
        julia.wait().unwrap();
        assert_eq!(not_in_use(&dir), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_julia_another_juliaup_channel_also_runs_isnt_ours_to_hold_remove_for() {
        let file = "/home/ada/.julia/juliaup/julia-1.12.6+0.x64.linux.gnu/bin/julia";
        let julia = Path::new(file);
        let listed = |release: &str| serde_json::json!({ "DefaultChannel": { "Name": "release", "File": release }, "OtherChannels": [{ "Name": JULIA_VERSION, "File": file }] }).to_string();
        assert!(another_channel_has(&listed(file), julia), "release is the same Julia");
        assert!(!another_channel_has(&listed("/home/ada/.julia/juliaup/julia-1.12.7+0.x64.linux.gnu/bin/julia"), julia));
        let alone = serde_json::json!({ "DefaultChannel": null, "OtherChannels": [{ "Name": JULIA_VERSION, "File": file }] }).to_string();
        assert!(!another_channel_has(&alone, julia), "its own channel doesn't count");
        assert!(!another_channel_has("not json", julia));
    }

    #[test]
    fn the_added_mark_goes_once_juliaup_lists_no_channel_for_the_julia() {
        let dir = std::env::temp_dir().join(format!("endeavor-julia-mark-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mark = dir.join(format!("julia-{JULIA_VERSION}.juliaup"));
        let with = format!(r#"{{"DefaultChannel":{{"Name":"release","File":"/j/julia"}},"OtherChannels":[{{"Name":"{JULIA_VERSION}","File":"/j/julia"}}]}}"#);
        let without = r#"{"DefaultChannel":{"Name":"release","File":"/j/julia"},"OtherChannels":[]}"#;
        std::fs::write(&mark, "").unwrap();
        forget_if_gone(None, &mark);
        assert!(mark.exists(), "a juliaup that couldn't say leaves the mark");
        forget_if_gone(Some("Error: something else"), &mark);
        assert!(mark.exists(), "so does one that said something else");
        forget_if_gone(Some(&with), &mark);
        assert!(mark.exists(), "the channel is still there");
        forget_if_gone(Some(without), &mark);
        assert!(!mark.exists(), "the channel went another way");
        forget_if_gone(Some(without), &mark);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn every_login_shell_finds_the_julia_a_line_puts_on_the_path() {
        let dir = std::env::temp_dir().join(format!("endeavor-julia-shell-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::test_exec::write_executable(&dir.join("julia"), "#!/bin/sh\n");
        let d = dir.display();
        let shells = [
            ("/bin/sh", format!("export PATH=\"{d}:$PATH\""), "PATH=/nonexistent"),
            ("/bin/bash", format!("export PATH=\"{d}:$PATH\""), "PATH=/nonexistent"),
            ("/bin/zsh", format!("export PATH=\"{d}:$PATH\""), "PATH=/nonexistent"),
            ("/usr/bin/fish", format!("set -gx PATH {d} $PATH"), "set -gx PATH /nonexistent"),
            ("/bin/tcsh", format!("echo loading\nsetenv PATH \"{d}:$PATH\""), "setenv PATH /nonexistent"),
            ("/bin/csh", format!("setenv PATH \"{d}:$PATH\""), "setenv PATH /nonexistent"),
        ];
        let found = |shell: &str, line: &str| {
            let output = lookup_command(shell, Some(line)).stdin(Stdio::null()).stderr(Stdio::null()).output().unwrap();
            let (path, _, ended_early) = looked_up(&output);
            (output.status.success(), path, ended_early)
        };
        let mut tried = 0;
        for (shell, line, no_julia) in shells {
            if !Path::new(shell).exists() {
                continue;
            }
            tried += 1;
            assert_eq!(found(shell, &line), (true, Some(dir.join("julia").display().to_string()), false), "{shell}");
            assert_eq!(found(shell, no_julia), (false, None, false), "{shell}: a PATH without julia finds none");
            assert_eq!(found(shell, &format!("{line} # julia 1.12")), (true, None, true), "{shell}: a comment ends the line before the lookup (#478)");
        }
        assert!(tried > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chosen_julia_is_checked_by_its_version() {
        assert_eq!(chosen_from_version("julia version 1.12.6\n"), Chosen::Usable("1.12.6".into()));
        assert_eq!(chosen_from_version("julia version 1.13.4\n"), Chosen::Usable("1.13.4".into()));
        assert_eq!(chosen_from_version("julia version 1.11.9\n"), Chosen::TooOld("1.11.9".into()), "#370");
        assert_eq!(chosen_from_version("Python 3.12.1\n"), Chosen::NotJulia);
        assert_eq!(chosen_from_version(""), Chosen::NotJulia);
        assert_eq!(min_version(), "1.12");
    }

    #[test]
    fn a_preference_chooses_the_runtimes_julia() {
        assert_eq!(choose(&Preference::Auto, None), Source::Auto, "yours, else Endeavor's");
        assert_eq!(choose(&Preference::Endeavors, None), Source::Own);
        let path = Preference::Path("/opt/julia/bin/julia".into());
        assert_eq!(choose(&path, Some(&Chosen::Usable("1.13.4".into()))), Source::Path("/opt/julia/bin/julia".into()));
        for broken in [Chosen::Gone, Chosen::NotJulia, Chosen::TooOld("1.11.9".into())] {
            assert_eq!(choose(&path, Some(&broken)), Source::Auto, "{broken:?} runs another");
        }
        assert_eq!(choose(&Preference::Path("/no/such/julia".into()), None), Source::Auto, "checked when not asked yet");
        assert_eq!(Source::Auto.value().as_deref(), Some("auto"));
        assert_eq!(Source::Own.value().as_deref(), Some("own"));
        assert_eq!(Source::Shell("module load julia".into()).value(), None);
    }

    #[test]
    fn versions_and_home_paths() {
        assert_eq!(parse_version("1.12.6"), Some((1, 12)));
        assert!(parse_version("1.10.4").unwrap() < MIN_JULIA);
        assert_eq!(parse_version("garbage"), None);
        let home = std::env::home_dir().unwrap();
        assert_eq!(expand_home("~/julia/bin/julia"), format!("{}/julia/bin/julia", home.display()));
        assert_eq!(expand_home("/opt/julia"), "/opt/julia");
    }

    /// Windows has no login shell, so `--julia auto` looks in the PATH's folders itself.
    #[cfg(windows)]
    #[test]
    fn windows_finds_julia_exe_on_the_path() {
        let (without, with) = (crate::client::scratch("julia-path-without"), crate::client::scratch("julia-path-with"));
        std::fs::write(with.join("julia.exe"), "").unwrap();
        assert_eq!(julia_in(&std::env::join_paths([&without, &with]).unwrap()), Some(with.join("julia.exe").display().to_string()));
        assert_eq!(julia_in(&std::env::join_paths([&without]).unwrap()), None);
    }

    #[test]
    fn juliaups_launcher_isnt_run_on_a_juliaup_with_no_channel_yet() {
        let launcher = "/home/someone/.juliaup/bin/julia".to_owned();
        // Running it would install juliaup's latest Julia (#512).
        assert_eq!(launchers_julia(launcher.clone(), Some(r#"{"DefaultChannel":null,"OtherChannels":[]}"#)), None);
        // With a channel, the launcher still picks, so JULIAUP_CHANNEL and overrides hold; also when juliaup can't say.
        let listed = serde_json::json!({ "DefaultChannel": { "Name": "release", "File": "/j/bin/julia" }, "OtherChannels": [] }).to_string();
        assert_eq!(launchers_julia(launcher.clone(), Some(&listed)), Some(launcher.clone()));
        assert_eq!(launchers_julia(launcher.clone(), None), Some(launcher));
    }

    #[test]
    fn juliaups_launcher_gives_way_to_the_julia_in_its_bindir() {
        let (bindir, exe) = if cfg!(windows) {
            (r"C:\Users\someone\.julia\juliaup\julia-1.12.6+0.x64.w64.mingw32\bin", "julia.exe")
        } else {
            ("/home/someone/.julia/juliaup/julia-1.12.6+0.x64.linux.gnu/bin", "julia")
        };
        assert_eq!(bindir_julia(bindir), Some(Path::new(bindir).join(exe)));
        assert_eq!(bindir_julia(&format!("a startup message\n{bindir}\r\n")), Some(Path::new(bindir).join(exe)));
        assert_eq!(bindir_julia(" \n"), None);
        // juliaup's launcher isn't Julia's own binary; Julia's, beside its library, is.
        let root = crate::client::scratch("julia-libjulia");
        std::fs::create_dir_all(root.join("launcher")).unwrap();
        std::fs::create_dir_all(root.join("julia").join("bin")).unwrap();
        let launcher = root.join("launcher").join(if cfg!(windows) { "julia.exe" } else { "julialauncher" });
        std::fs::write(&launcher, "").unwrap();
        std::fs::write(root.join("julia").join("bin").join(exe), "").unwrap();
        std::fs::write(root.join("julia").join("bin").join("libjulia.dll"), "").unwrap();
        assert!(!is_real_julia(&launcher));
        assert!(is_real_julia(&root.join("julia").join("bin").join(exe)));
        // Julia's own binary (an empty file, which can't run) is started as it is, without being asked.
        let real = root.join("julia").join("bin").join(exe).display().to_string();
        assert_eq!(real_julia(real.clone()), real);
    }

    /// The cloud VMs' setup script installs this Julia and the pinned Rust too (Endeavor's docs/cloud.md).
    #[test]
    fn the_cloud_setup_script_pins_the_same_julia() {
        let script = include_str!("../../../scripts/cloud-setup.sh");
        let (_, _, url, sha, _) = TARBALLS.iter().find(|t| t.0 == "Linux" && t.1 == "x86_64").unwrap();
        let rust = include_str!("../../../rust-toolchain.toml").lines().find_map(|l| l.strip_prefix("channel = ")).unwrap().trim_matches('"');
        for line in [format!("JULIA_VERSION={JULIA_VERSION}"), format!("JULIA_URL={url}"), format!("JULIA_SHA256={sha}"), format!("RUST_TOOLCHAIN={rust}")] {
            assert!(script.lines().any(|l| l == line), "scripts/cloud-setup.sh should have `{line}`");
        }
    }
}

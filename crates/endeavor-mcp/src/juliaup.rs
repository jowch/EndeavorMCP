//! juliaup, Julia's own version manager, as a way to get Endeavor's pinned Julia
//! (`julia::Source::Own`). Where a computer has juliaup, Endeavor adds the pinned
//! version as a channel (`juliaup add 1.12.6`) rather than download a second Julia
//! of its own, so the person keeps one Julia manager, and `juliaup update` can't
//! move notebooks off the tested version. The person's default and other channels
//! stay as they were. On Windows, where juliaup is the only way Endeavor gets
//! Julia, it installs juliaup for this account first when there is none (no admin).
//!
//! juliaup is asked through `juliaup` itself, never through its `julia` launcher:
//! on a juliaup with no setup yet, the launcher first downloads the latest Julia
//! and makes it the default. Notebooks run the channel's real `julia`, not the
//! launcher, so ending the runtime ends what it started (EndeavorMCP #55).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// juliaup's id in the Microsoft Store, as its README installs it.
#[cfg(windows)]
const STORE_ID: &str = "9NJNWW8PVKMN";
/// juliaup's App Installer file, for a computer whose Store is blocked.
#[cfg(windows)]
const APP_INSTALLER: &str = "https://install.julialang.org/Julia.appinstaller";

/// How long each install route may take before the next is tried. A Store
/// install can sit queued behind other updates for good.
#[cfg(windows)]
const INSTALL_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long `juliaup add` may take: about 300 MB on a slow connection.
const ADD_LIMIT: Duration = Duration::from_secs(45 * 60);
/// How long juliaup may take to list or remove a channel.
const LIST_LIMIT: Duration = Duration::from_secs(60);

/// The `juliaup` program, if this computer has one: where juliaup's installer
/// puts it, else on the PATH (on Windows, also the Store app's alias, which the
/// PATH the app started with may not have yet).
pub fn find() -> Option<PathBuf> {
    let home = std::env::home_dir().map(|home| home.join(".juliaup").join("bin").join(EXE));
    home.into_iter().find(|juliaup| exists(juliaup)).or_else(on_path)
}

#[cfg(unix)]
const EXE: &str = "juliaup";
#[cfg(windows)]
const EXE: &str = "juliaup.exe";

/// The login shell's `juliaup` (Homebrew's, or juliaup's own installer's when
/// HOME's isn't where it put it).
#[cfg(unix)]
fn on_path() -> Option<PathBuf> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let output = Command::new(&shell).args(["-lc", "command -v juliaup"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.lines().rev().map(str::trim).find(|l| l.starts_with('/')).map(PathBuf::from);
    path.filter(|path| output.status.success() && path.is_file())
}

#[cfg(windows)]
fn on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let local = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft").join("WindowsApps"));
    path.into_iter().chain(local).map(|dir| dir.join(EXE)).find(|exe| exists(exe))
}

/// A Store app's alias is a reparse point that some checks can't follow, so
/// ask only whether something is there.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// The `julia` juliaup's channel `version` runs, if it has that channel.
pub fn channel_julia(juliaup: &Path, version: &str) -> Option<PathBuf> {
    let listed = run(Command::new(juliaup).args(["api", "getconfig1"]), LIST_LIMIT, &mut |_| {}).ok()?;
    channel_file(&listed, version).filter(|julia| julia.is_file())
}

/// Add the channel `version` to juliaup, which downloads that Julia, and give its
/// `julia`. `progress` hears how long it has taken so far.
pub fn add(juliaup: &Path, version: &str, progress: &mut dyn FnMut(String)) -> Result<PathBuf, String> {
    let what = format!("Downloading Julia {version} with juliaup");
    progress(format!("{what}…"));
    let added = run(Command::new(juliaup).args(["add", version]), ADD_LIMIT, &mut |waited| progress(format!("{what}… {} so far", minutes(waited))));
    channel_julia(juliaup, version).ok_or_else(|| {
        let why = added.err().map(|e| format!(" ({e})")).unwrap_or_default();
        format!("juliaup couldn't install Julia {version}{why}. Check the internet connection, then try again.")
    })
}

/// Remove juliaup's channel `version`, and the Julia it installed.
pub fn remove(juliaup: &Path, version: &str) -> Result<(), String> {
    run(Command::new(juliaup).args(["remove", version]), LIST_LIMIT, &mut |_| {}).map(|_| ()).map_err(|e| format!("juliaup couldn't remove Julia {version} ({e})."))
}

/// Install juliaup for this account: from the Microsoft Store with winget, else
/// from its App Installer file. Neither needs admin. The `juliaup.exe` installed.
#[cfg(windows)]
pub fn install(progress: &mut dyn FnMut(String)) -> Result<PathBuf, String> {
    progress("Installing juliaup, which installs Julia…".into());
    let mut quiet = |_: Duration| {};
    let store = run(
        Command::new("winget").args(["install", "--id", STORE_ID, "--exact", "--source", "msstore", "--accept-package-agreements", "--accept-source-agreements"]),
        INSTALL_LIMIT,
        &mut quiet,
    );
    let installed = match store {
        Ok(_) => {
            eprintln!("Installed juliaup from the Microsoft Store.");
            Ok(())
        }
        Err(store) => {
            eprintln!("Installing juliaup from the Microsoft Store failed ({store}); trying its App Installer file.");
            // Add-AppxPackage's own error is several lines that end with its error
            // id, so print the message alone, on one line.
            let script = format!(
                "try {{ Add-AppxPackage -AppInstallerFile '{APP_INSTALLER}' -ErrorAction Stop }} \
                 catch {{ [Console]::Error.WriteLine(($_.Exception.Message -replace '\\s+', ' ').Trim()); exit 1 }}"
            );
            let install = || run(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]), INSTALL_LIMIT, &mut |_| {});
            // One more try after a failure that wasn't the time limit: in the app's CI it
            // failed once in sixteen runs on Windows Server 2022, cause unknown.
            let file = install().or_else(|first| {
                if let Failed::TimedOut(_) = first {
                    return Err(first);
                }
                eprintln!("Installing juliaup from its App Installer file failed ({first}); trying once more.");
                std::thread::sleep(Duration::from_secs(5));
                install()
            });
            match file {
                Ok(_) => {
                    eprintln!("Installed juliaup from its App Installer file.");
                    Ok(())
                }
                Err(file) => Err(format!(
                    "Couldn't install juliaup, which Endeavor uses to install Julia. From the Microsoft Store: {store}. From its installer file: {file}. \
                     If this computer blocks app installs, install Julia from julialang.org/downloads and choose its julia.exe."
                )),
            }
        }
    };
    installed?;
    find().ok_or_else(|| {
        "juliaup was installed, but Endeavor can't find juliaup.exe. If Julia's app execution aliases are turned off in Windows Settings, turn them on; otherwise try again.".to_owned()
    })
}

/// Why a command `run` started didn't succeed.
enum Failed {
    /// It ran past its time limit and was stopped.
    TimedOut(Duration),
    /// It couldn't start, or it failed: its last words.
    Error(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Failed::TimedOut(limit) => write!(f, "it didn't finish within {}", minutes(*limit)),
            Failed::Error(why) => f.write_str(why),
        }
    }
}

/// Run `command` without a window or input, for at most `limit`, telling
/// `tick` every 15 s how long it has run; its output, or why it failed.
fn run(command: &mut Command, limit: Duration, tick: &mut dyn FnMut(Duration)) -> Result<String, Failed> {
    use std::io::Read;
    crate::client::no_window(command);
    // juliaup's errors end in a backtrace when these ask for one, and the last line is what's shown.
    command.env_remove("RUST_BACKTRACE").env_remove("RUST_LIB_BACKTRACE");
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| Failed::Error(e.to_string()))?;
    // Read both pipes while it runs, so a full pipe can't stall it.
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    let mut told = Duration::ZERO;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| Failed::Error(e.to_string()))? {
            break status;
        }
        let waited = started.elapsed();
        if waited >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failed::TimedOut(limit));
        }
        if waited >= told + Duration::from_secs(15) {
            told = waited;
            tick(waited);
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    if status.success() {
        return Ok(stdout);
    }
    // Its last words, from stderr if it wrote any (winget writes to stdout).
    let last = |text: &str| text.lines().map(str::trim).rfind(|l| !l.is_empty()).map(str::to_owned);
    Err(Failed::Error(last(stderr.as_str()).or_else(|| last(stdout.as_str())).unwrap_or_else(|| status.to_string())))
}

/// "4 min", or "30 s" under a minute.
fn minutes(time: Duration) -> String {
    match time.as_secs() {
        s if s < 60 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

/// The `File` of the channel named `version` in `juliaup api getconfig1`'s
/// JSON (`DefaultChannel` and `OtherChannels`): that channel's real julia.
fn channel_file(listed: &str, version: &str) -> Option<PathBuf> {
    let config: serde_json::Value = serde_json::from_str(listed.trim()).ok()?;
    let others = config["OtherChannels"].as_array().into_iter().flatten();
    std::iter::once(&config["DefaultChannel"]).chain(others).find(|c| c["Name"] == version).and_then(|c| c["File"].as_str()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_julia_is_the_file_of_the_channel_juliaup_lists() {
        let file = r"C:\Users\someone\.julia\juliaup\julia-1.12.6+0.x64.w64.mingw32\bin\julia.exe";
        let listed = serde_json::json!({
            "DefaultChannel": { "Name": "release", "File": r"C:\j\julia-1.12.7\bin\julia.exe", "Args": [], "Version": "1.12.7", "Arch": "x64" },
            "OtherChannels": [{ "Name": "1.12.6", "File": file, "Args": [], "Version": "1.12.6", "Arch": "x64" }],
        })
        .to_string();
        assert_eq!(channel_file(&listed, "1.12.6"), Some(PathBuf::from(file)));
        assert_eq!(channel_file(&listed, "1.12.5"), None);
        let as_default = serde_json::json!({ "DefaultChannel": { "Name": "1.12.6", "File": file }, "OtherChannels": [] }).to_string();
        assert_eq!(channel_file(&as_default, "1.12.6"), Some(PathBuf::from(file)));
        assert_eq!(channel_file(r#"{"DefaultChannel":null,"OtherChannels":[]}"#, "1.12.6"), None, "a juliaup with no channels yet");
        assert_eq!(channel_file("not json", "1.12.6"), None);
    }
}

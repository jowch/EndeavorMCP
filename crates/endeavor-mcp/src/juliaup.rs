use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const STORE_ID: &str = "9NJNWW8PVKMN";
#[cfg(windows)]
const APP_INSTALLER: &str = "https://install.julialang.org/Julia.appinstaller";

#[cfg(windows)]
const INSTALL_LIMIT: Duration = Duration::from_secs(10 * 60);
const ADD_LIMIT: Duration = Duration::from_secs(45 * 60);
const LIST_LIMIT: Duration = Duration::from_secs(60);
const RETRY_PAUSE: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 5 });

pub fn find() -> Option<PathBuf> {
    let home = std::env::home_dir().map(|home| home.join(".juliaup").join("bin").join(EXE));
    home.into_iter().find(|juliaup| exists(juliaup)).or_else(on_path)
}

#[cfg(unix)]
const EXE: &str = "juliaup";
#[cfg(windows)]
const EXE: &str = "juliaup.exe";

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

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub julia: PathBuf,
    pub default: bool,
}

pub fn listing(juliaup: &Path) -> Option<String> {
    run(Command::new(juliaup).args(["api", "getconfig1"]), LIST_LIMIT, &mut |_| {}).ok()
}

pub fn channel_julia(juliaup: &Path, version: &str) -> Option<Channel> {
    channel(&listing(juliaup)?, version).filter(|channel| channel.julia.is_file())
}

pub fn add(juliaup: &Path, version: &str, progress: &mut dyn FnMut(String)) -> Result<Channel, String> {
    let what = format!("Downloading Julia {version} with juliaup");
    progress(format!("{what}…"));
    let add = |again: &str, progress: &mut dyn FnMut(String)| run(Command::new(juliaup).args(["add", version]), ADD_LIMIT, &mut |waited| progress(format!("{what}…{again} {} so far", minutes(waited))));
    let mut added = add("", progress);
    if let Err(Failed::Error(first)) = &added
        && channel_julia(juliaup, version).is_none()
    {
        eprintln!("juliaup couldn't install Julia {version} ({first}); trying once more.");
        progress(format!("{what}… trying once more"));
        std::thread::sleep(RETRY_PAUSE);
        added = add(" trying once more,", progress);
    }
    channel_julia(juliaup, version).ok_or_else(|| match added {
        Err(why) => format!("juliaup couldn't install Julia {version} ({why})."),
        Ok(_) => format!("juliaup said it installed Julia {version}, but doesn't list it."),
    })
}

pub fn remove(juliaup: &Path, version: &str) -> Result<(), String> {
    run(Command::new(juliaup).args(["remove", version]), LIST_LIMIT, &mut |_| {}).map(|_| ()).map_err(|e| format!("juliaup couldn't remove Julia {version} ({e})."))
}

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
            let script = format!(
                "try {{ Add-AppxPackage -AppInstallerFile '{APP_INSTALLER}' -ErrorAction Stop }} \
                 catch {{ [Console]::Error.WriteLine(($_.Exception.Message -replace '\\s+', ' ').Trim()); exit 1 }}"
            );
            let install = || run(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]), INSTALL_LIMIT, &mut |_| {});
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

enum Failed {
    TimedOut(Duration),
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

fn run(command: &mut Command, limit: Duration, tick: &mut dyn FnMut(Duration)) -> Result<String, Failed> {
    use std::io::Read;
    crate::client::no_window(command);
    crate::host_tools::with_command_mask(command);
    command.env_remove("RUST_BACKTRACE").env_remove("RUST_LIB_BACKTRACE");
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| Failed::Error(e.to_string()))?;
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
    let program = Path::new(command.get_program()).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let said: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    for line in &said[said.len().saturating_sub(40)..] {
        eprintln!("{program}: {line}");
    }
    let last = |text: &str| text.lines().map(str::trim).rfind(|l| !l.is_empty()).map(str::to_owned);
    Err(Failed::Error(last_words(&stderr).or_else(|| last(stdout.as_str())).unwrap_or_else(|| status.to_string())))
}

fn last_words(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let chain = lines.iter().position(|l| *l == "Caused by:").map(|at| &lines[at + 1..]).unwrap_or_default();
    let cause = |line: &str| line.split_once(": ").filter(|(n, _)| n.parse::<u32>().is_ok()).map(|(_, text)| text.to_owned());
    let causes: Vec<String> = chain.iter().filter_map(|l| cause(l)).collect();
    match causes.as_slice() {
        [] => lines.last().map(|l| l.to_string()),
        [only] => Some(only.clone()),
        [.., step, why] => Some(format!("{}: {why}", step.trim_end_matches('.'))),
    }
}

fn minutes(time: Duration) -> String {
    match time.as_secs() {
        s if s < 60 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

pub fn channel(listed: &str, version: &str) -> Option<Channel> {
    let config: serde_json::Value = serde_json::from_str(listed.trim()).ok()?;
    let default = std::iter::once((&config["DefaultChannel"], true));
    let others = config["OtherChannels"].as_array().into_iter().flatten().map(|c| (c, false));
    let (found, default) = default.chain(others).find(|(c, _)| c["Name"] == version)?;
    Some(Channel { julia: PathBuf::from(found["File"].as_str()?), default })
}

pub fn default_julia(listed: &str) -> Option<Option<PathBuf>> {
    let config: serde_json::Value = serde_json::from_str(listed.trim()).ok()?;
    match config.get("DefaultChannel")? {
        serde_json::Value::Null => Some(None),
        default => Some(Some(PathBuf::from(default["File"].as_str()?))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_julia_is_the_default_channel_s_file_or_none_before_any_channel() {
        let listed = serde_json::json!({ "DefaultChannel": { "Name": "release", "File": "/j/julia-1.13.4/bin/julia" }, "OtherChannels": [] }).to_string();
        assert_eq!(default_julia(&listed), Some(Some(PathBuf::from("/j/julia-1.13.4/bin/julia"))));
        assert_eq!(default_julia(r#"{"DefaultChannel":null,"OtherChannels":[]}"#), Some(None));
        assert_eq!(default_julia(r#"{"DefaultChannel":{"Name":"mine"}}"#), None);
        assert_eq!(default_julia("Error: Failed to load configuration file"), None);
    }

    #[test]
    fn the_julia_is_the_file_of_the_channel_juliaup_lists() {
        let file = r"C:\Users\someone\.julia\juliaup\julia-1.12.6+0.x64.w64.mingw32\bin\julia.exe";
        let listed = serde_json::json!({
            "DefaultChannel": { "Name": "release", "File": r"C:\j\julia-1.12.7\bin\julia.exe", "Args": [], "Version": "1.12.7", "Arch": "x64" },
            "OtherChannels": [{ "Name": "1.12.6", "File": file, "Args": [], "Version": "1.12.6", "Arch": "x64" }],
        })
        .to_string();
        let julia = |default| Some(Channel { julia: PathBuf::from(file), default });
        assert_eq!(channel(&listed, "1.12.6"), julia(false));
        assert_eq!(channel(&listed, "1.12.5"), None);
        let as_default = serde_json::json!({ "DefaultChannel": { "Name": "1.12.6", "File": file }, "OtherChannels": [] }).to_string();
        assert_eq!(channel(&as_default, "1.12.6"), julia(true));
        assert_eq!(channel(r#"{"DefaultChannel":null,"OtherChannels":[]}"#, "1.12.6"), None, "a juliaup with no channels yet");
        assert_eq!(channel("not json", "1.12.6"), None);
    }

    #[test]
    fn a_juliaup_error_says_which_step_failed() {
        let error = "    Checking for new Julia versions\n  Installing Julia 1.12.6+0.x64.w64.mingw32\nError: Failed to extract downloaded file from url `https://example.org/julia.tar.gz`.\n\nCaused by:\n    0: Failed to move extracted archive to 'C:\\d\\juliaup\\julia-temp-3KkKKt'. Previous installation has been restored.\n    1: Access is denied. (os error 5)\n";
        assert_eq!(last_words(error).unwrap(), r"Failed to move extracted archive to 'C:\d\juliaup\julia-temp-3KkKKt'. Previous installation has been restored: Access is denied. (os error 5)");
        assert_eq!(last_words("Error: no such channel\n\nCaused by:\n    The channel 9.9 isn't known\n").unwrap(), "The channel 9.9 isn't known");
        let three = "Error: Failed to install\n\nCaused by:\n    0: Failed to extract\n    1: Failed to move\n    2: Access is denied. (os error 5)\n";
        assert_eq!(last_words(three).unwrap(), "Failed to move: Access is denied. (os error 5)", "the innermost step, not the first");
        assert_eq!(last_words("Error: the configuration file is locked\n").unwrap(), "Error: the configuration file is locked", "no chain: the last line");
        assert_eq!(last_words("\n"), None);
    }

    #[cfg(unix)]
    fn failing_juliaup(name: &str, fails: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("endeavor-juliaup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fails"), fails.to_string()).unwrap();
        let script = r#"#!/bin/sh
dir=$(dirname "$0")
case "$1" in
api) if [ -e "$dir/added" ]; then printf '{"DefaultChannel":{"Name":"1.12.6","File":"%s"},"OtherChannels":[]}' "$0"; else echo '{"DefaultChannel":null,"OtherChannels":[]}'; fi ;;
add) n=$(cat "$dir/fails"); echo $((n - 1)) > "$dir/fails"
     if [ "$n" -gt 0 ]; then echo "1: Access is denied. (os error 5)" >&2; exit 1; fi
     touch "$dir/added" ;;
esac
"#;
        let juliaup = dir.join("juliaup");
        crate::test_exec::write_executable(&juliaup, script);
        juliaup
    }

    #[cfg(unix)]
    #[test]
    fn an_add_that_fails_once_is_tried_again() {
        let juliaup = failing_juliaup("once", 1);
        let mut said = Vec::new();
        let added = add(&juliaup, "1.12.6", &mut |line| said.push(line));
        assert_eq!(added, Ok(Channel { julia: juliaup.clone(), default: true }));
        assert!(said.iter().any(|line| line.ends_with("trying once more")), "{said:?}");
        let _ = std::fs::remove_dir_all(juliaup.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn an_add_that_fails_twice_says_why() {
        let juliaup = failing_juliaup("twice", 2);
        let added = add(&juliaup, "1.12.6", &mut |_| {});
        assert_eq!(added, Err("juliaup couldn't install Julia 1.12.6 (1: Access is denied. (os error 5)).".to_owned()));
        assert_eq!(std::fs::read_to_string(juliaup.with_file_name("fails")).unwrap().trim(), "0", "tried twice, not more");
        let _ = std::fs::remove_dir_all(juliaup.parent().unwrap());
    }
}

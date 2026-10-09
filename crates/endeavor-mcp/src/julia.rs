//! Finding the julia to start the runtime with: a path the user gave, the one a
//! shell line of theirs sets up (`module load julia`), the one on their login
//! shell's PATH, or else Endeavor's own, downloaded and checked here.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Needed by runtime/Project.toml's `[sources]` section.
const MIN_JULIA: (u32, u32) = (1, 11);

/// The Julia downloaded when a machine has none, pinned with the official
/// tarballs' SHA-256 and size (bump all with the app's own in src/runtime.rs).
const JULIA_VERSION: &str = "1.12.6";
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
    match source {
        Source::Path(path) => {
            let path = expand_home(path);
            let version = checked_version(&path, &format!("Julia wasn't found at {path}."))?;
            Ok((path, version))
        }
        Source::Shell(line) => {
            let (path, err) = login_shell_julia(&format!("{line} >/dev/null && command -v julia"));
            let path = path.ok_or_else(|| {
                let why = err.lines().rev().find(|l| !l.trim().is_empty()).map(|l| format!(" ({})", l.trim())).unwrap_or_default();
                format!("Running `{line}` in a login shell didn't put julia on the PATH{why}.")
            })?;
            let version = checked_version(&path, &format!("`{line}` gave {path}, which doesn't run."))?;
            Ok((path, version))
        }
        Source::Auto => {
            if let Some(path) = path_julia() {
                match checked_version(&path, &format!("Found {path}, but it doesn't run.")) {
                    Ok(version) => return Ok((path, version)),
                    // Windows has no download to fall back on: say what's wrong with the one found.
                    Err(why) if cfg!(windows) => return Err(format!("{why} With juliaup, `juliaup update` or `juliaup default release` gives a newer one.").into()),
                    Err(_) => {}
                }
            }
            let path = own_julia(download, progress)?;
            let version = checked_version(&path, "Endeavor's Julia doesn't run on this machine.")?;
            Ok((path, version))
        }
    }
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
        _ => path.to_owned(),
    }
}

/// The julia on the login shell's PATH.
#[cfg(unix)]
fn path_julia() -> Option<String> {
    login_shell_julia("command -v julia").0
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

/// The last absolute path a login shell prints for `script` (profiles can
/// print other things first), and what it wrote to stderr.
fn login_shell_julia(script: &str) -> (Option<String>, String) {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let Ok(output) = Command::new(&shell).args(["-lc", script]).stdin(Stdio::null()).output() else {
        return (None, format!("couldn't run {shell}"));
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.lines().rev().map(str::trim).find(|l| l.starts_with('/')).map(str::to_owned);
    (path.filter(|_| output.status.success()), String::from_utf8_lossy(&output.stderr).into_owned())
}

/// `julia --version`, if it's new enough.
fn checked_version(julia: &str, missing: &str) -> Result<String, String> {
    let output = Command::new(julia).arg("--version").stdin(Stdio::null()).output().map_err(|_| missing.to_owned())?;
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

fn uname(flag: &str) -> String {
    Command::new("uname").arg(flag).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

/// `~/.cache/endeavor/julia-<version>/bin/julia`, downloading it the first time.
fn own_julia(download: bool, progress: &mut dyn FnMut(String)) -> Result<String, Failure> {
    if cfg!(windows) {
        return Err(format!(
            "No Julia {}.{} or newer on this computer's PATH, and Endeavor doesn't download Julia on Windows. Install it with juliaup (`winget install --id 9NJNWW8PVKMN -e -s msstore`), or pass its julia.exe with --julia, then try again.",
            MIN_JULIA.0, MIN_JULIA.1
        )
        .into());
    }
    let env = crate::paths::Env::here();
    if env.home.as_os_str().is_empty() {
        return Err("HOME isn't set".to_owned().into());
    }
    let cache = env.server_root();
    let dir = cache.join(format!("julia-{JULIA_VERSION}"));
    let bin = dir.join("bin/julia");
    if !bin.exists() {
        let (os, arch) = (uname("-s"), uname("-m").replace("arm64", "aarch64"));
        let &(_, _, url, sha256, size) = TARBALLS
            .iter()
            .find(|t| t.0 == os && t.1 == arch)
            .ok_or_else(|| format!("No julia on this machine's PATH, and Endeavor has no Julia download for {os} {arch}. Set How to get Julia for this server."))?;
        if !download {
            return Err(Failure::Missing(wire::Item {
                kind: wire::KIND_RUNTIME.into(),
                name: format!("Julia {JULIA_VERSION}"),
                size_mb: Some(size / 1_000_000),
                place: Some(dir.display().to_string()),
            }));
        }
        install(&cache, &dir, url, sha256, size, progress)?;
    }
    Ok(bin.display().to_string())
}

fn install(cache: &Path, dir: &Path, url: &str, sha256: &str, size: u64, progress: &mut dyn FnMut(String)) -> Result<(), String> {
    std::fs::create_dir_all(cache).map_err(|e| format!("Couldn't create {}: {e}", cache.display()))?;
    let top = format!("julia-{JULIA_VERSION}");
    let part = cache.join(format!("{top}.tar.gz.part"));
    let mut download = if has("curl") {
        Command::new("curl").args(["-fsSL", "--retry", "3", "-C", "-", "-o"]).arg(&part).arg(url).stderr(Stdio::piped()).spawn()
    } else if has("wget") {
        Command::new("wget").args(["-q", "-c", "-O"]).arg(&part).arg(url).stderr(Stdio::piped()).spawn()
    } else {
        return Err("No julia on this machine's PATH, and neither curl nor wget to download one. Set How to get Julia for this server.".into());
    }
    .map_err(|e| format!("Couldn't start the Julia download: {e}"))?;
    let mut shown = u64::MAX;
    let status = loop {
        if let Some(status) = download.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        let percent = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) * 100 / size;
        if percent != shown {
            shown = percent;
            progress(format!("Downloading Julia {JULIA_VERSION}… {percent}%"));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    if !status.success() {
        let mut err = String::new();
        let _ = std::io::Read::read_to_string(&mut download.stderr.take().unwrap(), &mut err);
        return Err(format!("Couldn't download Julia {JULIA_VERSION} on the server ({}). It resumes on the next try.", err.trim()));
    }

    progress(format!("Checking Julia {JULIA_VERSION}…"));
    let got = sha256_of(&part)?;
    if got != sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("The Julia {JULIA_VERSION} download was corrupt or tampered with (SHA-256 {got}); it was deleted."));
    }
    progress(format!("Unpacking Julia {JULIA_VERSION}…"));
    // Unpacked beside the target, then renamed, so a half-unpacked Julia is never used.
    let staging = cache.join(format!("{top}.unpacking"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let untar = Command::new("tar").arg("-xzf").arg(&part).arg("-C").arg(&staging).status().map_err(|e| e.to_string())?;
    if !untar.success() || !staging.join(&top).is_dir() {
        return Err(format!("Couldn't unpack Julia {JULIA_VERSION} ({untar})."));
    }
    std::fs::rename(staging.join(&top), dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&part);
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

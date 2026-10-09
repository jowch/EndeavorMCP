//! Finding the julia to start the runtime with: a path the user gave, the one a
//! shell line of theirs sets up (`module load julia`), the one on their login
//! shell's PATH (on Windows the PATH's), or else Endeavor's own, downloaded and
//! checked here.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Needed by runtime/Project.toml's `[sources]` section.
const MIN_JULIA: (u32, u32) = (1, 11);

/// The Julia downloaded when a machine has none, pinned with the official
/// tarballs' (Windows: zip's) SHA-256 and size (bump all with the app's own in
/// src/runtime.rs).
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

/// Windows' Julia. There is no Windows ARM64 build of Julia 1.12, so Windows
/// on ARM gets the x64 one, which it runs under emulation.
const WINDOWS_ZIP: (&str, &str, u64) = (
    "https://julialang-s3.julialang.org/bin/winnt/x64/1.12/julia-1.12.6-win64.zip",
    "a63d991976e6893f508c512e3dc7bca1836c1a1f6ad1f3e4aedec159b6733e89",
    275_091_967,
);

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// `--julia PATH`
    Path(String),
    /// `--julia auto`: the login shell's (on Windows the PATH's), else Endeavor's own.
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
            if let Some(path) = path_julia()
                && let Ok(version) = checked_version(&path, &format!("Found {path}, but it doesn't run.")) {
                return Ok((path, version));
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

fn uname(flag: &str) -> String {
    Command::new("uname").arg(flag).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

/// The download for this machine: its URL, SHA-256 and size.
fn download_here() -> Result<(&'static str, &'static str, u64), String> {
    if cfg!(windows) {
        return Ok(WINDOWS_ZIP);
    }
    let (os, arch) = (uname("-s"), uname("-m").replace("arm64", "aarch64"));
    TARBALLS
        .iter()
        .find(|t| t.0 == os && t.1 == arch)
        .map(|&(_, _, url, sha256, size)| (url, sha256, size))
        .ok_or_else(|| format!("No julia on this machine's PATH, and Endeavor has no Julia download for {os} {arch}. Set How to get Julia for this server."))
}

/// `~/.cache/endeavor/julia-<version>/bin/julia`, downloading it the first time.
/// On Windows `%LOCALAPPDATA%\Endeavor\julia-<version>\bin\julia.exe`, the
/// app's own Julia, so the two share one install.
fn own_julia(download: bool, progress: &mut dyn FnMut(String)) -> Result<String, Failure> {
    let env = crate::paths::Env::here();
    let cache = if cfg!(windows) {
        if !env.local.is_absolute() {
            return Err("LOCALAPPDATA isn't set, so Endeavor has no folder to download Julia into.".to_owned().into());
        }
        env.local.clone()
    } else {
        if env.home.as_os_str().is_empty() {
            return Err("HOME isn't set".to_owned().into());
        }
        env.server_root()
    };
    let dir = cache.join(format!("julia-{JULIA_VERSION}"));
    let bin = dir.join("bin").join(if cfg!(windows) { "julia.exe" } else { "julia" });
    if !bin.exists() {
        let (url, sha256, size) = download_here()?;
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
    let kind = if url.ends_with(".zip") { "zip" } else { "tar.gz" };
    let part = cache.join(format!("{top}.{kind}.part"));
    let mut download = if has("curl") {
        let mut curl = Command::new("curl");
        curl.args(["-fsSL", "--retry", "3", "-C", "-", "-o"]).arg(&part).arg(url);
        curl
    } else if has("wget") {
        let mut wget = Command::new("wget");
        wget.args(["-q", "-c", "-O"]).arg(&part).arg(url);
        wget
    } else {
        return Err("No julia on this machine's PATH, and neither curl nor wget to download one. Set How to get Julia for this server.".into());
    };
    crate::client::no_window(&mut download);
    let mut download = download.stderr(Stdio::piped()).spawn().map_err(|e| format!("Couldn't start the Julia download: {e}"))?;
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
        return Err(format!("Couldn't download Julia {JULIA_VERSION} ({}). It resumes on the next try.", err.trim()));
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
    let untar = unpack(&part, &staging)?;
    if !untar.success() || !staging.join(&top).is_dir() {
        return Err(format!("Couldn't unpack Julia {JULIA_VERSION} ({untar})."));
    }
    // Another install (the app's, on Windows) may have finished first, and on
    // Windows an antivirus scan can hold a just-unpacked file for a moment.
    let mut tries = if cfg!(windows) { 10 } else { 1 };
    while let Err(e) = std::fs::rename(staging.join(&top), dir) {
        if dir.join("bin").is_dir() {
            break;
        }
        tries -= 1;
        if tries == 0 {
            return Err(format!("Couldn't move the unpacked Julia {JULIA_VERSION} into place ({e})."));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&part);
    Ok(())
}

#[cfg(unix)]
fn has(program: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {program}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Windows 10 and later ship `curl.exe`; there is no `wget`.
#[cfg(windows)]
fn has(program: &str) -> bool {
    let mut curl = Command::new("curl");
    crate::client::no_window(&mut curl);
    program == "curl" && curl.arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn sha256_of(path: &Path) -> Result<String, String> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::open(path).map_err(|e| format!("Couldn't check the download: {e}"))?;
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("Couldn't check the download: {e}"))?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn unpack(tarball: &Path, into: &Path) -> Result<std::process::ExitStatus, String> {
    Command::new("tar").arg("-xzf").arg(tarball).arg("-C").arg(into).status().map_err(|e| e.to_string())
}

/// With Windows' own bsdtar, which reads zips. A `tar` earlier on the PATH may
/// be GNU tar from Git for Windows, which doesn't, and takes `C:` for a host.
#[cfg(windows)]
fn unpack(zip: &Path, into: &Path) -> Result<std::process::ExitStatus, String> {
    let system = std::env::var_os("SystemRoot").map(|root| std::path::PathBuf::from(root).join("System32").join("tar.exe"));
    let tar = system.filter(|tar| tar.exists()).unwrap_or_else(|| "tar.exe".into());
    let mut command = Command::new(tar);
    crate::client::no_window(&mut command);
    command.arg("-xf").arg(zip).arg("-C").arg(into).status().map_err(|e| format!("Couldn't unpack Julia: Windows' tar.exe didn't start ({e})."))
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

    #[test]
    fn the_checksum_is_sha256() {
        let file = crate::client::scratch("julia-sha").join("abc");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(sha256_of(&file).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// The download is checked and unpacked whole: a tar.gz, and on Windows a
    /// zip, from a `file://` address with curl.
    #[test]
    fn installs_a_checked_download_and_deletes_a_bad_one() {
        // Without the `\\?\` a canonical Windows path has, as a real cache path is.
        let tmp = crate::client::scratch("julia-install");
        let tmp = tmp.to_str().and_then(|p| p.strip_prefix(r"\\?\")).map(std::path::PathBuf::from).unwrap_or(tmp);
        let top = format!("julia-{JULIA_VERSION}");
        let bin = tmp.join("src").join(&top).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("julia"), "").unwrap();
        let kind = if cfg!(windows) { "zip" } else { "tar.gz" };
        let archive = tmp.join(format!("julia.{kind}"));
        let mut pack = if cfg!(windows) { Command::new(std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_default()).join("System32").join("tar.exe")) } else { Command::new("tar") };
        // -a: the format from the file name.
        pack.args(if cfg!(windows) { ["-a", "-cf"].as_slice() } else { ["-czf"].as_slice() });
        assert!(pack.arg(&archive).arg("-C").arg(tmp.join("src")).arg(&top).status().unwrap().success());
        let url = crate::client::file_url(&archive.display().to_string());
        let size = std::fs::metadata(&archive).unwrap().len();
        let sha = sha256_of(&archive).unwrap();

        let cache = tmp.join("cache");
        let dir = cache.join(&top);
        let err = install(&cache, &dir, &url, &"0".repeat(64), size, &mut |_| {}).unwrap_err();
        assert!(err.contains("corrupt"), "{err}");
        assert!(!dir.exists() && !cache.join(format!("{top}.{kind}.part")).exists());

        install(&cache, &dir, &url, &sha, size, &mut |_| {}).unwrap();
        assert!(dir.join("bin").join("julia").exists());
        assert!(!cache.join(format!("{top}.{kind}.part")).exists() && !cache.join(format!("{top}.unpacking")).exists());
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

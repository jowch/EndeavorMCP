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

/// Needed by runtime/Project.toml's `[sources]` section.
const MIN_JULIA: (u32, u32) = (1, 11);

/// Endeavor's own Julia, the tested version, pinned with the official tarballs'
/// SHA-256 and size (bump all with the app's own in src/runtime.rs).
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
    let (path, version) = found(source, download, progress)?;
    Ok((real_julia(path), version))
}

fn found(source: &Source, download: bool, progress: &mut dyn FnMut(String)) -> Result<(String, String), Failure> {
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
                && let Ok(version) = checked_version(&path, &format!("Found {path}, but it doesn't run."))
            {
                return Ok((path, version));
            }
            let path = own_julia(download, progress)?;
            let version = checked_version(&path, "Endeavor's Julia doesn't run on this machine.")?;
            Ok((path, version))
        }
        Source::Own => {
            let path = own_julia(download, progress)?;
            let version = checked_version(&path, "Endeavor's Julia doesn't run on this machine.")?;
            if version != JULIA_VERSION {
                return Err(format!("Endeavor's Julia should be {JULIA_VERSION}, but {path} is {version}.").into());
            }
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

/// juliaup's `julia.exe` (the Store app's alias, or juliaup's own launcher)
/// only starts the real one. Started through the alias, that launcher and the
/// Julia under it run outside the core's job, so ending the core left Julia
/// running (EndeavorMCP #55). Start the `julia.exe` in that Julia's own
/// `Sys.BINDIR` instead. A julia.exe with Julia's library beside it is the
/// real one already, and isn't asked. If asking fails, the path as found.
#[cfg(windows)]
fn real_julia(julia: String) -> String {
    if Path::new(&julia).parent().is_some_and(|dir| dir.join("libjulia.dll").is_file()) {
        return julia;
    }
    let mut command = Command::new(&julia);
    crate::client::no_window(&mut command);
    let output = command.args(["--startup-file=no", "--history-file=no", "-e", "print(Sys.BINDIR)"]).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let Ok(output) = output else { return julia };
    if !output.status.success() {
        return julia;
    }
    match bindir_julia(&String::from_utf8_lossy(&output.stdout)) {
        Some(exe) if exe.is_file() => exe.display().to_string(),
        _ => julia,
    }
}

#[cfg(unix)]
fn real_julia(julia: String) -> String {
    julia
}

/// The julia.exe in the `Sys.BINDIR` Julia printed (the last line: a startup
/// message may come first).
#[cfg_attr(unix, allow(dead_code))]
fn bindir_julia(printed: &str) -> Option<std::path::PathBuf> {
    let bindir = printed.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    Some(Path::new(bindir).join("julia.exe"))
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
    let channel = crate::juliaup::channel_julia(&crate::juliaup::find()?, JULIA_VERSION)?;
    Some(OwnJulia { julia: channel.julia, from: OwnFrom::Juliaup { added: marked(), default: channel.default } })
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
        #[cfg(windows)]
        Err(why) => return Err(why),
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
                crate::juliaup::remove(&juliaup, JULIA_VERSION)?;
            }
        }
        let _ = std::fs::remove_file(mark);
    }
    Ok(())
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
fn tarball() -> Result<(&'static str, &'static str, u64), String> {
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
    let untar = Command::new("tar").arg("-xzf").arg(&part).arg("-C").arg(&staging).status().map_err(|e| e.to_string())?;
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
    let mut download = if has("curl") {
        // A download that stalls (under 1 kB/s for 5 minutes) fails, rather than wait for good; the next try resumes it.
        Command::new("curl").args(["-fsSL", "--retry", "3", "--speed-limit", "1000", "--speed-time", "300", "-C", "-", "-o"]).arg(part).arg(url).stderr(Stdio::piped()).spawn()
    } else if has("wget") {
        Command::new("wget").args(["-q", "-c", "--read-timeout=300", "-O"]).arg(part).arg(url).stderr(Stdio::piped()).spawn()
    } else {
        return Err(format!("Couldn't download {name}: there is neither curl nor wget."));
    }
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
    fn juliaups_launcher_gives_way_to_the_julia_exe_in_its_bindir() {
        let bindir = r"C:\Users\someone\.julia\juliaup\julia-1.12.6+0.x64.w64.mingw32\bin";
        assert_eq!(bindir_julia(bindir), Some(Path::new(bindir).join("julia.exe")));
        assert_eq!(bindir_julia(&format!("a startup message\n{bindir}\r\n")), Some(Path::new(bindir).join("julia.exe")));
        assert_eq!(bindir_julia(" \n"), None);
    }

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

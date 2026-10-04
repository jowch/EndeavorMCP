//! `endeavor --version`, and `endeavor update`: replace this binary with the
//! newest build on the Helpers release, the one its `LATEST` file names.
//!
//! Only Linux has prebuilt binaries. A copy the Endeavor app installed, or
//! one cargo installed, is left to the app or to cargo.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{embedded, standalone};

/// The Helpers release; `ENDEAVOR_RELEASE_URL` replaces it (tests serve one locally).
const RELEASE: &str = "https://github.com/jowch/EndeavorMCP/releases/download/helpers";
const RELEASE_ENV: &str = "ENDEAVOR_RELEASE_URL";
const CARGO_INSTALL: &str = "cargo install --git https://github.com/jowch/EndeavorMCP endeavor-mcp";
const USAGE: &str = "usage: endeavor update   replace this binary with the newest build from the Helpers release";

/// This binary's version line: the package version, and the build
/// (`embedded::BUILD_VERSION`), which tells apart builds of one version.
pub(crate) fn version_line() -> String {
    format!("endeavor {} (build {})", env!("CARGO_PKG_VERSION"), embedded::BUILD_VERSION)
}

pub(crate) fn print_version() -> ! {
    println!("{}", version_line());
    std::process::exit(0)
}

/// What `update` works from.
struct Here {
    /// This binary, links resolved.
    exe: PathBuf,
    /// The Helpers release's name for this platform, if it has binaries for it.
    platform: Option<&'static str>,
    home: PathBuf,
    cargo_home: PathBuf,
    release: String,
    /// Where a runtime from the old build would be recorded.
    state_dir: PathBuf,
}

impl Here {
    fn now() -> Result<Here, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let exe = std::env::current_exe().and_then(|exe| exe.canonicalize()).map_err(|e| format!("Couldn't find this binary: {e}"))?;
        let home = std::env::home_dir().unwrap_or_default();
        let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some("linux-x86_64"),
            ("linux", "aarch64") => Some("linux-aarch64"),
            _ => None,
        };
        Ok(Here {
            exe,
            platform,
            cargo_home: var("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from),
            home,
            release: var(RELEASE_ENV).unwrap_or_else(|| RELEASE.into()),
            state_dir: standalone::default_state_dir(),
        })
    }
}

/// `endeavor update`.
pub(crate) fn main(argv: &[String]) -> ! {
    match argv.first().map(String::as_str) {
        None => {}
        Some("--help" | "-h") => {
            println!("{USAGE}");
            std::process::exit(0)
        }
        Some(arg) => {
            eprintln!("unknown argument {arg}\n{USAGE}");
            std::process::exit(2)
        }
    }
    match Here::now().and_then(|here| update(&here)) {
        Ok(message) => {
            println!("{message}");
            std::process::exit(0)
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1)
        }
    }
}

/// Update `here.exe`, or say why not. The message for the user either way.
fn update(here: &Here) -> Result<String, String> {
    let exe = &here.exe;
    let dir = exe.parent().ok_or_else(|| format!("{} has no folder", exe.display()))?;
    // The app installs its helper as ~/.cache/endeavor/<version>/endeavor and
    // reinstalls only when that folder is missing, so a replaced file there
    // would run under the wrong version's name.
    if dir.parent().is_some_and(|d| same(d, &here.home.join(".cache/endeavor"))) || exe.ancestors().any(|a| a.extension().is_some_and(|e| e == "app")) {
        return Err(format!("This copy of endeavor ({}) belongs to the Endeavor app, which installs and updates it. Update the app instead.", exe.display()));
    }
    if same(dir, &here.cargo_home.join("bin")) {
        return Err(format!("This copy of endeavor was installed with cargo. Update it the same way:\n    {CARGO_INSTALL}"));
    }
    let Some(platform) = here.platform else {
        return Err(format!(
            "There's no prebuilt endeavor for this platform ({} {}). Reinstall it with cargo install:\n    {CARGO_INSTALL}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };

    let release = here.release.trim_end_matches('/');
    let key = String::from_utf8_lossy(&download(&format!("{release}/LATEST"), None)?).trim().to_owned();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("The release's LATEST file doesn't name a build ({key:?})."));
    }
    let name = format!("endeavor-{key}-{platform}");
    let sums = String::from_utf8_lossy(&download(&format!("{release}/endeavor-{key}.sha256"), None)?).into_owned();
    let want = sums
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(_, file)| file.trim().trim_start_matches('*') == name)
        .map(|(sum, _)| sum.to_lowercase())
        .ok_or_else(|| format!("The newest build ({key}) has no binary for {platform}."))?;
    if sha256_of(exe)? == want {
        return Ok(format!("endeavor is up to date: {} is the newest build ({key}).", exe.display()));
    }

    let part = dir.join(format!(".endeavor.part.{}", std::process::id()));
    let installed = install(&format!("{release}/{name}"), &part, &want, exe);
    let _ = std::fs::remove_file(&part);
    let new_build = installed?;
    let mut message = format!("Updated {} to the newest build ({key}).", exe.display());
    if running(&here.state_dir)
        && let Some(note) = standalone::other_build_than(&here.state_dir, new_build.as_deref().unwrap_or_default())
    {
        message.push_str(&format!("\n{note}"));
    }
    Ok(message)
}

/// Whether `a` and `b` are one folder, links resolved where they exist.
fn same(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    real(a) == real(b)
}

/// Download `url` to `part`, check it against `sha256`, and put it in place of
/// `exe`. The new binary's build, if it says.
fn install(url: &str, part: &Path, sha256: &str, exe: &Path) -> Result<Option<String>, String> {
    download(url, Some(part))?;
    let got = sha256_of(part)?;
    if got != sha256 {
        return Err(format!(
            "The download from {url} doesn't match its checksum (SHA-256 {got}, expected {sha256}). It was deleted; {} is unchanged.",
            exe.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(part, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("Couldn't make {} executable: {e}", part.display()))?;
    }
    let build = Command::new(part).arg("--version").output().ok().and_then(|out| {
        let line = String::from_utf8_lossy(&out.stdout).into_owned();
        Some(line.split_once("(build ")?.1.split_once(')')?.0.to_owned())
    });
    // Atomic within one folder; a process running the old binary (a
    // runtime's core) keeps the file it started from.
    std::fs::rename(part, exe).map_err(|e| format!("Couldn't replace {}: {e}", exe.display()))?;
    Ok(build)
}

/// Whether a runtime recorded in `dir` is running on this machine.
fn running(dir: &Path) -> bool {
    crate::read_state(dir).is_some_and(|state| state.node == crate::hostname() && crate::pid_alive(state.pid, state.started))
}

/// `url`'s body, or with `to`, nothing, having written it there. With curl,
/// or wget where there's no curl.
fn download(url: &str, to: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", "--retry", "2"]);
    let mut wget = Command::new("wget");
    wget.arg("-q");
    match to {
        Some(path) => {
            curl.arg("-o").arg(path);
            wget.arg("-O").arg(path);
        }
        None => {
            wget.args(["-O", "-"]);
        }
    }
    let output = match curl.arg(url).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => wget.arg(url).output().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "endeavor update needs curl or wget, and neither is on the PATH.".to_owned(),
            _ => format!("Couldn't start wget: {e}"),
        })?,
        other => other.map_err(|e| format!("Couldn't start curl: {e}"))?,
    };
    if !output.status.success() {
        return Err(format!("Couldn't download {url} ({}).", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(all(test, unix))]
mod tests;

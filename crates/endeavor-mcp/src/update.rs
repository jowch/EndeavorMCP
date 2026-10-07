//! `endeavor --version`, and `endeavor update`: replace this binary with the
//! newest build on the Helpers release, the one its `LATEST` file names.
//!
//! The release has binaries for Linux, macOS and Windows (`release::platform_name`).
//! A copy the Endeavor app installed, or one cargo installed, is left to the
//! app, to cargo or to a plugin.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::release::{asset_name, checksum_for, download, sha256_of, this_platform_name};
use crate::{embedded, paths, release, standalone};

const CARGO_INSTALL: &str = "cargo install --git https://github.com/jowch/EndeavorMCP endeavor-mcp";
const USAGE: &str = "usage: endeavor update   replace this binary with the newest build from the Helpers release";

/// This binary's version line: the package version, and the build
/// (`embedded::BUILD_VERSION`), which tells apart builds of one version.
pub(crate) fn version_line() -> String {
    format!("endeavor {} (build {})", env!("CARGO_PKG_VERSION"), embedded::BUILD_VERSION)
}

/// `version_line`, and on a release build a second line, `release <key>`.
pub(crate) fn print_version() -> ! {
    println!("{}", version_line());
    if let Some(key) = embedded::RELEASE_KEY {
        println!("release {key}");
    }
    std::process::exit(0)
}

/// What `update` works from.
struct Here {
    /// This binary, links resolved.
    exe: PathBuf,
    /// The Helpers release's name for this platform, if it has binaries for it.
    platform: Option<&'static str>,
    /// `~/.cache/endeavor`, where the app installs its helper (`paths::Env::server_root`).
    server_root: PathBuf,
    cargo_home: PathBuf,
    /// Where the plugins' launcher keeps binaries (`paths::Env::plugin_bin`).
    plugin_bin: PathBuf,
    release: String,
    /// Where a runtime from the old build would be recorded.
    state_dir: PathBuf,
}

impl Here {
    fn now() -> Result<Here, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let exe = std::env::current_exe().and_then(|exe| exe.canonicalize()).map_err(|e| format!("Couldn't find this binary: {e}"))?;
        let env = paths::Env::here();
        Ok(Here {
            exe,
            platform: this_platform_name(),
            cargo_home: var("CARGO_HOME").map_or_else(|| env.home.join(".cargo"), PathBuf::from),
            plugin_bin: env.plugin_bin(),
            server_root: env.server_root(),
            release: release::base_url(),
            state_dir: env.state_dir(),
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
    if dir.parent().is_some_and(|d| same(d, &here.server_root)) || exe.ancestors().any(|a| a.extension().is_some_and(|e| e == "app")) {
        return Err(format!("This copy of endeavor ({}) belongs to the Endeavor app, which installs and updates it. Update the app instead.", exe.display()));
    }
    if same(dir, &here.cargo_home.join("bin")) {
        return Err(format!("This copy of endeavor was installed with cargo. Update it the same way:\n    {CARGO_INSTALL}"));
    }
    if dir.parent().is_some_and(|d| same(d, &here.plugin_bin) || d.parent().is_some_and(|b| same(b, &paths::plugin_bin_from(&here.plugin_bin)))) {
        return Err(format!(
            "This copy of endeavor ({}) belongs to the Endeavor plugin, which manages it. The plugin fetches a newer build when it is updated or, if it isn't pinned to one, at the start of a Claude Code session and once a day in the background.\nTo fetch the newest now, run the plugin's launch/endeavor-mcp.sh with --fetch-only (`sh <the plugin's folder>/launch/endeavor-mcp.sh --fetch-only`; the folder is different for each agent).",
            exe.display()
        ));
    }
    let Some(platform) = here.platform else {
        return Err(format!(
            "There's no prebuilt endeavor for this platform ({} {}). Reinstall it with cargo install:\n    {CARGO_INSTALL}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };

    clear_asides(exe, cfg!(windows));
    let release = here.release.trim_end_matches('/');
    let key = String::from_utf8_lossy(&download(&format!("{release}/LATEST"), None)?).trim().to_owned();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("The release's LATEST file doesn't name a build ({key:?})."));
    }
    let name = asset_name(&key, platform);
    let sums = String::from_utf8_lossy(&download(&format!("{release}/endeavor-{key}.sha256"), None)?).into_owned();
    let want = checksum_for(&sums, &name).ok_or_else(|| format!("The newest build ({key}) has no binary for {platform}."))?;
    if sha256_of(exe)? == want {
        return Ok(format!("endeavor is up to date: {} is the newest build ({key}).", exe.display()));
    }

    let part = dir.join(format!(".endeavor.part.{}{}", std::process::id(), if cfg!(windows) { ".exe" } else { "" }));
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
    put_in_place(part, exe, cfg!(windows))?;
    Ok(build)
}

/// Where `put_in_place` moves a running binary that can't be overwritten: a
/// name of its own, since an earlier one may still be running from its aside.
fn aside_name(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_owned();
    name.push(format!(".old-{}", std::process::id()));
    exe.with_file_name(name)
}

/// With `aside` (Windows), remove the binaries earlier updates moved aside
/// that no process is running from any more; a running one can't be removed.
fn clear_asides(exe: &Path, aside: bool) {
    let Some((dir, name)) = aside.then(|| exe.parent().zip(exe.file_name())).flatten() else { return };
    let prefix = format!("{}.old", name.to_string_lossy());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let found = entry.file_name().to_string_lossy().into_owned();
        if found == prefix || found.strip_prefix(&prefix).is_some_and(|rest| rest.starts_with('-')) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Make `part` the binary `exe`. A rename within one folder is atomic, and a
/// process running the old binary (a runtime's core) keeps the file it
/// started from. Windows can rename a running exe but not replace it, so with
/// `aside` the old one is renamed out of the way first, and put back if the
/// new one can't be moved in.
fn put_in_place(part: &Path, exe: &Path, aside: bool) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("Couldn't replace {}: {e}", exe.display());
    if !aside {
        return std::fs::rename(part, exe).map_err(fail);
    }
    let old = aside_name(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(fail)?;
    std::fs::rename(part, exe).map_err(|e| {
        let _ = std::fs::rename(&old, exe);
        fail(e)
    })
}

/// Whether a runtime recorded in `dir` is running on this machine.
fn running(dir: &Path) -> bool {
    crate::runtime::look(dir, false, false).alive().is_some()
}

#[cfg(all(test, unix))]
mod tests;

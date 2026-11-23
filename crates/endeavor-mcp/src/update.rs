use std::path::{Path, PathBuf};
use std::process::Command;

use crate::release::{asset_name, checksum_for, download, read_pin, sha256_of, this_platform_name};
use crate::{embedded, paths, release, standalone};

const CARGO_INSTALL: &str = "cargo install --git https://github.com/jowch/EndeavorMCP endeavor-mcp";
const USAGE: &str = "usage: endeavor update   replace this binary with the newest build from the Helpers release";

pub(crate) fn version_line() -> String {
    format!("endeavor {} (build {})", env!("CARGO_PKG_VERSION"), embedded::BUILD_VERSION)
}

pub(crate) fn print_version() -> ! {
    println!("{}", version_line());
    if let Some(key) = embedded::RELEASE_KEY {
        println!("release {key}");
    }
    println!("interface {}", crate::core::INTERFACE);
    std::process::exit(0)
}

fn build_and_interface(output: &str) -> (Option<String>, Option<u32>) {
    let build = output.lines().next().and_then(|line| Some(line.split_once("(build ")?.1.split_once(')')?.0.to_owned()));
    let interface = output.lines().find_map(|line| line.strip_prefix("interface ")?.trim().parse().ok());
    (build, interface)
}

struct Here {
    exe: PathBuf,
    platform: Option<&'static str>,
    server_root: PathBuf,
    cargo_home: PathBuf,
    plugin_bin: PathBuf,
    release: String,
    pin: Option<String>,
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
            pin: release::pin_url(),
            state_dir: crate::runtime::default_state_dir(&env),
        })
    }
}

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
    if let Some(refusal) = app_helper_refusal(crate::HELPER_ARGS.get().copied().unwrap_or_default()) {
        eprintln!("{refusal}");
        std::process::exit(1)
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

fn app_helper_refusal(helper_args: &[&str]) -> Option<String> {
    (!helper_args.is_empty()).then(|| "This is the Endeavor app, running as its own helper. The app installs and updates itself; update the app instead.".to_owned())
}

fn update(here: &Here) -> Result<String, String> {
    let exe = &here.exe;
    let dir = exe.parent().ok_or_else(|| format!("{} has no folder", exe.display()))?;
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
    let skip = format!("{} is unchanged. To update to the release's newest build checked only against the release's own checksum file, run it again with {}=none.", exe.display(), release::PIN_ENV);
    let pin = match here.pin.as_deref().map(|url| (url, read_pin(url))) {
        Some((_, Ok(Some(pin)))) => Some(pin),
        Some((url, Ok(None))) => return Err(format!("{url} doesn't pin a build with checksums. {skip}")),
        Some((_, Err(e))) => return Err(format!("Couldn't read the build main pins ({e}), so the download can't be checked against it. {skip}")),
        None => None,
    };
    let pinned = pin.is_some();
    let (key, name, want) = match pin {
        Some((key, sums)) => {
            let name = asset_name(&key, platform);
            let want = checksum_for(&sums, &name).ok_or_else(|| format!("The checksums main pins list no binary of build {key} for {platform}."))?;
            (key, name, want)
        }
        None => {
            let key = String::from_utf8_lossy(&download(&format!("{release}/LATEST"), None)?).trim().to_owned();
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!("The release's LATEST file doesn't name a build ({key:?})."));
            }
            let name = asset_name(&key, platform);
            let sums = String::from_utf8_lossy(&download(&format!("{release}/endeavor-{key}.sha256"), None)?).into_owned();
            let want = checksum_for(&sums, &name).ok_or_else(|| format!("The newest build ({key}) has no binary for {platform}."))?;
            (key, name, want)
        }
    };
    if sha256_of(exe)? == want {
        return Ok(format!("endeavor is up to date: {} is the newest build ({key}).", exe.display()));
    }

    let part = dir.join(format!(".endeavor.part.{}{}", std::process::id(), if cfg!(windows) { ".exe" } else { "" }));
    let installed = install(&format!("{release}/{name}"), &part, &want, pinned, exe);
    let _ = std::fs::remove_file(&part);
    let (new_build, new_interface) = installed?;
    let mut message = format!("Updated {} to the newest build ({key}).", exe.display());
    if running(&here.state_dir)
        && let Some(note) = standalone::other_build_than(&here.state_dir, new_build.as_deref().unwrap_or_default(), new_interface)
    {
        message.push_str(&format!("\n{note}"));
    }
    Ok(message)
}

fn same(a: &Path, b: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    real(a) == real(b)
}

fn install(url: &str, part: &Path, sha256: &str, pinned: bool, exe: &Path) -> Result<(Option<String>, Option<u32>), String> {
    download(url, Some(part))?;
    let got = sha256_of(part)?;
    if got != sha256 {
        let against = if pinned { "the checksum main pins" } else { "its checksum" };
        return Err(format!(
            "The download from {url} doesn't match {against} (SHA-256 {got}, expected {sha256}). It was deleted; {} is unchanged.",
            exe.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(part, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("Couldn't make {} executable: {e}", part.display()))?;
    }
    let said = Command::new(part).arg("--version").output().map_or((None, None), |out| build_and_interface(&String::from_utf8_lossy(&out.stdout)));
    put_in_place(part, exe, cfg!(windows))?;
    Ok(said)
}

fn aside_name(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_owned();
    name.push(format!(".old-{}", std::process::id()));
    exe.with_file_name(name)
}

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

fn running(dir: &Path) -> bool {
    crate::runtime::look(dir, false, false).alive().is_some()
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
mod app_helper_tests {
    use super::app_helper_refusal;

    #[test]
    fn the_app_running_as_its_helper_refuses_to_update() {
        assert!(app_helper_refusal(&["--helper"]).is_some_and(|m| m.contains("update the app")));
        assert_eq!(app_helper_refusal(&[]), None);
    }
}

//! The Helpers release on GitHub: its address, downloading from it and
//! checking SHA-256, the platform names its files use, and the helper for a
//! server whose platform isn't this computer's. `update` and the link use it.
//!
//! The checksum file comes from the same release as the binary, so a check
//! catches a corrupt or cut-short download and not a release that was
//! tampered with.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{client, embedded};

/// The Helpers release; `ENDEAVOR_RELEASE_URL` replaces it (tests serve one locally; `file://` works too).
const RELEASE: &str = "https://github.com/jowch/EndeavorMCP/releases/download/helpers";
pub(crate) const RELEASE_ENV: &str = "ENDEAVOR_RELEASE_URL";

/// The release's address without a trailing slash.
pub(crate) fn base_url() -> String {
    std::env::var(RELEASE_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| RELEASE.into()).trim_end_matches('/').to_owned()
}

/// The release's name for a platform given as `uname` words after `client`'s
/// mapping (`linux`, `darwin` or `windows`, and `x86_64` or `aarch64`), if it
/// has binaries for it.
pub(crate) fn platform_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("darwin", "x86_64") => Some("darwin-x86_64"),
        ("darwin", "aarch64") => Some("darwin-aarch64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        _ => None,
    }
}

/// The release's name for this computer's platform.
pub(crate) fn this_platform_name() -> Option<&'static str> {
    let (os, arch) = client::this_platform();
    platform_name(&os, &arch)
}

/// The file the release holds for `platform` and `key`.
pub(crate) fn asset_name(key: &str, platform: &str) -> String {
    let exe = if platform.starts_with("windows-") { ".exe" } else { "" };
    format!("endeavor-{key}-{platform}{exe}")
}

/// `bytes`' SHA-256 in lower-case hex.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// The checksum a release's `endeavor-<key>.sha256` (`sha256sum`'s output) gives for `name`.
pub(crate) fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().filter_map(|line| line.split_once(char::is_whitespace)).find(|(_, file)| file.trim().trim_start_matches('*') == name).map(|(sum, _)| sum.to_lowercase())
}

/// `url`'s body, or with `to`, nothing, having written it there. With curl,
/// or wget where there's no curl.
pub(crate) fn download(url: &str, to: Option<&Path>) -> Result<Vec<u8>, String> {
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
            std::io::ErrorKind::NotFound => "Downloading needs curl or wget, and neither is on the PATH.".to_owned(),
            _ => format!("Couldn't start wget: {e}"),
        })?,
        other => other.map_err(|e| format!("Couldn't start curl: {e}"))?,
    };
    if !output.status.success() {
        return Err(format!("Couldn't download {url} ({}).", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(output.stdout)
}

/// The helper to send to a server of platform `os` and `arch`, as `client`
/// names them, fetched from the release this build belongs to.
pub(crate) fn helper_for(os: &str, arch: &str, cache: &Path) -> Result<PathBuf, String> {
    fetch_helper(os, arch, embedded::RELEASE_KEY, &base_url(), cache)
}

/// `endeavor-<key>-<platform>` from `release`, checked against that key's
/// checksum file and kept as `cache/<key>/<platform>/endeavor`. A kept file is
/// used again once its checksum, recorded beside it, still matches. A
/// download that fails its check is deleted and nothing is kept.
pub(crate) fn fetch_helper(os: &str, arch: &str, key: Option<&str>, release: &str, cache: &Path) -> Result<PathBuf, String> {
    let platform = platform_name(os, arch).filter(|_| os != "windows").ok_or_else(|| client::no_helper(os, arch))?;
    let key = key.ok_or_else(|| format!("A helper for {os} {arch} servers needs a release build of Endeavor, installed with the install script, and this build has none."))?;
    let dir = cache.join(key).join(platform);
    let (kept, recorded) = (dir.join("endeavor"), dir.join("endeavor.sha256"));
    if let (Ok(want), Ok(have)) = (std::fs::read_to_string(&recorded), sha256_of(&kept))
        && want.trim() == have
    {
        return Ok(kept);
    }
    let _ = std::fs::remove_file(&kept);
    let _ = std::fs::remove_file(&recorded);

    let release = release.trim_end_matches('/');
    let name = asset_name(key, platform);
    let sums = download(&format!("{release}/endeavor-{key}.sha256"), None).map_err(|e| format!("Couldn't get the helper for {os} {arch} servers from the release: {e}"))?;
    let want = checksum_for(&String::from_utf8_lossy(&sums), &name).ok_or_else(|| format!("The release has no helper for {os} {arch} servers in build {key} ({name} isn't in its checksum file)."))?;
    make_private_dir(&dir)?;
    let part = dir.join(format!("endeavor.part.{}", std::process::id()));
    let fetched = download(&format!("{release}/{name}"), Some(&part)).map_err(|e| format!("Couldn't get the helper for {os} {arch} servers from the release: {e}")).and_then(|_| {
        let got = sha256_of(&part)?;
        if got != want {
            return Err(format!("The helper for {os} {arch} servers downloaded from {release}/{name} doesn't match its checksum (SHA-256 {got}, expected {want}). It was deleted."));
        }
        make_private(&part)?;
        std::fs::write(&recorded, &want).map_err(|e| format!("Couldn't write {}: {e}", recorded.display()))?;
        std::fs::rename(&part, &kept).map_err(|e| format!("Couldn't keep the helper in {}: {e}", kept.display()))
    });
    let _ = std::fs::remove_file(&part);
    if let Err(e) = fetched {
        let _ = std::fs::remove_file(&recorded);
        return Err(e);
    }
    Ok(kept)
}

/// `dir` and the folders above it that don't exist yet, readable only by the user.
fn make_private_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))
}

fn make_private(file: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("Couldn't restrict {}: {e}", file.display()))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(test)]
mod tests;

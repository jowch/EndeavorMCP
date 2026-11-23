use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::{client, embedded};

const RELEASE: &str = "https://github.com/jowch/EndeavorMCP/releases/download/helpers";
pub(crate) const RELEASE_ENV: &str = "ENDEAVOR_RELEASE_URL";

pub(crate) fn base_url() -> String {
    std::env::var(RELEASE_ENV).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| RELEASE.into()).trim_end_matches('/').to_owned()
}

const PIN: &str = "https://raw.githubusercontent.com/jowch/EndeavorMCP/main/scripts/release-key";
pub(crate) const PIN_ENV: &str = "ENDEAVOR_PIN_URL";

pub(crate) fn pin_url() -> Option<String> {
    match std::env::var(PIN_ENV) {
        Ok(url) => Some(url).filter(|url| !url.is_empty() && url != "none"),
        Err(_) if std::env::var(RELEASE_ENV).is_ok_and(|v| !v.is_empty()) => None,
        Err(_) => Some(PIN.into()),
    }
}

pub(crate) fn pinned_key(text: &str) -> Option<&str> {
    let mut lines = text.lines().map(str::trim);
    let key = lines.next().filter(|key| !key.is_empty() && key.chars().all(|c| c.is_ascii_hexdigit()))?;
    let is_sum = |line: &str| line.split_whitespace().next().is_some_and(|sum| sum.len() == 64 && sum.chars().all(|c| c.is_ascii_hexdigit()));
    lines.any(is_sum).then_some(key)
}

pub(crate) fn read_pin(url: &str) -> Result<Option<(String, String)>, String> {
    let text = String::from_utf8_lossy(&download(url, None)?).into_owned();
    Ok(pinned_key(&text).map(str::to_owned).map(|key| (key, text)))
}

fn pinned_sums(key: &str, pin: Option<&str>) -> Option<String> {
    if let Some(text) = embedded::PINNED_SUMS
        && pinned_key(text) == Some(key)
    {
        return Some(text.to_owned());
    }
    read_pin(pin?).ok().flatten().filter(|(pinned, _)| pinned == key).map(|(_, text)| text)
}

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

pub(crate) fn this_platform_name() -> Option<&'static str> {
    let (os, arch) = client::this_platform();
    platform_name(&os, &arch)
}

pub(crate) fn asset_name(key: &str, platform: &str) -> String {
    let exe = if platform.starts_with("windows-") { ".exe" } else { "" };
    format!("endeavor-{key}-{platform}{exe}")
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sha256_of(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

pub(crate) fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().filter_map(|line| line.split_once(char::is_whitespace)).find(|(_, file)| file.trim().trim_start_matches('*') == name).map(|(sum, _)| sum.to_lowercase())
}

const CONNECT_SECS: &str = "15";
const FILE_SECS: &str = "900";
const TEXT_SECS: &str = "60";

pub(crate) fn download(url: &str, to: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut curl = Command::new("curl");
    curl.args(["-fsSL", "--retry", "2", "--connect-timeout", CONNECT_SECS, "--max-time", if to.is_some() { FILE_SECS } else { TEXT_SECS }, "--proto-redir", "=https"]);
    client::no_window(&mut curl);
    crate::host_tools::with_command_mask(&mut curl);
    if let Some(path) = to {
        curl.arg("-o").arg(path);
    }
    let (output, by_wget) = match curl.arg(url).output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            wget(url, to).output().map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => "Downloading needs curl or wget, and neither is on the PATH.".to_owned(),
                _ => format!("Couldn't start wget: {e}"),
            })?,
            true,
        ),
        other => (other.map_err(|e| format!("Couldn't start curl: {e}"))?, false),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(to_where) = redirect_not_https(url, &stderr).filter(|_| by_wget) {
        if let Some(path) = to {
            let _ = std::fs::remove_file(path);
        }
        return Err(format!("Couldn't download {url}: it was redirected to {to_where}, which isn't https, so the download was thrown away."));
    }
    if !output.status.success() {
        let reason = if by_wget { stderr.lines().map(str::trim).rfind(|l| l.starts_with("HTTP/")).unwrap_or("wget failed").to_owned() } else { stderr.into_owned() };
        return Err(format!("Couldn't download {url} ({}).", reason.trim()));
    }
    Ok(output.stdout)
}

fn wget(url: &str, to: Option<&Path>) -> Command {
    let mut wget = Command::new("wget");
    client::no_window(&mut wget);
    crate::host_tools::with_command_mask(&mut wget);
    wget.args(["-q", "-S", "--tries=3"]);
    wget.arg(format!("--connect-timeout={CONNECT_SECS}")).arg(format!("--timeout={TEXT_SECS}"));
    if is_wget2() && url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://")) {
        wget.arg("--https-enforce=hard");
    }
    match to {
        Some(path) => wget.arg("-O").arg(path),
        None => wget.args(["-O", "-"]),
    };
    wget.arg(url);
    wget
}

fn is_wget2() -> bool {
    let mut version = Command::new("wget");
    client::no_window(&mut version);
    version.arg("--version").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    version.output().is_ok_and(|out| String::from_utf8_lossy(&out.stdout).lines().next().is_some_and(|line| line.starts_with("GNU Wget2")))
}

pub(crate) fn redirect_not_https(url: &str, headers: &str) -> Option<String> {
    let is_https = |u: &str| u.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"));
    let mut https = is_https(url);
    for line in headers.lines() {
        let line = line.trim();
        let Some(to) = line.get(..9).filter(|s| s.eq_ignore_ascii_case("location:")).map(|_| line[9..].trim()) else { continue };
        let scheme = to.split_once(':').map(|(s, _)| s).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && s.starts_with(|c: char| c.is_ascii_alphabetic()));
        if scheme.is_some() {
            https = is_https(to);
        }
        if !https {
            return Some(to.to_owned());
        }
    }
    None
}

pub(crate) fn helper_for(os: &str, arch: &str, cache: &Path) -> Result<PathBuf, String> {
    fetch_helper(os, arch, embedded::RELEASE_KEY, &base_url(), pin_url().as_deref(), cache, &removed_build)
}

fn removed_build(key: &str, os: &str, arch: &str) -> String {
    format!("Build {key} of endeavor is old and was removed from the release, so it can't get the helper for {os} {arch} servers. Update endeavor: update the plugin, or run `endeavor update` if you installed it with the install script.")
}

pub fn release_helper(os: &str, arch: &str, key: &str, cache: &Path, removed: &dyn Fn(&str, &str, &str) -> String) -> Result<PathBuf, String> {
    fetch_helper(os, arch, Some(key), &base_url(), pin_url().as_deref(), cache, removed)
}

const LOCK_WAIT: Duration = Duration::from_secs(1200);

pub(crate) fn fetch_helper(os: &str, arch: &str, key: Option<&str>, release: &str, pin: Option<&str>, cache: &Path, removed: &dyn Fn(&str, &str, &str) -> String) -> Result<PathBuf, String> {
    let platform = platform_name(os, arch).filter(|_| os != "windows").ok_or_else(|| client::no_helper(os, arch))?;
    let key = key.ok_or_else(|| format!("A helper for {os} {arch} servers needs a release build of Endeavor, installed with the install script, and this build has none."))?;
    let dir = cache.join(key).join(platform);
    make_private_dir(&dir)?;
    let _lock = lock(&dir)?;
    let kept = dir.join("endeavor");
    let helper = if is_good(&dir) { kept } else { fetch(os, arch, key, platform, release.trim_end_matches('/'), pin, &dir, removed)? };
    prune(cache, key);
    Ok(helper)
}

fn is_good(dir: &Path) -> bool {
    matches!((std::fs::read_to_string(dir.join("endeavor.sha256")), sha256_of(&dir.join("endeavor"))), (Ok(want), Ok(have)) if want.trim() == have)
}

#[allow(clippy::too_many_arguments)]
fn fetch(os: &str, arch: &str, key: &str, platform: &str, release: &str, pin: Option<&str>, dir: &Path, removed: &dyn Fn(&str, &str, &str) -> String) -> Result<PathBuf, String> {
    let (kept, recorded) = (dir.join("endeavor"), dir.join("endeavor.sha256"));
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().contains(".part.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let name = asset_name(key, platform);
    let missing = |e: String| if was_removed(release, key) { removed(key, os, arch) } else { format!("Couldn't get the helper for {os} {arch} servers from the release: {e}") };
    let pinned = pinned_sums(key, pin);
    let want = match &pinned {
        Some(sums) => checksum_for(sums, &name).ok_or_else(|| format!("The checksums pinned for build {key} list no helper for {os} {arch} servers ({name})."))?,
        None => {
            let sums = download(&format!("{release}/endeavor-{key}.sha256"), None).map_err(missing)?;
            checksum_for(&String::from_utf8_lossy(&sums), &name).ok_or_else(|| format!("The release has no helper for {os} {arch} servers in build {key} ({name} isn't in its checksum file)."))?
        }
    };
    let against = if pinned.is_some() { format!("the checksum pinned for build {key}") } else { "its checksum".to_owned() };
    let pid = std::process::id();
    let (part, part_sum) = (dir.join(format!("endeavor.part.{pid}")), dir.join(format!("endeavor.sha256.part.{pid}")));
    let fetched = download(&format!("{release}/{name}"), Some(&part)).map_err(|e| if pinned.is_some() { missing(e) } else { format!("Couldn't get the helper for {os} {arch} servers from the release: {e}") }).and_then(|_| {
        let got = sha256_of(&part)?;
        if got != want {
            return Err(format!("The helper for {os} {arch} servers downloaded from {release}/{name} doesn't match {against} (SHA-256 {got}, expected {want}). It was deleted."));
        }
        make_private(&part)?;
        std::fs::write(&part_sum, &want).map_err(|e| format!("Couldn't write {}: {e}", part_sum.display()))?;
        make_private(&part_sum)?;
        std::fs::rename(&part, &kept).map_err(|e| format!("Couldn't keep the helper in {}: {e}", kept.display()))?;
        std::fs::rename(&part_sum, &recorded).map_err(|e| format!("Couldn't write {}: {e}", recorded.display()))
    });
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_file(&part_sum);
    fetched.map(|()| kept)
}

fn was_removed(release: &str, key: &str) -> bool {
    let Ok(newest) = download(&format!("{release}/LATEST"), None) else { return false };
    let newest = String::from_utf8_lossy(&newest).trim().to_owned();
    !newest.is_empty() && newest != key && newest.chars().all(|c| c.is_ascii_hexdigit())
}

fn lock(dir: &Path) -> Result<File, String> {
    let path = dir.join("endeavor.lock");
    let file = crate::owner_only(OpenOptions::new().read(true).write(true).create(true).truncate(false)).open(&path).map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    let started = Instant::now();
    while !crate::try_lock(&file) {
        if started.elapsed() > LOCK_WAIT {
            return Err(format!("Another Endeavor process has held {} for too long.", path.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(file)
}

fn prune(cache: &Path, key: &str) {
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        if entry.file_name() != key && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

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

use std::path::{Path, PathBuf};
use std::process::Command;

pub const VERSION: &str = "0.11.32";

const TARBALLS: [(&str, &str, &str, &str, u64); 4] = [
    ("Darwin", "aarch64", "aarch64-apple-darwin", "ed336d0ba49db8ef89b2b41fffa372ce63bd032f22a56f001c265891aec32829", 22_510_462),
    ("Darwin", "x86_64", "x86_64-apple-darwin", "77f5ca26c0de20e992a3677a174fe1121ee25c36f9b1434a863f75bf077a05eb", 24_228_395),
    ("Linux", "x86_64", "x86_64-unknown-linux-gnu", "aab924fd522efd06f1c5f3b93a243864fc453132c94b2dc49f1371b528a4b967", 26_296_576),
    ("Linux", "aarch64", "aarch64-unknown-linux-gnu", "4d4fa08d95b06642e5800df6a22bd71455f23f988269e18da2847971d8c0bf31", 24_555_223),
];

struct Tarball {
    target: &'static str,
    sha256: &'static str,
    size: u64,
}

impl Tarball {
    fn url(&self) -> String {
        format!("https://github.com/astral-sh/uv/releases/download/{VERSION}/uv-{}.tar.gz", self.target)
    }
}

fn tarball_for(os: &str, arch: &str) -> Option<Tarball> {
    let arch = arch.replace("arm64", "aarch64");
    TARBALLS.iter().find(|t| t.0 == os && t.1 == arch).map(|&(_, _, target, sha256, size)| Tarball { target, sha256, size })
}

fn tarball() -> Option<Tarball> {
    tarball_for(&crate::julia::uname("-s"), &crate::julia::uname("-m"))
}

fn own_dir(env: &crate::paths::Env) -> Option<PathBuf> {
    (!env.home.as_os_str().is_empty()).then(|| env.server_root().join(format!("uv-{VERSION}")))
}

pub fn own() -> Option<PathBuf> {
    own_dir(&crate::paths::Env::here()).map(|dir| dir.join("uv")).filter(|uv| uv.is_file())
}

pub fn item() -> Option<wire::Item> {
    if cfg!(windows) {
        return None;
    }
    let (tarball, dir) = (tarball()?, own_dir(&crate::paths::Env::here())?);
    Some(wire::Item { kind: wire::KIND_RUNTIME.into(), name: format!("uv {VERSION}"), size_mb: Some(tarball.size / 1_000_000), place: Some(dir.display().to_string()) })
}

pub fn install(progress: &mut dyn FnMut(String)) -> Result<PathBuf, String> {
    if let Some(uv) = own() {
        return Ok(uv);
    }
    let env = crate::paths::Env::here();
    let (tarball, dir) = match (tarball(), own_dir(&env)) {
        (Some(tarball), Some(dir)) => (tarball, dir),
        (None, _) => return Err(format!("Endeavor has no uv {VERSION} for this computer. Install uv (https://docs.astral.sh/uv/getting-started/installation/), then try again.")),
        (_, None) => return Err("HOME isn't set".into()),
    };
    let (root, name) = (env.server_root(), format!("uv {VERSION}"));
    std::fs::create_dir_all(&root).map_err(|e| format!("Couldn't create {}: {e}", root.display()))?;
    let part = root.join(format!("uv-{VERSION}-{}.tar.gz.part", tarball.target));
    crate::julia::download(&part, &tarball.url(), tarball.sha256, tarball.size, &name, progress)?;
    progress(format!("Unpacking {name}…"));
    let staging = root.join(format!("uv-{VERSION}.unpacking"));
    let unpacked = unpack(&part, &staging, &format!("uv-{}", tarball.target), &dir);
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&part);
    unpacked.map_err(|why| format!("Couldn't unpack {name}: {why}"))?;
    own().ok_or(format!("{name} was unpacked, but there is no uv in {}", dir.display()))
}

fn unpack(tarball: &Path, staging: &Path, top: &str, dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).map_err(|e| e.to_string())?;
    let mut untar = Command::new("tar");
    crate::host_tools::with_command_mask(untar.arg("-xzf").arg(tarball).arg("-C").arg(staging));
    let status = untar.status().map_err(|e| e.to_string())?;
    let unpacked = staging.join(top);
    if !status.success() || !unpacked.join("uv").is_file() {
        return Err(format!("tar: {status}"));
    }
    std::fs::rename(&unpacked, dir).map_err(|e| format!("Couldn't move uv into {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mac_and_linux_endeavor_runs_on_has_a_pinned_uv() {
        for (os, arch) in [("Darwin", "arm64"), ("Darwin", "x86_64"), ("Linux", "x86_64"), ("Linux", "aarch64")] {
            let tarball = tarball_for(os, arch).unwrap_or_else(|| panic!("{os} {arch}"));
            assert!(tarball.url().starts_with("https://github.com/astral-sh/uv/releases/download/") && tarball.url().ends_with(&format!("/uv-{}.tar.gz", tarball.target)));
            assert_eq!(tarball.sha256.len(), 64);
        }
        assert!(tarball_for("Linux", "riscv64").is_none());
    }

    #[test]
    fn endeavors_own_uv_is_in_its_own_folder_by_version() {
        let env = crate::paths::Env { home: PathBuf::from("/h"), ..crate::paths::Env::here() };
        assert_eq!(own_dir(&env), Some(PathBuf::from(format!("/h/.cache/endeavor/uv-{VERSION}"))));
        assert_eq!(own_dir(&crate::paths::Env { home: PathBuf::new(), ..env }), None);
    }
}

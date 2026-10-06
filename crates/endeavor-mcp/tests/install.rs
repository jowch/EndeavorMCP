//! `scripts/install.sh` run as a user runs it, against a release that is a
//! folder under `target/tmp` (`ENDEAVOR_RELEASE_URL=file://…`), with `HOME`
//! there too.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";
const NEW: &[u8] = b"#!/bin/sh\necho 'endeavor 0.1.0 (build 0.1.0-00000000000000aa)'\n";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The platform `install.sh` picks on this computer.
fn platform() -> String {
    let os = if cfg!(target_os = "macos") { "darwin" } else { "linux" };
    format!("{os}-{}", std::env::consts::ARCH)
}

struct Place {
    dir: PathBuf,
}

impl Place {
    fn new(name: &str) -> Place {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("install-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("release")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Place { dir }
    }

    /// A release holding NEW for this platform, the checksum file saying `sum`.
    fn release(&self, sum: &str) {
        let name = format!("endeavor-{KEY}-{}", platform());
        self.publish(&format!("{KEY}\n"), &format!("{}  endeavor-{KEY}-other-platform\n{sum}  {name}\n", sha(b"other")));
        std::fs::write(self.dir.join("release").join(name), NEW).unwrap();
    }

    fn publish(&self, latest: &str, sums: &str) {
        std::fs::write(self.dir.join("release/LATEST"), latest).unwrap();
        std::fs::write(self.dir.join(format!("release/endeavor-{KEY}.sha256")), sums).unwrap();
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn run(&self, args: &[&str], env: &[(&str, String)]) -> (bool, String, String) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
        let mut command = Command::new("sh");
        command.arg(script).args(args).env("HOME", self.home()).env("ENDEAVOR_RELEASE_URL", format!("file://{}", self.dir.join("release").display())).env_remove("ENDEAVOR_INSTALL_DIR");
        for (name, value) in env {
            command.env(name, value);
        }
        let out = command.output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    /// What is in `dir`, apart from the binary.
    fn others(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n != "endeavor").collect()).unwrap_or_default()
    }
}

#[test]
fn it_installs_the_newest_build_as_an_executable_and_says_how_to_reach_it() {
    let place = Place::new("installs");
    place.release(&sha(NEW));
    let bin = place.dir.join("bin");
    let (ok, stdout, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[]);
    assert!(ok, "{stderr}");
    let exe = bin.join("endeavor");
    assert_eq!(std::fs::read(&exe).unwrap(), NEW);
    assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(Place::others(&bin), Vec::<String>::new(), "nothing left beside it");
    assert!(stdout.contains(&format!("Installed endeavor (build {KEY}, {}) in {}", platform(), exe.display())), "{stdout}");
    assert!(stdout.contains("endeavor 0.1.0 (build 0.1.0-00000000000000aa)"), "it ran: {stdout}");
    assert!(stdout.contains(&format!("{} isn't on your PATH", bin.display())) && stdout.contains(&format!("export PATH=\"{}:$PATH\"", bin.display())), "{stdout}");
    assert!(stdout.contains("install the plugin for your agent"), "{stdout}");
    let ran = Command::new(&exe).arg("--version").output().unwrap();
    assert!(ran.status.success());
}

#[test]
fn the_default_folder_is_local_bin_and_no_hint_comes_when_it_is_on_the_path() {
    let place = Place::new("default");
    place.release(&sha(NEW));
    let (ok, stdout, stderr) = place.run(&[], &[]);
    assert!(ok, "{stderr}");
    let bin = place.home().join(".local/bin");
    assert_eq!(std::fs::read(bin.join("endeavor")).unwrap(), NEW);
    assert!(stdout.contains("export PATH=\"$HOME/.local/bin:$PATH\""), "{stdout}");

    let on_path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let (ok, stdout, stderr) = place.run(&[], &[("PATH", on_path)]);
    assert!(ok, "{stderr}");
    assert!(!stdout.contains("PATH"), "{stdout}");
}

#[test]
fn the_folder_can_come_from_a_variable() {
    let place = Place::new("variable");
    place.release(&sha(NEW));
    let bin = place.dir.join("elsewhere/bin");
    let (ok, _, stderr) = place.run(&[], &[("ENDEAVOR_INSTALL_DIR", bin.display().to_string())]);
    assert!(ok, "{stderr}");
    assert_eq!(std::fs::read(bin.join("endeavor")).unwrap(), NEW);
    assert!(!place.home().join(".local").exists());
}

#[test]
fn an_existing_binary_is_replaced() {
    let place = Place::new("replaces");
    place.release(&sha(NEW));
    let bin = place.dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("endeavor"), "old").unwrap();
    let (ok, _, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[]);
    assert!(ok, "{stderr}");
    assert_eq!(std::fs::read(bin.join("endeavor")).unwrap(), NEW);
    assert_eq!(Place::others(&bin), Vec::<String>::new());
}

#[test]
fn a_download_that_fails_its_checksum_is_refused_and_nothing_is_installed() {
    let place = Place::new("checksum");
    place.release(&sha(b"something else"));
    let bin = place.dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("endeavor"), "old").unwrap();
    let (ok, _, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[]);
    assert!(!ok);
    assert!(stderr.contains("doesn't match its checksum") && stderr.contains("nothing was installed"), "{stderr}");
    assert_eq!(std::fs::read(bin.join("endeavor")).unwrap(), b"old");
    assert_eq!(Place::others(&bin), Vec::<String>::new());
}

#[test]
fn a_release_without_this_platform_says_so() {
    let place = Place::new("missing");
    place.publish(&format!("{KEY}\n"), &format!("{}  endeavor-{KEY}-other-platform\n", sha(NEW)));
    let bin = place.dir.join("bin");
    let (ok, _, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[]);
    assert!(!ok);
    assert!(stderr.contains(&format!("the newest build ({KEY}) has no binary for {}", platform())), "{stderr}");
    assert!(!bin.join("endeavor").exists());

    // Listed, but the file isn't on the release.
    place.publish(&format!("{KEY}\n"), &format!("{}  endeavor-{KEY}-{}\n", sha(NEW), platform()));
    let (ok, _, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[]);
    assert!(!ok);
    assert!(stderr.contains("couldn't download"), "{stderr}");
    assert_eq!(Place::others(&bin), Vec::<String>::new());
}

#[test]
fn a_latest_file_that_names_no_build_is_refused() {
    let place = Place::new("latest");
    place.publish("<html>not found</html>\n", "");
    let (ok, _, stderr) = place.run(&["--dir", place.dir.join("bin").to_str().unwrap()], &[]);
    assert!(!ok);
    assert!(stderr.contains("LATEST file doesn't name a build"), "{stderr}");
}

#[test]
fn a_platform_without_builds_is_a_plain_error_naming_it() {
    let place = Place::new("platform");
    place.release(&sha(NEW));
    let fake = place.dir.join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("uname"), "#!/bin/sh\n[ \"$1\" = -s ] && echo FreeBSD || echo riscv64\n").unwrap();
    std::fs::set_permissions(fake.join("uname"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let bin = place.dir.join("bin");
    let (ok, _, stderr) = place.run(&["--dir", bin.to_str().unwrap()], &[("PATH", path)]);
    assert!(!ok);
    assert!(stderr.contains("there is no endeavor for FreeBSD riscv64"), "{stderr}");
    assert!(!bin.exists());
}

#[test]
fn an_unknown_argument_is_refused() {
    let (ok, _, stderr) = Place::new("argument").run(&["--sudo"], &[]);
    assert!(!ok && stderr.contains("unknown argument --sudo"), "{stderr}");
}

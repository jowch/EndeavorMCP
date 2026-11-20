//! scripts/publish-helpers.sh, the Helpers workflow's Publish step, run as the
//! workflow runs it but with a fake `gh` whose release is a folder.

// The workflow runs it on Linux (sha256sum).
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A checkout-like folder with `dist/<platform>/endeavor[.exe]` for `built`, a
/// release folder holding `earlier`, and a fake `gh` on the PATH that serves it.
struct Run {
    dir: PathBuf,
}

impl Run {
    fn new(name: &str, built: &[&str], earlier: &[(&str, &[u8])]) -> Run {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("publish-helpers-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["work", "release", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for platform in built {
            let exe = if platform.starts_with("windows-") { "endeavor.exe" } else { "endeavor" };
            std::fs::create_dir_all(dir.join("work/dist").join(platform)).unwrap();
            std::fs::write(dir.join("work/dist").join(platform).join(exe), format!("new {platform}")).unwrap();
        }
        for (name, body) in earlier {
            std::fs::write(dir.join("release").join(name), body).unwrap();
        }
        // view (with --json: the asset names), download --pattern P --dir D, upload FILES..., edit, create.
        let gh = format!(
            r#"#!/bin/sh
release='{release}'
echo "gh $*" >> '{log}'
[ "$1" = release ] || exit 2
shift
case $1 in
  view) shift; shift; [ "$1" = --json ] && ls "$release"; exit 0 ;;
  download) shift; shift; while [ $# -gt 0 ]; do case $1 in --pattern) p=$2; shift;; --dir) d=$2; shift;; esac; shift; done; mkdir -p "$d"; cp "$release/$p" "$d/" ;;
  upload) shift; shift; for f in "$@"; do case $f in --*) ;; *) cp "$f" "$release/" ;; esac; done ;;
  edit|create) ;;
  *) exit 2 ;;
esac
"#,
            release = dir.join("release").display(),
            log = dir.join("gh.log").display(),
        );
        std::fs::write(dir.join("bin/gh"), gh).unwrap();
        std::fs::set_permissions(dir.join("bin/gh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Run { dir }
    }

    fn publish(&self) -> std::process::Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/publish-helpers.sh");
        Command::new("sh")
            .arg(script)
            .current_dir(self.dir.join("work"))
            .env("PATH", format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap()))
            .env("KEY", KEY)
            .env("GITHUB_REF", "refs/heads/main")
            .env("GITHUB_SHA", "0000000")
            .env("GITHUB_OUTPUT", self.dir.join("output"))
            .output()
            .unwrap()
    }

    /// The published checksum file, as (sum, file) sorted by file.
    fn sums(&self) -> Vec<(String, String)> {
        let text = std::fs::read_to_string(self.dir.join(format!("release/endeavor-{KEY}.sha256"))).unwrap();
        let mut lines: Vec<(String, String)> = text.lines().map(|l| l.split_once("  ").map(|(s, f)| (s.to_owned(), f.to_owned())).unwrap_or_else(|| panic!("not sha256sum's form: {l:?}"))).collect();
        lines.sort_by(|a, b| a.1.cmp(&b.1));
        lines
    }
}

fn name(platform: &str) -> String {
    format!("endeavor-{KEY}-{platform}{}", if platform.starts_with("windows-") { ".exe" } else { "" })
}

const ALL: [&str; 5] = ["darwin-aarch64", "darwin-x86_64", "linux-aarch64", "linux-x86_64", "windows-x86_64"];

#[test]
fn a_run_whose_mac_and_windows_builds_failed_keeps_the_checksums_an_earlier_run_published() {
    // An earlier run of this key published every build (one line in sha256sum's binary-mode form, and a blank line).
    let earlier = format!(
        "{}  {}\n{} *{}\n\n{}  {}\n{}  {}\n{}  {}\n",
        sha(b"old darwin-aarch64"), name("darwin-aarch64"),
        sha(b"old darwin-x86_64"), name("darwin-x86_64"),
        sha(b"old linux-aarch64"), name("linux-aarch64"),
        sha(b"old linux-x86_64"), name("linux-x86_64"),
        sha(b"old windows-x86_64"), name("windows-x86_64"),
    );
    let run = Run::new("partial", &["linux-aarch64", "linux-x86_64"], &[(&format!("endeavor-{KEY}.sha256"), earlier.as_bytes())]);
    let out = run.publish();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let want: Vec<(String, String)> = ALL.iter().map(|p| (sha(if p.starts_with("linux-") { format!("new {p}") } else { format!("old {p}") }.as_bytes()), name(p))).collect();
    assert_eq!(run.sums(), want, "the Linux builds this run made have their new sums, the others keep the earlier ones, each once");
    assert_eq!(std::fs::read_to_string(run.dir.join("release/LATEST")).unwrap().trim(), KEY);
}

#[test]
fn a_binary_already_published_under_the_key_is_never_replaced() {
    // The plugins pin each build's checksums, so a re-run of a key leaves what it published alone.
    let earlier = ALL.iter().filter(|p| **p != "windows-x86_64").map(|p| format!("{}  {}\n", sha(format!("old {p}").as_bytes()), name(p))).collect::<String>();
    let mut files: Vec<(String, Vec<u8>)> = ALL.iter().filter(|p| **p != "windows-x86_64").map(|p| (name(p), format!("old {p}").into_bytes())).collect();
    // A file the checksum file doesn't list isn't trusted to be the key's, so it is replaced.
    files.push((name("windows-x86_64"), b"stray".to_vec()));
    files.push((format!("endeavor-{KEY}.sha256"), earlier.into_bytes()));
    let earlier: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect();
    let run = Run::new("full", &ALL, &earlier);
    let out = run.publish();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let kept = |p: &str| p != "windows-x86_64";
    let want: Vec<(String, String)> = ALL.iter().map(|p| (sha(if kept(p) { format!("old {p}") } else { format!("new {p}") }.as_bytes()), name(p))).collect();
    assert_eq!(run.sums(), want);
    for p in ALL {
        assert_eq!(std::fs::read(run.dir.join("release").join(name(p))).unwrap(), if kept(p) { format!("old {p}") } else { format!("new {p}") }.into_bytes(), "{p}");
    }
    // The step's output `sums` is the checksum file, for the pin job.
    let output = std::fs::read_to_string(run.dir.join("output")).unwrap();
    let file = std::fs::read_to_string(run.dir.join(format!("release/endeavor-{KEY}.sha256"))).unwrap();
    assert_eq!(output, format!("sums<<ENDEAVOR_SUMS\n{file}ENDEAVOR_SUMS\n"));
}

#[test]
fn a_first_run_of_a_key_publishes_what_it_built_and_a_missing_linux_build_publishes_nothing() {
    let run = Run::new("first", &["linux-aarch64", "linux-x86_64", "darwin-aarch64"], &[]);
    let out = run.publish();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let want: Vec<(String, String)> = ["darwin-aarch64", "linux-aarch64", "linux-x86_64"].iter().map(|p| (sha(format!("new {p}").as_bytes()), name(p))).collect();
    assert_eq!(run.sums(), want);
    assert!(!std::fs::read_to_string(run.dir.join("gh.log")).unwrap().contains("gh release download"), "no earlier checksum file to read");

    let run = Run::new("nolinux", &["linux-x86_64", "darwin-aarch64"], &[]);
    let out = run.publish();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("The linux-aarch64 build is missing"));
    assert_eq!(std::fs::read_dir(run.dir.join("release")).unwrap().count(), 0, "nothing was uploaded");
}

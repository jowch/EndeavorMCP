//! Endeavor's own Julia (`--julia own`) for real: downloaded into the cache folder,
//! and added to juliaup. Each test runs with a HOME of its own in the target folder.
//! They download Julia (about 300 MB each), so they're ignored by default:
//!
//!     ENDEAVOR_TEST_JULIAUP=/path/to/juliaup cargo test -p endeavor-mcp --test own_julia -- --ignored --nocapture --test-threads 1
//!
//! The juliaup tests copy the folder of `ENDEAVOR_TEST_JULIAUP` (juliaup and its
//! `julialauncher`, as juliaup's release tarball has them) into that HOME's
//! `.juliaup/bin`, where juliaup's installer puts them, and add the `release`
//! channel as the installer does, so 1.12.6 is not juliaup's default.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use endeavor_mcp::julia::{self, OwnFrom, Source};

const VERSION: &str = julia::JULIA_VERSION;

/// An empty HOME of the test's own, set for this process.
fn home(name: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("own-julia").join(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: each test binary runs these one at a time (--test-threads 1), and nothing else reads the environment meanwhile.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("SHELL", "/bin/sh");
    }
    home
}

/// A HOME with juliaup in it, set up as its installer leaves it: the `release` channel is the default.
fn home_with_juliaup(name: &str) -> (PathBuf, PathBuf) {
    let given = PathBuf::from(std::env::var("ENDEAVOR_TEST_JULIAUP").expect("set ENDEAVOR_TEST_JULIAUP to a juliaup program"));
    let home = home(name);
    let bin = home.join(".juliaup").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for entry in std::fs::read_dir(given.parent().unwrap()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), bin.join(entry.file_name())).unwrap();
    }
    std::fs::write(home.join(".juliaup").join("juliaupself.json"), r#"{"JuliaupChannel":"release"}"#).unwrap();
    let juliaup = bin.join("juliaup");
    assert!(Command::new(&juliaup).args(["add", "release"]).status().unwrap().success());
    (home, juliaup)
}

fn version_of(julia: &Path) -> String {
    let output = Command::new(julia).arg("--version").output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn channels(juliaup: &Path) -> String {
    let output = Command::new(juliaup).arg("status").output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore = "downloads Julia"]
fn downloaded_when_there_is_no_juliaup() {
    let home = home("download");
    assert_eq!(julia::own_installed(), None);
    let missing = julia::find(&Source::Own, false, &mut |_| {}).unwrap_err();
    let julia::Failure::Missing(item) = missing else { panic!("{missing:?}") };
    assert_eq!((item.name.as_str(), item.place.clone()), (format!("Julia {VERSION}").as_str(), Some(home.join(".cache/endeavor").join(format!("julia-{VERSION}")).display().to_string())));

    let mut said = Vec::new();
    let own = julia::install_own(&mut |line| said.push(line)).unwrap();
    assert_eq!(own.from, OwnFrom::Download);
    assert!(own.julia.starts_with(&home), "{}", own.julia.display());
    assert_eq!(version_of(&own.julia), format!("julia version {VERSION}"));
    assert!(said.iter().any(|line| line.starts_with(&format!("Downloading Julia {VERSION}…"))), "{said:?}");
    assert_eq!(julia::own_installed(), Some(own.clone()));
    assert_eq!(julia::find(&Source::Own, false, &mut |_| {}).unwrap(), (own.julia.display().to_string(), VERSION.to_owned()));

    julia::remove_own().unwrap();
    assert_eq!(julia::own_installed(), None);
    assert!(!home.join(".cache/endeavor").join(format!("julia-{VERSION}")).exists());
}

#[test]
#[ignore = "downloads Julia with juliaup"]
fn added_to_juliaup_and_removed_from_it() {
    let (home, juliaup) = home_with_juliaup("juliaup");
    assert_eq!(julia::own_installed(), None);
    let missing = julia::find(&Source::Own, false, &mut |_| {}).unwrap_err();
    let julia::Failure::Missing(item) = missing else { panic!("{missing:?}") };
    assert_eq!(item.name, format!("Julia {VERSION}, added to juliaup"));

    let own = julia::install_own(&mut |line| eprintln!("{line}")).unwrap();
    assert_eq!(own.from, OwnFrom::Juliaup { added: true });
    assert!(own.julia.starts_with(home.join(".julia/juliaup")), "{}", own.julia.display());
    assert_eq!(version_of(&own.julia), format!("julia version {VERSION}"));
    assert!(!home.join(".cache/endeavor").join(format!("julia-{VERSION}")).exists(), "no second Julia was downloaded");
    assert!(channels(&juliaup).contains(VERSION));
    assert_eq!(julia::find(&Source::Own, false, &mut |_| {}).unwrap(), (own.julia.display().to_string(), VERSION.to_owned()));

    julia::remove_own().unwrap();
    assert_eq!(julia::own_installed(), None);
    assert!(!channels(&juliaup).contains(VERSION), "{}", channels(&juliaup));
}

#[test]
#[ignore = "downloads Julia with juliaup"]
fn a_channel_the_person_added_stays_theirs() {
    let (_home, juliaup) = home_with_juliaup("theirs");
    assert!(Command::new(&juliaup).args(["add", VERSION]).status().unwrap().success());
    let own = julia::own_installed().unwrap();
    assert_eq!(own.from, OwnFrom::Juliaup { added: false });
    julia::remove_own().unwrap();
    assert_eq!(julia::own_installed(), Some(own), "removing Endeavor's Julia left the person's channel");
    assert!(channels(&juliaup).contains(VERSION));
}

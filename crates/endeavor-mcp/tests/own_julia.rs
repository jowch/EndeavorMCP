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
    let (home, juliaup) = home_with_bare_juliaup(name);
    assert!(Command::new(&juliaup).args(["add", "release"]).status().unwrap().success());
    (home, juliaup)
}

/// A HOME with juliaup in it and no channels yet, as the Microsoft Store leaves it on Windows. Its
/// `juliaup` is a wrapper that fails as asked: listing the channels as many more times as the file
/// `fail-list` beside it says, and adding a channel while there is a file `fail-add`.
fn home_with_bare_juliaup(name: &str) -> (PathBuf, PathBuf) {
    let given = PathBuf::from(std::env::var("ENDEAVOR_TEST_JULIAUP").expect("set ENDEAVOR_TEST_JULIAUP to a juliaup program"));
    let home = home(name);
    let bin = home.join(".juliaup").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for entry in std::fs::read_dir(given.parent().unwrap()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), bin.join(entry.file_name())).unwrap();
    }
    std::fs::rename(bin.join("juliaup"), bin.join("juliaup.real")).unwrap();
    let juliaup = bin.join("juliaup");
    let wrapper = r#"#!/bin/sh
dir=$(dirname "$0")
if [ "$1" = api ] && [ -s "$dir/fail-list" ]; then
  n=$(cat "$dir/fail-list"); echo $((n - 1)) > "$dir/fail-list"
  [ "$n" -gt 0 ] && { echo "Error: the configuration file is locked" >&2; exit 1; }
fi
[ "$1" = add ] && [ -e "$dir/fail-add" ] && { echo "Error: the configuration file is locked" >&2; exit 1; }
exec "$dir/juliaup.real" "$@"
"#;
    std::fs::write(&juliaup, wrapper).unwrap();
    std::fs::set_permissions(&juliaup, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(home.join(".juliaup").join("juliaupself.json"), r#"{"JuliaupChannel":"release"}"#).unwrap();
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
    assert_eq!(own.from, OwnFrom::Juliaup { added: true, default: false });
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
    assert_eq!(own.from, OwnFrom::Juliaup { added: false, default: false });
    julia::remove_own().unwrap();
    assert_eq!(julia::own_installed(), Some(own), "removing Endeavor's Julia left the person's channel");
    assert!(channels(&juliaup).contains(VERSION));
}

#[test]
#[ignore = "downloads Julia with juliaup"]
fn a_channel_the_person_added_isnt_taken_for_endeavors_when_juliaup_cant_list_its_channels() {
    let (_home, juliaup) = home_with_juliaup("theirs-unlisted");
    assert!(Command::new(&juliaup).args(["add", VERSION]).status().unwrap().success());
    // The look for an installed Julia and the look before adding both fail; juliaup counts the add as done.
    std::fs::write(juliaup.with_file_name("fail-list"), "2").unwrap();
    let own = julia::install_own(&mut |_| {}).unwrap();
    assert_eq!(own.from, OwnFrom::Juliaup { added: false, default: false });
    julia::remove_own().unwrap();
    assert!(channels(&juliaup).contains(VERSION), "the person's channel stays");
}

#[test]
#[ignore = "downloads Julia with juliaup"]
fn on_a_juliaup_with_no_channels_endeavors_julia_is_the_default_and_remove_says_so() {
    let (_home, juliaup) = home_with_bare_juliaup("bare");
    let own = julia::install_own(&mut |_| {}).unwrap();
    assert_eq!(own.from, OwnFrom::Juliaup { added: true, default: true });
    let why = julia::remove_own().unwrap_err();
    assert!(why.starts_with(&format!("juliaup uses Julia {VERSION} as its default, so it stays.")), "{why}");
    assert_eq!(julia::own_installed(), Some(own));
    assert!(channels(&juliaup).contains(VERSION));
}

#[test]
#[ignore = "downloads Julia"]
fn when_juliaup_cant_add_the_julia_it_is_downloaded() {
    let (home, juliaup) = home_with_juliaup("add-fails");
    std::fs::write(juliaup.with_file_name("fail-add"), "").unwrap();
    let mut said = Vec::new();
    let own = julia::install_own(&mut |line| said.push(line)).unwrap();
    assert_eq!(own.from, OwnFrom::Download);
    assert!(own.julia.starts_with(home.join(".cache/endeavor")), "{}", own.julia.display());
    let why = said.iter().find(|line| line.starts_with("juliaup couldn't install")).unwrap_or_else(|| panic!("{said:?}"));
    assert!(why.contains("the configuration file is locked") && why.ends_with(&format!("Downloading Endeavor's own Julia {VERSION} instead.")) && !why.contains("internet"), "{why}");
}

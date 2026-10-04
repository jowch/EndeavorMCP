//! `endeavor --version`.

use std::process::Command;

#[test]
fn version_names_the_package_and_the_build() {
    for flag in ["--version", "-V", "version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}");
        let line = String::from_utf8(out.stdout).unwrap();
        let version = env!("CARGO_PKG_VERSION");
        let build = line.trim().strip_prefix(&format!("endeavor {version} (build {version}-")).and_then(|rest| rest.strip_suffix(')')).unwrap_or_else(|| panic!("{flag}: {line}"));
        assert!(build.len() == 16 && build.chars().all(|c| c.is_ascii_hexdigit()), "{flag}: {line}");
    }
}

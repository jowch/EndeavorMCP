//! `endeavor --version`.

use std::process::Command;

#[test]
fn version_names_the_package_and_the_build() {
    for flag in ["--version", "-V", "version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_endeavor")).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}");
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        let line = lines.next().unwrap().to_owned();
        // A release build, made with ENDEAVOR_RELEASE_KEY, adds its key as a second line.
        let key = option_env!("ENDEAVOR_RELEASE_KEY").filter(|k| !k.is_empty());
        assert_eq!(lines.next().map(str::to_owned), key.map(|k| format!("release {k}")), "{flag}: {text}");
        assert_eq!(lines.next(), None, "{flag}: {text}");
        let version = env!("CARGO_PKG_VERSION");
        let build = line.trim().strip_prefix(&format!("endeavor {version} (build {version}-")).and_then(|rest| rest.strip_suffix(')')).unwrap_or_else(|| panic!("{flag}: {line}"));
        assert!(build.len() == 16 && build.chars().all(|c| c.is_ascii_hexdigit()), "{flag}: {line}");
    }
}

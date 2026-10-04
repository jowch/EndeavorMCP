//! `endeavor --version`.

/// This binary's version line: the package version, and the build
/// (`embedded::BUILD_VERSION`), which tells apart builds of one version.
pub(crate) fn version_line() -> String {
    format!("endeavor {} (build {})", env!("CARGO_PKG_VERSION"), crate::embedded::BUILD_VERSION)
}

pub(crate) fn print_version() -> ! {
    println!("{}", version_line());
    std::process::exit(0)
}

//! Embeds `runtime/` (the Julia side of the runtime) in the binary, so
//! `endeavor-remote serve` and `mcp` work from the one file (see `standalone`).
//! Its version names the folder it's unpacked to: the package version and a
//! hash of the files, as the app names a server install.

#[path = "../wire/src/tree.rs"]
mod tree;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let runtime = manifest.join("../../runtime").canonicalize().expect("runtime/ next to crates/");
    println!("cargo:rerun-if-changed={}", runtime.display());
    let files = tree::files(&runtime, "runtime").unwrap();
    let mut hash = tree::Fnv::default();
    hash.add_files(files.iter().map(|(path, contents, _)| (path.as_str(), contents.as_slice())));
    let version = format!("{}-{:016x}", std::env::var("CARGO_PKG_VERSION").unwrap(), hash.0);
    let mut out = format!("pub const VERSION: &str = {version:?};\npub static FILES: &[(&str, &[u8])] = &[\n");
    for (path, _, _) in &files {
        let source = runtime.join(path.strip_prefix("runtime/").unwrap());
        out.push_str(&format!("    ({path:?}, include_bytes!({:?})),\n", source.display().to_string()));
    }
    out.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("runtime.rs"), out).unwrap();
}

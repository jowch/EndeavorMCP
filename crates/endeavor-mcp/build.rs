//! Embeds `runtime/` (the Julia side of the runtime) and `plugin/` (the Pluto
//! skills as a Claude Code plugin) in the binary, so `endeavor serve`
//! and `mcp` work from the one file (see `standalone`) and Endeavor takes both
//! from this crate (`embedded`). Each one's version names the folder it's
//! unpacked to: the package version and a hash of the files.

#[path = "../wire/src/tree.rs"]
mod tree;

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut out = String::new();
    for (name, folder) in [("RUNTIME", "runtime"), ("PLUGIN", "plugin")] {
        let dir = manifest.join("../..").join(folder).canonicalize().unwrap_or_else(|e| panic!("{folder}/ next to crates/: {e}"));
        println!("cargo:rerun-if-changed={}", dir.display());
        let files = tree::files(&dir, folder).unwrap();
        let mut hash = tree::Fnv::default();
        hash.add_files(files.iter().map(|(path, contents, _)| (path.as_str(), contents.as_slice())));
        let version = format!("{}-{:016x}", std::env::var("CARGO_PKG_VERSION").unwrap(), hash.0);
        out.push_str(&format!("pub const {name}_VERSION: &str = {version:?};\npub static {name}_FILES: &[(&str, &[u8])] = &[\n"));
        for (path, _, _) in &files {
            let source = dir.join(path.strip_prefix(&format!("{folder}/")).unwrap());
            out.push_str(&format!("    ({path:?}, include_bytes!({:?})),\n", source.display().to_string()));
        }
        out.push_str("];\n");
    }
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded.rs"), out).unwrap();
}

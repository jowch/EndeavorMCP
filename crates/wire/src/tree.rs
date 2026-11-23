use std::path::Path;

pub fn files(dir: &Path, prefix: &str) -> Result<Vec<(String, Vec<u8>, bool)>, String> {
    let mut out = Vec::new();
    walk(dir, prefix, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>, bool)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        let meta = std::fs::metadata(entry.path()).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            walk(&entry.path(), &name, out)?;
        } else {
            let contents = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            let executable = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
            #[cfg(not(unix))]
            let executable = false;
            out.push((name, contents, executable));
        }
    }
    Ok(())
}

pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    pub fn add(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().chain(&[0xff]) {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn add_files<'a>(&mut self, files: impl IntoIterator<Item = (&'a str, &'a [u8])>) {
        for (path, contents) in files {
            self.add(path.as_bytes());
            self.add(contents);
        }
    }
}

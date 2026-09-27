//! Pluto notebook files on disk, read without Pluto: finding them in a folder,
//! and the first cells of one for the new-session screen's static preview. The
//! app reads This Mac's files with these; the helper a server's.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const HEADER: &str = "### A Pluto.jl notebook ###";
const CELL: &str = "# ╔═╡ ";
const ORDER: &str = "# ╔═╡ Cell order:";
/// Pluto's package cells, which hold the notebook's Project/Manifest.toml.
const PACKAGE_CELLS: [&str; 2] = ["PLUTO_PROJECT_TOML_CONTENTS", "PLUTO_MANIFEST_TOML_CONTENTS"];

/// A notebook found in a folder: its path, and when it last changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub path: PathBuf,
    pub modified: SystemTime,
}

const MAX_DEPTH: usize = 3;
const MAX_VISITED: usize = 2000;
const MAX_FOUND: usize = 200;
const SKIPPED: [&str; 5] = ["node_modules", ".git", "target", "venv", ".julia"];

/// Pluto notebooks in `folder` and its subfolders (3 levels, skipping hidden and
/// heavy folders, capped), newest first. Reads each `.jl` file's first line.
pub fn scan(folder: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    let mut visited = 0;
    let mut stack = vec![(folder.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_VISITED || found.len() >= MAX_FOUND {
                return sorted(found);
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() {
                if depth + 1 < MAX_DEPTH && !SKIPPED.contains(&name.as_ref()) {
                    stack.push((path, depth + 1));
                }
            } else if kind.is_file() && name.ends_with(".jl") && is_notebook(&path) {
                let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                found.push(Found { path, modified });
            }
        }
    }
    sorted(found)
}

fn sorted(mut found: Vec<Found>) -> Vec<Found> {
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    found
}

fn is_notebook(path: &Path) -> bool {
    use std::io::Read;
    let mut first = [0u8; HEADER.len()];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut first)).is_ok() && first == HEADER.as_bytes()
}

/// The start of a notebook for a static preview.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// Code of the first cells in notebook order, each cut to `LINES` lines.
    pub cells: Vec<Cell>,
    /// Cells in the notebook, not counting Pluto's package cells.
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub code: String,
    /// Lines were cut off the end.
    pub clipped: bool,
}

const CELLS: usize = 8;
const LINES: usize = 12;
/// Larger files are only their start: Pluto keeps the cell order at the end.
const MAX_READ: u64 = 16 << 20;

/// The preview of the notebook file at `path`.
pub fn read_preview(path: &Path) -> Result<Preview, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_READ).read_to_end(&mut bytes))
        .map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    Ok(preview(&String::from_utf8_lossy(&bytes)))
}

/// The first cells of a Pluto notebook file's text, in the notebook's order.
pub fn preview(text: &str) -> Preview {
    let mut bodies: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut order: Vec<&str> = Vec::new();
    let mut in_order = false;
    for line in text.lines() {
        if line.starts_with(ORDER) {
            in_order = true;
        } else if in_order {
            if let Some(id) = line.strip_prefix("# ╠═").or_else(|| line.strip_prefix("# ╟─")) {
                order.push(id.trim());
            }
        } else if let Some(id) = line.strip_prefix(CELL) {
            bodies.push((id.trim(), Vec::new()));
        } else if let Some((_, body)) = bodies.last_mut()
            // Cell metadata, e.g. `# ╠═╡ disabled = true`.
            && !line.starts_with("# ╠═╡")
        {
            body.push(line);
        }
    }
    // A file without a Cell order list keeps the file's order.
    if order.is_empty() {
        order = bodies.iter().map(|(id, _)| *id).collect();
    }
    let codes: Vec<&[&str]> = order
        .iter()
        .filter_map(|id| bodies.iter().find(|(b, _)| b == id))
        .map(|(_, body)| trim_blank(body))
        .filter(|body| !body.first().is_some_and(|l| PACKAGE_CELLS.iter().any(|p| l.starts_with(p))))
        .collect();
    Preview {
        total: codes.len(),
        cells: codes
            .iter()
            .take(CELLS)
            .map(|body| Cell { code: body[..body.len().min(LINES)].join("\n"), clipped: body.len() > LINES })
            .collect(),
    }
}

fn trim_blank<'a, 'b>(lines: &'b [&'a str]) -> &'b [&'a str] {
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(start, |i| i + 1);
    &lines[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"### A Pluto.jl notebook ###
# v0.19.40

using Markdown
using InteractiveUtils

# ╔═╡ 3c9e0c0a-0000-4000-8000-000000000003
model(t, p) = p[1] .* exp.(-p[2] .* t)

# ╔═╡ 1a2b3c4d-0000-4000-8000-000000000001
using CSV, DataFrames

# ╔═╡ 2b3c4d5e-0000-4000-8000-000000000002
# ╠═╡ show_logs = false
md"""
# Decay fits
"""

# ╔═╡ 4d5e6f70-0000-4000-8000-000000000004
begin
	a = 1
	b = 2
	c = 3
	d = 4
	e = 5
	f = 6
	g = 7
	h = 8
	i = 9
	j = 10
	k = 11
end

# ╔═╡ 00000000-0000-0000-0000-000000000001
PLUTO_PROJECT_TOML_CONTENTS = """
[deps]
"""

# ╔═╡ 00000000-0000-0000-0000-000000000002
PLUTO_MANIFEST_TOML_CONTENTS = """
julia_version = "1.12.6"
"""

# ╔═╡ Cell order:
# ╟─2b3c4d5e-0000-4000-8000-000000000002
# ╠═1a2b3c4d-0000-4000-8000-000000000001
# ╠═3c9e0c0a-0000-4000-8000-000000000003
# ╠═4d5e6f70-0000-4000-8000-000000000004
# ╟─00000000-0000-0000-0000-000000000001
# ╟─00000000-0000-0000-0000-000000000002
"#;

    #[test]
    fn cells_follow_the_cell_order_without_package_cells() {
        let p = preview(SAMPLE);
        assert_eq!(p.total, 4);
        let codes: Vec<&str> = p.cells.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(
            codes[..3],
            ["md\"\"\"\n# Decay fits\n\"\"\"", "using CSV, DataFrames", "model(t, p) = p[1] .* exp.(-p[2] .* t)"]
        );
    }

    #[test]
    fn long_cells_are_clipped_to_twelve_lines() {
        let last = &preview(SAMPLE).cells[3];
        assert!(last.clipped);
        assert_eq!(last.code.lines().count(), 12);
        assert_eq!(last.code.lines().last(), Some("\tk = 11"));
    }

    #[test]
    fn at_most_eight_cells() {
        let mut text = String::from("### A Pluto.jl notebook ###\n");
        for i in 0..10 {
            text += &format!("\n# ╔═╡ {i:08}-0000-4000-8000-000000000000\nx{i} = {i}\n");
        }
        let p = preview(&text);
        assert_eq!((p.total, p.cells.len()), (10, 8));
        assert_eq!(p.cells[7].code, "x7 = 7");
    }

    #[test]
    fn scan_finds_notebooks_by_their_first_line() {
        let dir = std::env::temp_dir().join(format!("endeavor-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["sub/deeper", ".hidden", "node_modules"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let notebook = "### A Pluto.jl notebook ###\n";
        std::fs::write(dir.join("a.jl"), notebook).unwrap();
        std::fs::write(dir.join("plain.jl"), "x = 1\n").unwrap();
        std::fs::write(dir.join("sub/b.jl"), notebook).unwrap();
        std::fs::write(dir.join("sub/deeper/c.jl"), notebook).unwrap();
        std::fs::write(dir.join(".hidden/d.jl"), notebook).unwrap();
        std::fs::write(dir.join("node_modules/e.jl"), notebook).unwrap();
        let mut found: Vec<_> = scan(&dir).into_iter().map(|f| f.path.strip_prefix(&dir).unwrap().to_path_buf()).collect();
        found.sort();
        assert_eq!(found, [PathBuf::from("a.jl"), PathBuf::from("sub/b.jl"), PathBuf::from("sub/deeper/c.jl")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! Notebook files on disk, read without their engine: finding them in a
//! folder, and the first cells of one for the new-session screen's static
//! preview. The app reads This Mac's files with these; the helper a server's.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;

const CELL: &str = "# ╔═╡ ";
const ORDER: &str = "# ╔═╡ Cell order:";
/// Pluto's package cells, which hold the notebook's Project/Manifest.toml.
const PACKAGE_CELLS: [&str; 2] = ["PLUTO_PROJECT_TOML_CONTENTS", "PLUTO_MANIFEST_TOML_CONTENTS"];

/// A notebook found in a folder: its path, when it last changed, and whose
/// notebook it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Found {
    pub path: PathBuf,
    pub modified: SystemTime,
    /// Missing from an older helper, which finds only Pluto's.
    #[serde(default = "pluto")]
    pub backend: Backend,
}

pub(crate) fn pluto() -> Backend {
    Backend::Pluto
}

const MAX_DEPTH: usize = 3;
const MAX_VISITED: usize = 2000;
const MAX_FOUND: usize = 200;
const SKIPPED: [&str; 5] = ["node_modules", ".git", "target", "venv", ".julia"];

/// Notebooks of `backends` in `folder` and its subfolders (3 levels, skipping
/// hidden and heavy folders, capped), newest first. See [`Backend::of_file`].
pub fn scan(folder: &Path, backends: &[Backend]) -> Vec<Found> {
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
            } else if kind.is_file()
                && let Some(backend) = Backend::of_file(&path).filter(|b| backends.contains(b))
            {
                let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                found.push(Found { path, modified, backend });
            }
        }
    }
    sorted(found)
}

fn sorted(mut found: Vec<Found>) -> Vec<Found> {
    found.sort_by(|a, b| b.modified.cmp(&a.modified));
    found
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
/// Larger files are only their start: Pluto and Ember keep the cell order at the end.
const MAX_READ: u64 = 16 << 20;

/// The preview of the notebook file at `path`.
pub fn read_preview(path: &Path) -> Result<Preview, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_READ).read_to_end(&mut bytes))
        .map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(match Backend::of_file(path) {
        Some(Backend::Ember) => ember_preview(&text),
        _ => preview(&text),
    })
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

const EMBER_CELL: &str = "# %%";
const EMBER_BLOCK_END: &str = "# ///";
const EMBER_ORDER: &str = "cell order";

/// The first cells of an Ember notebook file's text, in the notebook's order,
/// read as Ember's `parse_notebook` reads it: a cell runs from its `# %% id=…`
/// line to the next cell or `# /// <name>` block; the `cell order` block gives
/// the display order and which cells are `disabled` or `commented`, whose lines
/// the file prefixes with `## `.
pub fn ember_preview(text: &str) -> Preview {
    let mut bodies: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut order: Vec<(&str, bool)> = Vec::new();
    let mut block: Option<&str> = None;
    let mut in_cell = false;
    for line in text.lines() {
        if let Some(marker) = line.strip_prefix(EMBER_CELL) {
            let id = marker.split_whitespace().find_map(|word| word.strip_prefix("id=")).unwrap_or("");
            bodies.push((id, Vec::new()));
            (block, in_cell) = (None, true);
        } else if let Some(name) = line.strip_prefix("# /// ").filter(|name| !name.trim().is_empty()) {
            (block, in_cell) = (Some(name.trim()), false);
        } else if line == EMBER_BLOCK_END {
            block = None;
        } else if block == Some(EMBER_ORDER) {
            let mut words = line.trim_start_matches('#').split_whitespace();
            if let Some(id) = words.next() {
                order.push((id, words.any(|w| w == "disabled" || w == "commented")));
            }
        } else if in_cell && let Some((_, body)) = bodies.last_mut() {
            body.push(line);
        }
    }
    // As Ember does: ids the file has no cell for, and repeats, are skipped; a
    // cell the block doesn't list goes after the nearest cell before it in the
    // file that is placed, else first.
    let mut placed: Vec<(&str, bool)> = Vec::new();
    for (id, commented) in order {
        if bodies.iter().any(|(b, _)| *b == id) && !placed.iter().any(|(p, _)| *p == id) {
            placed.push((id, commented));
        }
    }
    for (at, (id, _)) in bodies.iter().enumerate() {
        if placed.iter().any(|(p, _)| p == id) {
            continue;
        }
        let after = bodies[..at].iter().rev().find_map(|(before, _)| placed.iter().position(|(p, _)| p == before));
        placed.insert(after.map_or(0, |i| i + 1), (id, false));
    }
    let codes: Vec<Vec<&str>> = placed
        .iter()
        .filter_map(|(id, commented)| {
            let (_, body) = bodies.iter().find(|(b, _)| b == id)?;
            let body: Vec<&str> = if *commented { body.iter().map(|l| l.strip_prefix("## ").or_else(|| l.strip_prefix("##")).unwrap_or(l)).collect() } else { body.clone() };
            Some(trim_blank(&body).to_vec())
        })
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
        std::fs::write(dir.join("sub/f.R"), "### An Ember notebook ###\n").unwrap();
        std::fs::write(dir.join("script.R"), "x <- 1\n").unwrap();
        let found = |backends: &[Backend]| {
            let mut found: Vec<_> = scan(&dir, backends).into_iter().map(|f| (f.path.strip_prefix(&dir).unwrap().to_path_buf(), f.backend)).collect();
            found.sort_by(|a, b| a.0.cmp(&b.0));
            found
        };
        let pluto = [(PathBuf::from("a.jl"), Backend::Pluto), (PathBuf::from("sub/b.jl"), Backend::Pluto), (PathBuf::from("sub/deeper/c.jl"), Backend::Pluto)];
        assert_eq!(found(&[Backend::Pluto]), pluto);
        assert_eq!(found(&[Backend::Ember]), [(PathBuf::from("sub/f.R"), Backend::Ember)]);
        assert_eq!(found(&Backend::ALL).len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn found_from_an_older_helper_is_pluto() {
        let found: Found = serde_json::from_str(r#"{"path":"/n/a.jl","modified":{"secs_since_epoch":0,"nanos_since_epoch":0}}"#).unwrap();
        assert_eq!(found.backend, Backend::Pluto);
    }

    /// As Ember writes a file: header block, cells in run order, then the
    /// footer. The display order differs, one cell is folded and one disabled.
    const EMBER_SAMPLE: &str = "### An Ember notebook ###
# /// environment
# ember_version = \"0.0.0.9000\"
# r_version = \"4.5.1\"
# snapshot = \"2026-09-01\"
# ///

# %% id=6f1c9a2e-0000-4000-8000-000000000001
library(dplyr)

# %% id=0b7d0000-0000-4000-8000-000000000002
#' ## Growth curves
#' Measured every 30 minutes.

# %% id=a41e0000-0000-4000-8000-000000000003
curves <- read.csv(\"growth.csv\")

# %% id=d7e00000-0000-4000-8000-000000000004
## curves + 1
## nrow(curves)

# /// cell order
# 0b7d0000-0000-4000-8000-000000000002
# 6f1c9a2e-0000-4000-8000-000000000001 folded
# a41e0000-0000-4000-8000-000000000003
# d7e00000-0000-4000-8000-000000000004 disabled
# ///
# /// lock
# dplyr 1.1.4 CRAN
# ///
";

    #[test]
    fn ember_cells_follow_the_cell_order() {
        let p = ember_preview(EMBER_SAMPLE);
        assert_eq!(p.total, 4);
        let codes: Vec<&str> = p.cells.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(
            codes,
            ["#' ## Growth curves\n#' Measured every 30 minutes.", "library(dplyr)", "curves <- read.csv(\"growth.csv\")", "curves + 1\nnrow(curves)"]
        );
    }

    #[test]
    fn ember_without_a_cell_order_keeps_the_file_order() {
        let text = "### An Ember notebook ###\n# /// environment\n# ///\n\n# %% id=a\nx <- 1\n\n# %% id=b\ny <- x\n";
        let codes: Vec<String> = ember_preview(text).cells.into_iter().map(|c| c.code).collect();
        assert_eq!(codes, ["x <- 1", "y <- x"]);
    }

    #[test]
    fn ember_places_cells_the_order_leaves_out_as_ember_does() {
        // c and a aren't listed: a has no listed cell before it, so it goes first; c goes after b.
        let text = "### An Ember notebook ###\n# %% id=a\nA\n# %% id=b\nB\n# %% id=c\nC\n# %% id=d\nD\n# /// cell order\n# d\n# b\n# x\n# b\n# ///\n";
        let codes: Vec<String> = ember_preview(text).cells.into_iter().map(|c| c.code).collect();
        assert_eq!(codes, ["A", "D", "B", "C"]);
    }

    #[test]
    fn ember_drops_a_disabled_cells_trailing_blank_lines() {
        let text = "### An Ember notebook ###\n# %% id=a\n## x <- 1\n##\n\n# /// cell order\n# a disabled\n# ///\n";
        assert_eq!(ember_preview(text).cells[0].code, "x <- 1");
    }

    #[test]
    fn read_preview_picks_the_format_from_the_file() {
        let dir = std::env::temp_dir().join(format!("endeavor-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.R"), EMBER_SAMPLE).unwrap();
        std::fs::write(dir.join("b.jl"), SAMPLE).unwrap();
        assert_eq!(read_preview(&dir.join("a.R")).unwrap().cells[1].code, "library(dplyr)");
        assert_eq!(read_preview(&dir.join("b.jl")).unwrap().cells[1].code, "using CSV, DataFrames");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

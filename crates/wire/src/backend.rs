//! The notebook systems Endeavor works with, and what differs between them on
//! the app's side: Pluto (Julia), Ember (R) and marimo (Python).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Pluto,
    /// Ember, reactive notebooks for R (https://github.com/jowch/Ember). Its
    /// files are found and previewed; the runtime can't open them yet.
    Ember,
    /// marimo, reactive notebooks for Python (https://marimo.io). The runtime runs them; the app
    /// doesn't show them yet, so they aren't in `ALL`.
    Marimo,
}

const PLUTO_HEADER: &str = "### A Pluto.jl notebook ###";
const EMBER_HEADER: &str = "### An Ember notebook ###";

impl Backend {
    /// The kinds the app offers and lists.
    pub const ALL: [Backend; 2] = [Backend::Pluto, Backend::Ember];
    /// Every kind the runtime runs.
    pub const EVERY: [Backend; 3] = [Backend::Pluto, Backend::Ember, Backend::Marimo];

    /// Its name in `notebook://<name>/…` cell URIs.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Pluto => "pluto",
            Backend::Ember => "ember",
            Backend::Marimo => "marimo",
        }
    }

    /// The first line of its notebook files.
    fn header(self) -> &'static str {
        match self {
            Backend::Pluto => PLUTO_HEADER,
            Backend::Ember => EMBER_HEADER,
            // A marimo file has no first line of its own (`is_marimo`).
            Backend::Marimo => "",
        }
    }

    /// Where its pages are on the runtime's port: Pluto at the root, later
    /// engines under a prefix of their own.
    fn prefix(self) -> &'static str {
        match self {
            Backend::Pluto => "",
            Backend::Ember => "/ember",
            Backend::Marimo => "/marimo",
        }
    }

    /// The backend whose notebook the file at `path` is, from its name and first line.
    pub fn of_file(path: &Path) -> Option<Backend> {
        use std::io::Read;
        let backend = match path.extension()?.to_str()? {
            "jl" => Backend::Pluto,
            "R" | "r" => Backend::Ember,
            "py" => return is_marimo(path).then_some(Backend::Marimo),
            _ => return None,
        };
        let header = backend.header();
        let mut first = vec![0u8; header.len()];
        let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut first)).is_ok();
        (read && first == header.as_bytes()).then_some(backend)
    }

    /// The page for notebook `id`, from the runtime's root URL (`http://host:port/?token=…`).
    pub fn notebook_url(self, root: &str, id: &str) -> String {
        root.replacen("/?", &format!("{}/edit?id={id}&", self.prefix()), 1)
    }

    /// The notebook id in a notebook page's URL, not yet validated.
    pub fn notebook_id(self, url: &str) -> Option<&str> {
        let (address, query) = url.split_once('?')?;
        // The path after `scheme://host:port`, or the whole thing when the URL has no host.
        let path = address.split_once("://").map_or(address, |(_, rest)| rest.find('/').map_or("", |slash| &rest[slash..]));
        if path != format!("{}/edit", self.prefix()) {
            return None;
        }
        query.split('&').find_map(|kv| kv.strip_prefix("id="))
    }
}

/// Whether the `.py` file at `path` is a marimo notebook: near its start, after any PEP 723 header
/// and docstring, it imports marimo and makes `app = marimo.App(...)`, as marimo writes every one.
fn is_marimo(path: &Path) -> bool {
    use std::io::Read;
    let mut start = Vec::new();
    if std::fs::File::open(path).and_then(|f| f.take(MARIMO_READ).read_to_end(&mut start)).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&start);
    text.lines().any(|line| line.trim_end() == "import marimo") && text.lines().any(|line| line.starts_with("app = marimo.App("))
}

/// How much of a `.py` file `is_marimo` reads: a long PEP 723 header and docstring fit.
const MARIMO_READ: u64 = 16 << 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluto_notebook_urls() {
        let url = Backend::Pluto.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc");
        assert_eq!(url, "http://127.0.0.1:1234/edit?id=abc&token=t0k3n");
        assert_eq!(Backend::Pluto.notebook_id(&url), Some("abc"));
        assert_eq!(Backend::Pluto.notebook_id("http://127.0.0.1:1234/edit?id=abc"), Some("abc"), "once the token has gone from the URL");
        assert_eq!(Backend::Pluto.notebook_id("http://127.0.0.1:1234/?token=t0k3n"), None);
    }

    #[test]
    fn ember_pages_are_under_their_own_prefix() {
        let url = Backend::Ember.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc");
        assert_eq!(url, "http://127.0.0.1:1234/ember/edit?id=abc&token=t0k3n");
        assert_eq!(Backend::Ember.notebook_id(&url), Some("abc"));
        assert_eq!(Backend::Pluto.notebook_id(&url), None, "an Ember page isn't a Pluto one");
        assert_eq!(Backend::Ember.notebook_id("http://127.0.0.1:1234/edit?id=abc"), None);
    }

    #[test]
    fn files_by_extension_and_first_line() {
        let dir = std::env::temp_dir().join(format!("endeavor-backend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [
            ("a.jl", "### A Pluto.jl notebook ###\n", Some(Backend::Pluto)),
            ("b.R", "### An Ember notebook ###\n# /// environment\n", Some(Backend::Ember)),
            ("c.r", "### An Ember notebook ###\n", Some(Backend::Ember)),
            ("d.R", "x <- 1\n", None),
            ("e.jl", "### An Ember notebook ###\n", None),
            ("f.R", "### A Pluto.jl notebook ###\n", None),
            ("g.txt", "### An Ember notebook ###\n", None),
            ("h.py", "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n", Some(Backend::Marimo)),
            ("i.py", "# /// script\n# dependencies = [\"marimo\"]\n# ///\n\nimport marimo\n\napp = marimo.App(width=\"medium\")\n", Some(Backend::Marimo)),
            ("j.py", "import marimo\nprint(marimo.__version__)\n", None),
            ("k.py", "import numpy as np\n", None),
        ];
        for (name, text, _) in cases {
            std::fs::write(dir.join(name), text).unwrap();
        }
        for (name, _, want) in cases {
            assert_eq!(Backend::of_file(&dir.join(name)), want, "{name}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn marimo_pages_are_under_their_own_prefix() {
        let url = Backend::Marimo.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc");
        assert_eq!(url, "http://127.0.0.1:1234/marimo/edit?id=abc&token=t0k3n");
        assert_eq!(Backend::Marimo.notebook_id(&url), Some("abc"));
        assert_eq!(Backend::Ember.notebook_id(&url), None);
    }

    #[test]
    fn names_on_the_wire() {
        assert_eq!(serde_json::to_string(&Backend::Marimo).unwrap(), "\"marimo\"");
        assert_eq!(serde_json::to_string(&Backend::Ember).unwrap(), "\"ember\"");
        assert_eq!(serde_json::from_str::<Backend>("\"pluto\"").unwrap(), Backend::Pluto);
    }
}

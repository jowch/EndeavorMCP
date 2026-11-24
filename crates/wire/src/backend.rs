use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Pluto,
    Ember,
    Marimo,
}

const PLUTO_HEADER: &str = "### A Pluto.jl notebook ###";
const EMBER_HEADER: &str = "### An Ember notebook ###";

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Pluto, Backend::Ember];

    pub fn name(self) -> &'static str {
        match self {
            Backend::Pluto => "pluto",
            Backend::Ember => "ember",
            Backend::Marimo => "marimo",
        }
    }

    fn header(self) -> &'static str {
        match self {
            Backend::Pluto => PLUTO_HEADER,
            Backend::Ember => EMBER_HEADER,
            Backend::Marimo => "",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Backend::Pluto => "",
            Backend::Ember => "/ember",
            Backend::Marimo => "/marimo",
        }
    }

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

    pub fn notebook_url(self, root: &str, id: &str, path: &str) -> String {
        match self {
            Backend::Marimo => root.replacen("/?", &format!("{}/?file={}&", self.prefix(), query_value(path)), 1),
            _ => root.replacen("/?", &format!("{}/edit?id={id}&", self.prefix()), 1),
        }
    }

    pub fn notebook_id(self, url: &str) -> Option<&str> {
        let (address, query) = url.split_once('?')?;
        let path = address.split_once("://").map_or(address, |(_, rest)| rest.find('/').map_or("", |slash| &rest[slash..]));
        if self == Backend::Marimo || path != format!("{}/edit", self.prefix()) {
            return None;
        }
        query.split('&').find_map(|kv| kv.strip_prefix("id="))
    }

    pub fn notebook_path(self, url: &str) -> Option<String> {
        let (address, query) = url.split_once('?')?;
        let path = address.split_once("://").map_or(address, |(_, rest)| rest.find('/').map_or("", |slash| &rest[slash..]));
        if self != Backend::Marimo || path != format!("{}/", self.prefix()) {
            return None;
        }
        let mut files = query.split('&').filter_map(|kv| kv.strip_prefix("file="));
        match (files.next(), files.next()) {
            (Some(file), None) => query_decoded(file),
            _ => None,
        }
    }
}

fn query_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = text.get(i + 1..i + 3).filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub fn query_value(text: &str) -> String {
    text.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn is_marimo(path: &Path) -> bool {
    use std::io::Read;
    let mut start = Vec::new();
    if std::fs::File::open(path).and_then(|f| f.take(MARIMO_READ).read_to_end(&mut start)).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&start);
    text.lines().any(|line| line.trim_end() == "import marimo") && text.lines().any(|line| line.starts_with("app = marimo.App("))
}

const MARIMO_READ: u64 = 16 << 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluto_notebook_urls() {
        let url = Backend::Pluto.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc", "/w/a.jl");
        assert_eq!(url, "http://127.0.0.1:1234/edit?id=abc&token=t0k3n");
        assert_eq!(Backend::Pluto.notebook_id(&url), Some("abc"));
        assert_eq!(Backend::Pluto.notebook_id("http://127.0.0.1:1234/edit?id=abc"), Some("abc"), "once the token has gone from the URL");
        assert_eq!(Backend::Pluto.notebook_id("http://127.0.0.1:1234/?token=t0k3n"), None);
    }

    #[test]
    fn ember_pages_are_under_their_own_prefix() {
        let url = Backend::Ember.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc", "/w/a.R");
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
        let url = Backend::Marimo.notebook_url("http://127.0.0.1:1234/?token=t0k3n", "abc", "/w/a b&c.py");
        assert_eq!(url, "http://127.0.0.1:1234/marimo/?file=/w/a%20b%26c.py&token=t0k3n", "marimo's page names its notebook by path");
        assert_eq!(Backend::Marimo.notebook_id(&url), None);
        assert_eq!(Backend::Marimo.notebook_path(&url).as_deref(), Some("/w/a b&c.py"), "the path its URL was made from");
        assert_eq!(Backend::Marimo.notebook_path("http://127.0.0.1:1234/marimo/?file=/w/a+b.py").as_deref(), Some("/w/a b.py"));
        assert_eq!(Backend::Marimo.notebook_path("http://127.0.0.1:1234/marimo/?file=/a.py&file=/b.py"), None, "one file or none");
        assert_eq!(Backend::Marimo.notebook_path("http://127.0.0.1:1234/marimo/?file=%zz"), None);
        assert_eq!(Backend::Marimo.notebook_path("http://127.0.0.1:1234/marimo/?file=%+f"), None, "a sign isn't a hex digit");
        assert_eq!(Backend::Marimo.notebook_path("http://127.0.0.1:1234/marimo/api/x?file=/w/a.py"), None, "only the page");
        assert_eq!(Backend::Pluto.notebook_path(&url), None);
        assert_eq!(Backend::Marimo.notebook_id("http://127.0.0.1:1234/marimo/edit?id=abc"), None);
    }

    #[test]
    fn names_on_the_wire() {
        assert_eq!(serde_json::to_string(&Backend::Marimo).unwrap(), "\"marimo\"");
        assert_eq!(serde_json::to_string(&Backend::Ember).unwrap(), "\"ember\"");
        assert_eq!(serde_json::from_str::<Backend>("\"pluto\"").unwrap(), Backend::Pluto);
    }
}

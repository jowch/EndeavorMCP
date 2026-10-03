//! The notebook systems Endeavor works with, and what differs between them on
//! the app's side. Pluto is the only one so far.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Pluto,
}

const PLUTO_HEADER: &str = "### A Pluto.jl notebook ###";

impl Backend {
    /// Its name in `notebook://<name>/…` cell URIs.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Pluto => "pluto",
        }
    }

    /// The backend whose notebook the file at `path` is, from its name and first line.
    pub fn of_file(path: &Path) -> Option<Backend> {
        use std::io::Read;
        if path.extension().is_none_or(|e| e != "jl") {
            return None;
        }
        let mut first = [0u8; PLUTO_HEADER.len()];
        let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut first)).is_ok();
        (read && first == PLUTO_HEADER.as_bytes()).then_some(Backend::Pluto)
    }

    /// The page for notebook `id`, from the runtime's root URL (`http://host:port/?token=…`).
    pub fn notebook_url(self, root: &str, id: &str) -> String {
        match self {
            Backend::Pluto => root.replacen("/?", &format!("/edit?id={id}&"), 1),
        }
    }

    /// The notebook id in a notebook page's URL, not yet validated.
    pub fn notebook_id(self, url: &str) -> Option<&str> {
        match self {
            Backend::Pluto => {
                let (path, query) = url.split_once('?')?;
                if !path.ends_with("/edit") {
                    return None;
                }
                query.split('&').find_map(|kv| kv.strip_prefix("id="))
            }
        }
    }
}

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
}

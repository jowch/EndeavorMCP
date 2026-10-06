//! The XDG base folders: a variable counts only if it holds an absolute path.
//! A relative one would put endeavor's files under whichever folder the process
//! started in.

use std::path::PathBuf;

/// `name`'s value, if it is set to an absolute path, else `None`.
pub(crate) fn absolute_var(read: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    read(name).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::absolute_var;

    #[test]
    fn only_an_absolute_path_counts() {
        let read = |name: &str| match name {
            "ABS" => Some("/data".to_owned()),
            "REL" => Some("data/share".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(absolute_var(&read, "ABS"), Some("/data".into()));
        for name in ["REL", "EMPTY", "UNSET"] {
            assert_eq!(absolute_var(&read, name), None, "{name}");
        }
    }
}

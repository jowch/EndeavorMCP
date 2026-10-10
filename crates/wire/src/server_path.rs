//! Paths on a server. Servers are Linux or macOS, so a path there follows `/`
//! rules whatever this computer's are; on Windows a `PathBuf` would put `\`
//! into it on `join` and read `C:` as a drive. So the app and this library
//! carry a server's paths as plain strings, and take them apart with these.

/// `name` (a name or a relative path) inside `dir`. An absolute `name`
/// stands alone, as with `Path::join`.
pub fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') || dir.is_empty() {
        name.to_owned()
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `path` without trailing `/`s, but `/` itself kept.
fn trimmed(path: &str) -> &str {
    let t = path.trim_end_matches('/');
    if t.is_empty() && path.starts_with('/') { "/" } else { t }
}

/// The folder `path` is in: None for `/` and for an empty path; `""` for a
/// bare name, as with `Path::parent`.
pub fn parent(path: &str) -> Option<&str> {
    let path = trimmed(path);
    if path == "/" || path.is_empty() {
        return None;
    }
    Some(match path.rsplit_once('/') {
        Some((head, _)) => match head.trim_end_matches('/') {
            "" => "/",
            head => head,
        },
        None => "",
    })
}

/// `path`, then each folder it is in up to `/`, as with `Path::ancestors`.
pub fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(path), |p| parent(p)).filter(|p| !p.is_empty())
}

/// The last part of `path`: None for `/`, an empty path or one ending in `..`.
pub fn file_name(path: &str) -> Option<&str> {
    let path = trimmed(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    (!name.is_empty() && name != "..").then_some(name)
}

/// `path` relative to `base`, part by part: `Some("")` for `base` itself,
/// None when `path` isn't inside it (`/data2` isn't inside `/data`).
pub fn strip_prefix<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    let (path, base) = (trimmed(path), trimmed(base));
    if base == "/" {
        return path.strip_prefix('/').map(|rest| rest.trim_start_matches('/'));
    }
    match path.strip_prefix(base)? {
        "" => Some(""),
        rest => rest.strip_prefix('/').map(|rest| rest.trim_start_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_with_slashes_whatever_this_computer_uses() {
        assert_eq!(join("/home/ana", "decay-fits"), "/home/ana/decay-fits");
        assert_eq!(join("/home/ana/", "data/run 1.csv"), "/home/ana/data/run 1.csv");
        assert_eq!(join("/", "srv"), "/srv");
        assert_eq!(join("/home/ana", "/srv/data"), "/srv/data");
        assert_eq!(join("", "a.jl"), "a.jl");
        // A name Windows would read as a drive is only a name here.
        assert_eq!(join("/home/ana", "C:x"), "/home/ana/C:x");
    }

    #[test]
    fn parents_and_ancestors() {
        assert_eq!(parent("/home/ana/fits"), Some("/home/ana"));
        assert_eq!(parent("/home/ana/fits/"), Some("/home/ana"));
        assert_eq!(parent("/home"), Some("/"));
        assert_eq!(parent("/home//ana"), Some("/home"));
        assert_eq!(parent("/"), None);
        assert_eq!(parent(""), None);
        assert_eq!(parent("fits"), Some(""));
        assert_eq!(parent("sub/a.jl"), Some("sub"));
        assert_eq!(ancestors("/home/ana/fits").collect::<Vec<_>>(), ["/home/ana/fits", "/home/ana", "/home", "/"]);
        assert_eq!(ancestors("/").collect::<Vec<_>>(), ["/"]);
        assert_eq!(ancestors("sub/a.jl").collect::<Vec<_>>(), ["sub/a.jl", "sub"]);
    }

    #[test]
    fn names() {
        assert_eq!(file_name("/home/ana/fit.jl"), Some("fit.jl"));
        assert_eq!(file_name("/home/ana/"), Some("ana"));
        assert_eq!(file_name("fit.jl"), Some("fit.jl"));
        assert_eq!(file_name(r"/home/ana/a\b.jl"), Some(r"a\b.jl"), "a backslash is part of a name");
        assert_eq!(file_name("/"), None);
        assert_eq!(file_name(""), None);
        assert_eq!(file_name("/home/.."), None);
    }

    #[test]
    fn relative_paths_go_part_by_part() {
        assert_eq!(strip_prefix("/home/ana/fits/a.jl", "/home/ana"), Some("fits/a.jl"));
        assert_eq!(strip_prefix("/home/ana/fits/a.jl", "/home/ana/"), Some("fits/a.jl"));
        assert_eq!(strip_prefix("/home/ana", "/home/ana"), Some(""));
        assert_eq!(strip_prefix("/home/ana/", "/home/ana"), Some(""));
        assert_eq!(strip_prefix("/home/anabel", "/home/ana"), None);
        assert_eq!(strip_prefix("/srv/a.jl", "/home/ana"), None);
        assert_eq!(strip_prefix("/srv/a.jl", "/"), Some("srv/a.jl"));
        assert_eq!(strip_prefix("srv/a.jl", "/"), None);
    }
}

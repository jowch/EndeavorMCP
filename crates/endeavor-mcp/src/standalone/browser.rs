//! Opening a notebook in the user's browser. The link that lets a browser in carries the
//! runtime's token, so it goes from this process to the browser and never through the agent:
//! tool results only carry the link without it (`mcp::browser_link`), which works in a browser
//! that has been let in.

use std::process::{Command, Stdio};

/// Whether this process can show the user a browser: not one started over ssh, and on Linux and
/// the other Unixes only with a display.
pub(crate) fn can_open(var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    if cfg!(any(target_os = "macos", windows)) {
        return !set("SSH_CONNECTION") && !set("SSH_TTY");
    }
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// The program and arguments that open `url` in the default browser. The URL is one argument and
/// no shell reads it (on Windows, `start` in cmd would split it at `&`).
pub(crate) fn command(url: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        ("open", vec![url.to_owned()])
    } else if cfg!(windows) {
        ("rundll32.exe", vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()])
    } else {
        ("xdg-open", vec![url.to_owned()])
    }
}

/// Open `url` in the user's browser. Whether it was handed to the browser.
///
/// A debug build never opens a real browser, so that the tests don't: with
/// `ENDEAVOR_TEST_BROWSER` set to a file it adds each URL to that file as a line and counts it as
/// opened, and without it nothing is opened.
pub(crate) fn open(url: &str) -> bool {
    if cfg!(debug_assertions) {
        let Some(file) = std::env::var_os("ENDEAVOR_TEST_BROWSER") else { return false };
        use std::io::Write;
        return std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{url}")).is_ok();
    }
    if !can_open(|name| std::env::var(name).ok()) {
        return false;
    }
    let (program, args) = command(url);
    // stdout is the agent's MCP connection: the opener writes nothing there.
    match Command::new(program).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(e) => {
            eprintln!("endeavor: couldn't open the browser ({program}): {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(set: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| set.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn a_browser_opens_only_where_the_user_sees_it() {
        let over_ssh = can_open(vars(&[("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22"), ("DISPLAY", ":0")]));
        if cfg!(any(target_os = "macos", windows)) {
            assert!(can_open(vars(&[])));
            assert!(!over_ssh);
        } else {
            assert!(!can_open(vars(&[])), "no display");
            assert!(!can_open(vars(&[("DISPLAY", "")])));
            assert!(can_open(vars(&[("DISPLAY", ":0")])));
            assert!(can_open(vars(&[("WAYLAND_DISPLAY", "wayland-0")])));
            assert!(over_ssh, "ssh -X: the browser shows on the user's display");
        }
    }

    #[test]
    fn the_url_is_one_argument() {
        let url = "http://localhost:1/edit?id=a&token=b";
        let (_, args) = command(url);
        assert_eq!(args.last().map(String::as_str), Some(url));
    }
}

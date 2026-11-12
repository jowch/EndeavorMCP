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

/// Whether this process can show the user a browser, as `open` decides it: in a debug build, whether a
/// test stands one in (`ENDEAVOR_TEST_BROWSER`).
pub(crate) fn shows(var: impl Fn(&str) -> Option<String>) -> bool {
    if cfg!(debug_assertions) {
        return var("ENDEAVOR_TEST_BROWSER").is_some();
    }
    can_open(var)
}

/// What the agent is told with a `browser_url` that no browser was opened for: this computer
/// (`host`) shows no browser. Whether the user's browser reaches `localhost:{port}` as it is can't be
/// told from here: a virtual machine on their computer that shares its ports reaches it, and one
/// entered with `orb run` has the same ssh variables as a real ssh login, which doesn't. So the address
/// comes first, and the port forward only if it doesn't load. The agent can't forward it: the forward
/// starts on the user's computer. `open` lets their browser in (`endeavor open`), since the link a
/// browser needs carries the token, which the agent never gets.
pub(crate) fn note(host: &str, port: u16, open: &str) -> String {
    format!(
        "No browser could be opened on {host}, so the notebook did not open for the user. \
Give them `browser_url` to open in their browser, and don't say it is open. \
If the page asks to let the browser in, they run `{open}` in a terminal on {host} and open the link it prints. \
Only if the address doesn't load at all, their browser is on another computer: there they run `ssh -L {port}:localhost:{port} {host}` \
(with the name they use to ssh to {host}), keep it open, and open `browser_url` again. \
You can't forward the port for them: it starts on their computer."
    )
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

/// How long the opener is waited for: `open` and `xdg-open` exit soon and say when no browser took the
/// link. One still running then is taken as opening it.
const OPENER_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Open `url` in the user's browser. Whether it was handed to the browser: on macOS and Linux the
/// opener's exit says so; on Windows `rundll32` always exits 0, so there it is whether it started.
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
            if cfg!(windows) {
                std::thread::spawn(move || child.wait());
                return true;
            }
            took(child, program, OPENER_WAIT)
        }
        Err(e) => {
            eprintln!("endeavor: couldn't open the browser ({program}): {e}");
            false
        }
    }
}

/// Whether the opener `child` (`program`) took the link: it exited with success within `wait`, or
/// is still running then (and is left to end on its own).
fn took(mut child: std::process::Child, program: &str, wait: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    eprintln!("endeavor: {program} couldn't open the browser ({status})");
                }
                return status.success();
            }
            Ok(None) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(50)),
            Ok(None) => {
                std::thread::spawn(move || child.wait());
                return true;
            }
            Err(_) => return false,
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

    #[cfg(unix)]
    #[test]
    fn an_opener_that_fails_is_no_open_browser() {
        let run = |line: &str| Command::new("sh").args(["-c", line]).spawn().unwrap();
        let wait = std::time::Duration::from_millis(500);
        assert!(!took(run("exit 3"), "sh", wait), "no browser took the link");
        assert!(took(run("exit 0"), "sh", wait));
        assert!(took(run("sleep 5"), "sh", wait), "still opening it");
    }

    #[test]
    fn the_note_gives_the_address_first_and_the_forward_only_if_it_fails() {
        let note = note("lab3", 8456, "endeavor open --state-dir /s");
        assert!(note.contains("`ssh -L 8456:localhost:8456 lab3`") && note.contains("`endeavor open --state-dir /s`"), "{note}");
        let (address, forward) = (note.find("Give them `browser_url`").unwrap(), note.find("ssh -L").unwrap());
        assert!(address < forward && note.contains("Only if the address doesn't load"), "{note}");
    }

    #[test]
    fn the_url_is_one_argument() {
        let url = "http://localhost:1/edit?id=a&token=b";
        let (_, args) = command(url);
        assert_eq!(args.last().map(String::as_str), Some(url));
    }
}

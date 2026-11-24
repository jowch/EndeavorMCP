use std::process::{Command, Stdio};

#[derive(Debug, PartialEq)]
enum Opener {
    System,
    Programs(Vec<String>),
    Nothing,
}

const BROWSER_LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

fn chosen(var: &impl Fn(&str) -> Option<String>) -> Opener {
    let value = var("BROWSER").unwrap_or_default();
    let entries: Vec<String> = value.split(BROWSER_LIST_SEPARATOR).map(str::trim).filter(|entry| !entry.is_empty()).map(str::to_owned).collect();
    match entries.first() {
        None => Opener::System,
        Some(first) if first.eq_ignore_ascii_case("none") => Opener::Nothing,
        Some(_) => Opener::Programs(entries),
    }
}

fn with_url(entry: &str, url: &str) -> (String, Vec<String>) {
    if !entry.contains("%s") {
        return (entry.to_owned(), vec![url.to_owned()]);
    }
    let mut words = entry.split_whitespace().map(|word| word.replace("%s", url));
    let program = words.next().unwrap_or_default();
    (program, words.collect())
}

pub(crate) fn can_open(var: impl Fn(&str) -> Option<String>) -> bool {
    match chosen(&var) {
        Opener::Programs(_) => return true,
        Opener::Nothing => return false,
        Opener::System => {}
    }
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    if cfg!(any(target_os = "macos", windows)) {
        return !set("SSH_CONNECTION") && !set("SSH_TTY");
    }
    set("DISPLAY") || set("WAYLAND_DISPLAY")
}

pub(crate) fn shows(var: impl Fn(&str) -> Option<String>) -> bool {
    if cfg!(debug_assertions) {
        return var("ENDEAVOR_TEST_BROWSER").is_some();
    }
    can_open(var)
}

pub(crate) fn note(host: &str, port: u16, open: &str) -> String {
    format!(
        "No browser could be opened on {host}, so the notebook did not open for the user. \
Give them `browser_url` to open in their browser, and don't say it is open: that page shows the notebook, but it can't approve an install. \
If the page asks to let the browser in, they run `{open}` in a terminal on {host} and open the link it prints. \
Only if the address doesn't load at all, their browser is on another computer: there they run `ssh -L {port}:localhost:{port} {host}` \
(with the name they use to ssh to {host}), keep it open, and open `browser_url` again. \
You can't forward the port for them: it starts on their computer."
    )
}

fn commands(url: &str, opener: Opener) -> Vec<(String, Vec<String>)> {
    match opener {
        Opener::Nothing => Vec::new(),
        Opener::Programs(entries) => entries.iter().map(|entry| with_url(entry, url)).collect(),
        Opener::System if cfg!(target_os = "macos") => vec![("open".to_owned(), vec![url.to_owned()])],
        Opener::System if cfg!(windows) => vec![("rundll32.exe".to_owned(), vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()])],
        Opener::System => vec![("xdg-open".to_owned(), vec![url.to_owned()])],
    }
}

const OPENER_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn open(url: &str) -> bool {
    if cfg!(debug_assertions) {
        let Some(file) = std::env::var_os("ENDEAVOR_TEST_BROWSER") else { return false };
        use std::io::Write;
        return std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{url}")).is_ok();
    }
    let var = |name: &str| std::env::var(name).ok();
    if !can_open(var) {
        return false;
    }
    let opener = chosen(&var);
    let users_own = matches!(opener, Opener::Programs(_));
    first_that_opens(commands(url, opener), users_own)
}

fn first_that_opens(tried: Vec<(String, Vec<String>)>, users_own: bool) -> bool {
    tried.into_iter().any(|(program, args)| match Command::new(&program).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut child) => {
            if cfg!(windows) && !users_own {
                std::thread::spawn(move || child.wait());
                return true;
            }
            took(child, &program, OPENER_WAIT)
        }
        Err(e) => {
            eprintln!("endeavor: couldn't open the browser ({program}): {e}");
            false
        }
    })
}

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
        for opener in [Opener::System, Opener::Programs(vec!["firefox".into()])] {
            let (_, args) = commands(url, opener).remove(0);
            assert_eq!(args.last().map(String::as_str), Some(url));
        }
    }

    #[test]
    fn browser_names_the_programs_to_try_in_order_and_none_opens_nothing() {
        let url = "http://localhost:1/edit?id=a";
        let tried = |value: &str| commands(url, chosen(&vars(&[("BROWSER", value)])));
        assert_eq!(chosen(&vars(&[])), Opener::System);
        assert_eq!(chosen(&vars(&[("BROWSER", "  ")])), Opener::System, "an empty BROWSER is no choice");
        assert_eq!(chosen(&vars(&[("BROWSER", "None")])), Opener::Nothing);
        assert!(tried("none").is_empty());
        let sep = BROWSER_LIST_SEPARATOR;
        let listed = tried(&format!("firefox{sep}{sep} chromium --new-window %s "));
        assert_eq!(listed, vec![("firefox".to_owned(), vec![url.to_owned()]), ("chromium".to_owned(), vec!["--new-window".to_owned(), url.to_owned()])], "each entry in turn; %s is the link");
        if cfg!(windows) {
            let program = r"C:\Program Files\Firefox\firefox.exe";
            assert_eq!(tried(program), vec![(program.to_owned(), vec![url.to_owned()])], "a path with spaces is one program");
        }
        let over_ssh = [("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22"), ("SSH_TTY", "/dev/pts/1")];
        assert!(can_open(vars(&[over_ssh[0], over_ssh[1], ("BROWSER", "/usr/local/bin/code-open")])), "a BROWSER of the user's own opens even over ssh, as an editor's remote helper does");
        assert!(!can_open(vars(&[("DISPLAY", ":0"), ("BROWSER", "none")])));
    }

    #[cfg(unix)]
    #[test]
    fn the_next_browser_is_tried_when_one_is_missing_or_fails() {
        let dir = std::env::temp_dir().join(format!("endeavor-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let opened = dir.join("opened");
        let script = dir.join("opener");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$1\" > '{}'\n", opened.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tried = commands("http://localhost:1/", chosen(&vars(&[("BROWSER", &format!("{}/missing:false:{}", dir.display(), script.display()))])));
        assert!(first_that_opens(tried, true));
        assert_eq!(std::fs::read_to_string(&opened).unwrap().trim(), "http://localhost:1/");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

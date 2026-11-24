use std::io::Write;
use std::process::{Command, Stdio};

const ADAPTER: &str = include_str!("../../../runtime/marimo/adapter.py");

fn between<'a>(from: &str, to: &str) -> &'a str {
    let start = ADAPTER.find(from).unwrap();
    &ADAPTER[start..start + ADAPTER[start..].find(to).unwrap()]
}

fn run(body: &str, lines: &[String]) -> Option<Vec<String>> {
    let script = format!("import ntpath, os, sys\n{}\n{}\n{body}", between("def drive_root(", "\nROOT = "), between("def other_drive(", "def on_marimos_drive("));
    let works = Command::new("python3").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !works {
        return None;
    }
    let mut python = Command::new("python3").args(["-I", "-c", &script]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    python.stdin.take().unwrap().write_all(lines.join("\n").as_bytes()).unwrap();
    let out = python.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect())
}

fn other_drives(pairs: &[(&str, &str)]) -> Option<Vec<String>> {
    let lines: Vec<String> = pairs.iter().map(|(path, root)| format!("{path}|{root}")).collect();
    run("for line in sys.stdin.read().splitlines():\n    path, root = line.split('|')\n    print(other_drive(path, root, ntpath.splitdrive) or '-')\n", &lines)
}

#[test]
fn marimo_is_started_at_the_root_of_the_drive_its_first_notebook_resolves_to() {
    let cases = [(r"C:\Users\me\subst-target", r"C:\"), (r"\\server\share\data", r"\\server\share\"), (r"D:\", r"D:\")];
    let lines: Vec<String> = cases.iter().map(|(resolved, _)| resolved.to_string()).collect();
    let Some(found) = run("for line in sys.stdin.read().splitlines():\n    print(drive_root(line, ntpath.splitdrive, '\\\\'))\n", &lines) else {
        eprintln!("no python3: skipped");
        return;
    };
    for ((resolved, want), got) in cases.iter().zip(&found) {
        assert_eq!(got, want, "{resolved}");
    }
}

#[test]
fn a_notebook_is_on_marimos_drive_by_its_drive_or_share_whatever_its_case() {
    let cases = [
        (r"D:\data\growth.py", r"D:\", "-"),
        (r"d:\data\growth.py", r"D:\", "-"),
        (r"C:\Users\me\a.py", r"D:\", "C:"),
        (r"\\server\share\a.py", r"\\server\share\", "-"),
        (r"\\SERVER\Share\a.py", r"\\server\share\", "-"),
        (r"\\server\other\a.py", r"\\server\share\", r"\\server\other"),
        (r"D:\a.py", r"\\server\share\", "D:"),
        ("/home/me/a.py", "/", "-"),
    ];
    let pairs: Vec<(&str, &str)> = cases.iter().map(|&(path, root, _)| (path, root)).collect();
    let Some(found) = other_drives(&pairs) else {
        eprintln!("no python3: skipped");
        return;
    };
    for ((path, root, want), got) in cases.iter().zip(&found) {
        assert_eq!(got, want, "{path} against {root}");
    }
}

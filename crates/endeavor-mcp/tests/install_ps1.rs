//! `scripts/install.ps1` under Windows PowerShell 5.1 (`powershell.exe`), against a release served
//! over http on 127.0.0.1 whose files are behind redirects, as GitHub's are. Under 5.1 the script
//! follows each redirect by hand (#98), so this checks that it still gets there, checks the
//! checksum, and reports a missing file plainly. The refusal of a redirect from https to http needs
//! a TLS server and isn't covered here.
//!
//! The script adds its folder to the user's PATH in the registry, so this runs only in CI
//! (`CI` set), on a runner that is thrown away after.

#![cfg(windows)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Serves `files` under `/real/`. `/rel/LATEST` redirects there with a relative address, any
/// other `/rel/NAME` with an absolute one; anything else is a 404.
fn serve(files: HashMap<String, Vec<u8>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let files = Arc::new(files);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let files = files.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let (status, location, body): (&str, String, Vec<u8>) = if path == "/rel/LATEST" {
                    ("302 Found", "/real/LATEST".into(), Vec::new())
                } else if let Some(name) = path.strip_prefix("/rel/") {
                    ("301 Moved Permanently", format!("http://127.0.0.1:{port}/real/{name}"), Vec::new())
                } else if let Some(body) = path.strip_prefix("/real/").and_then(|name| files.get(name)) {
                    ("200 OK", String::new(), body.clone())
                } else {
                    ("404 Not Found", String::new(), b"not found".to_vec())
                };
                let mut stream = stream;
                let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                if !location.is_empty() {
                    head.push_str(&format!("Location: {location}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });
    port
}

fn install(release: &str, dir: &Path, pin: Option<&str>) -> (bool, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.ps1");
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(script).env("ENDEAVOR_RELEASE_URL", release).env("ENDEAVOR_INSTALL_DIR", dir);
    match pin {
        Some(url) => command.env("ENDEAVOR_PIN_URL", url),
        None => command.env_remove("ENDEAVOR_PIN_URL"),
    };
    let out = command.output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn windows_powershell_follows_the_releases_redirects_by_hand_and_checks_the_download() {
    if std::env::var_os("CI").is_none() {
        eprintln!("skipped: install.ps1 changes the user's PATH, so this runs only in CI");
        return;
    }
    let exe = b"not really an exe".to_vec();
    let name = format!("endeavor-{KEY}-windows-x86_64.exe");
    let mut files = HashMap::new();
    files.insert("LATEST".to_owned(), format!("{KEY}\n").into_bytes());
    files.insert(format!("endeavor-{KEY}.sha256"), format!("{}  {name}\n", sha(&exe)).into_bytes());
    files.insert(name.clone(), exe.clone());
    // Main's pins: one naming an older build the release still holds, and one whose checksum isn't the release's binary's.
    let pinned = "00000000aaaa";
    let pinned_exe = b"the pinned build".to_vec();
    files.insert(format!("endeavor-{pinned}-windows-x86_64.exe"), pinned_exe.clone());
    files.insert("pin-older".to_owned(), format!("{pinned}\r\n{}  endeavor-{pinned}-windows-x86_64.exe\r\n", sha(&pinned_exe)).into_bytes());
    files.insert("pin-other-sum".to_owned(), format!("{KEY}\n{}  {name}\n", sha(b"the real build")).into_bytes());
    let port = serve(files);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("install-ps1");
    let _ = std::fs::remove_dir_all(&dir);

    let (ok, said) = install(&format!("http://127.0.0.1:{port}/rel"), &dir, None);
    assert!(ok, "{said}");
    assert!(said.contains(&format!("Installed endeavor (build {KEY}, windows-x86_64)")), "{said}");
    assert_eq!(std::fs::read(dir.join("endeavor.exe")).unwrap(), exe);

    // A release that answers 404 says which file it couldn't get, and installs nothing over the binary there.
    let (ok, said) = install(&format!("http://127.0.0.1:{port}/missing"), &dir, None);
    assert!(!ok && said.contains(&format!("Couldn't download http://127.0.0.1:{port}/missing/LATEST")) && said.contains("404"), "{said}");
    assert_eq!(std::fs::read(dir.join("endeavor.exe")).unwrap(), exe);

    // Main pins a build: that one is installed, checked against main's checksum.
    let (ok, said) = install(&format!("http://127.0.0.1:{port}/rel"), &dir, Some(&format!("http://127.0.0.1:{port}/rel/pin-older")));
    assert!(ok, "{said}");
    assert!(said.contains(&format!("Installed endeavor (build {pinned}, windows-x86_64)")), "{said}");
    assert_eq!(std::fs::read(dir.join("endeavor.exe")).unwrap(), pinned_exe);

    // The release's binary and checksum file agree, but main pins another checksum: refused.
    let (ok, said) = install(&format!("http://127.0.0.1:{port}/rel"), &dir, Some(&format!("http://127.0.0.1:{port}/rel/pin-other-sum")));
    assert!(!ok && said.contains("doesn't match the checksum main pins"), "{said}");
    assert_eq!(std::fs::read(dir.join("endeavor.exe")).unwrap(), pinned_exe);

    // Main can't be read: LATEST, checked against the release's checksum file, with a warning.
    let (ok, said) = install(&format!("http://127.0.0.1:{port}/rel"), &dir, Some(&format!("http://127.0.0.1:{port}/no-such-pin")));
    assert!(ok && said.contains("Couldn't read the build main pins"), "{said}");
    assert_eq!(std::fs::read(dir.join("endeavor.exe")).unwrap(), exe);
}

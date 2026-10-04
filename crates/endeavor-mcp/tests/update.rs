//! `endeavor update`, run as a user runs it, against a release served on
//! 127.0.0.1 (`ENDEAVOR_RELEASE_URL`). The full update needs Linux, the only
//! platform with prebuilt binaries; elsewhere it says to use cargo install.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use sha2::{Digest, Sha256};

const KEY: &str = "0123456789ab";
const NEW: &[u8] = b"#!/bin/sh\necho 'endeavor 0.1.0 (build 0.1.0-00000000000000aa)'\n";

fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

/// Serves LATEST, the checksums and this platform's NEW, `sum` being the checksum file's claim.
fn release(sum: String) -> String {
    let name = format!("endeavor-{KEY}-{}", platform().unwrap_or("linux-x86_64"));
    let files = [("LATEST".to_owned(), format!("{KEY}\n").into_bytes()), (format!("endeavor-{KEY}.sha256"), format!("{sum}  {name}\n").into_bytes()), (name, NEW.to_vec())];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                line.clear();
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_owned();
            let body = files.iter().find(|(name, _)| *name == path).map(|(_, body)| body.clone());
            let status = if body.is_some() { "200 OK" } else { "404 Not Found" };
            let body = body.unwrap_or_default();
            let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(&body);
        }
    });
    url
}

/// A copy of the built binary in a scratch folder of its own, as if downloaded there.
fn copy(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-update-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("endeavor");
    std::fs::copy(env!("CARGO_BIN_EXE_endeavor"), &exe).unwrap();
    exe
}

fn update(exe: &PathBuf, release: &str) -> (bool, String, String) {
    let home = exe.parent().unwrap().join("home");
    let out = Command::new(exe)
        .arg("update")
        .env("ENDEAVOR_RELEASE_URL", release)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("XDG_STATE_HOME", home.join("state"))
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn update_replaces_the_binary_or_says_to_use_cargo() {
    let exe = copy("good");
    let (ok, stdout, stderr) = update(&exe, &release(sha(NEW)));
    if platform().is_none() {
        assert!(!ok && stderr.starts_with("There's no prebuilt endeavor for this platform") && stderr.contains("cargo install --git"), "{stderr}");
    } else {
        assert!(ok, "{stderr}");
        assert!(stdout.starts_with("Updated ") && stdout.contains(&format!("to the newest build ({KEY})")), "{stdout}");
        assert_eq!(std::fs::read(&exe).unwrap(), NEW);
    }
    let _ = std::fs::remove_dir_all(exe.parent().unwrap());
}

#[test]
fn update_with_a_bad_checksum_leaves_the_binary() {
    let exe = copy("bad");
    let before = std::fs::read(&exe).unwrap();
    let (ok, _, stderr) = update(&exe, &release(sha(b"not it")));
    assert!(!ok);
    if platform().is_some() {
        assert!(stderr.contains("doesn't match its checksum"), "{stderr}");
    }
    assert_eq!(std::fs::read(&exe).unwrap(), before);
    assert_eq!(std::fs::read_dir(exe.parent().unwrap()).unwrap().count(), 1, "nothing left beside it");
    let _ = std::fs::remove_dir_all(exe.parent().unwrap());
}

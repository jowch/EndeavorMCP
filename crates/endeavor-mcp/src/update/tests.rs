use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;

/// A release served on 127.0.0.1: file name to contents. Its URL.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/');
            let mut stream = stream;
            match files.get(path) {
                Some(body) => {
                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(body);
                }
                None => drop(write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")),
            }
        }
    });
    url
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("endeavor-update-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

const KEY: &str = "4859266ddff1";
/// The newest build: a script that answers `--version` as endeavor does.
const NEW: &[u8] = b"#!/bin/sh\necho 'endeavor 0.1.0 (build 0.1.0-00000000000000aa)'\n";

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A release with NEW for linux-x86_64, whose checksum file says `sum`.
fn release(sum: &str) -> String {
    let name = format!("endeavor-{KEY}-linux-x86_64");
    serve(HashMap::from([
        ("LATEST".to_owned(), format!("{KEY}\n").into_bytes()),
        (format!("endeavor-{KEY}.sha256"), format!("{}  endeavor-{KEY}-linux-aarch64\n{sum}  {name}\n", sha(b"other")).into_bytes()),
        (name, NEW.to_vec()),
    ]))
}

/// An old `endeavor` in `dir/bin`, updating from `release`.
fn here(dir: &Path, release: String) -> Here {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("endeavor");
    std::fs::write(&exe, "old").unwrap();
    Here { exe, platform: Some("linux-x86_64"), home: dir.join("home"), cargo_home: dir.join("home/.cargo"), release, state_dir: dir.join("state") }
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n != "endeavor").collect()
}

#[test]
fn it_replaces_the_binary_with_the_newest_build_then_is_up_to_date() {
    let dir = scratch("updates");
    let here = here(&dir, release(&sha(NEW)));
    let message = update(&here).unwrap();
    assert_eq!(message, format!("Updated {} to the newest build ({KEY}).", here.exe.display()));
    assert_eq!(std::fs::read(&here.exe).unwrap(), NEW);
    assert_eq!(std::fs::metadata(&here.exe).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(leftovers(&dir.join("bin")), Vec::<String>::new());

    let message = update(&here).unwrap();
    assert_eq!(message, format!("endeavor is up to date: {} is the newest build ({KEY}).", here.exe.display()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_download_that_fails_its_checksum_changes_nothing() {
    let dir = scratch("checksum");
    let here = here(&dir, release(&sha(b"something else")));
    let error = update(&here).unwrap_err();
    assert!(error.contains("doesn't match its checksum") && error.contains("is unchanged"), "{error}");
    assert_eq!(std::fs::read(&here.exe).unwrap(), b"old");
    assert_eq!(leftovers(&dir.join("bin")), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_release_without_this_platform_or_unreachable_says_so() {
    let dir = scratch("missing");
    let mut here = here(&dir, release(&sha(NEW)));
    here.platform = Some("linux-riscv64");
    assert_eq!(update(&here).unwrap_err(), format!("The newest build ({KEY}) has no binary for linux-riscv64."));
    here.release = serve(HashMap::new());
    assert!(update(&here).unwrap_err().starts_with("Couldn't download http://127.0.0.1:"));
    assert_eq!(std::fs::read(&here.exe).unwrap(), b"old");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn it_ends_with_how_to_switch_when_julia_runs_from_the_old_build() {
    let dir = scratch("running");
    let here = here(&dir, release(&sha(NEW)));
    std::fs::create_dir_all(&here.state_dir).unwrap();
    let state = json!({ "launcher": "process", "node": crate::hostname(), "pid": std::process::id(), "port": 1, "token": "t", "build": embedded::BUILD_VERSION });
    std::fs::write(here.state_dir.join("runtime.json"), state.to_string()).unwrap();
    let message = update(&here).unwrap();
    assert!(message.contains(&format!("({KEY}).\nThe Julia running from {}", here.state_dir.display())), "{message}");
    assert!(message.contains("this is build 0.1.0-00000000000000aa") && message.ends_with("run `endeavor stop`, then start it again."), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copies_the_app_or_cargo_installed_are_left_to_them() {
    let dir = scratch("managed");
    let nowhere = "http://127.0.0.1:9".to_owned();
    let mut here = here(&dir, nowhere.clone());

    let app_helper = here.home.join(".cache/endeavor/0.1.0-0123456789abcdef");
    std::fs::create_dir_all(&app_helper).unwrap();
    here.exe = app_helper.join("endeavor");
    let error = update(&here).unwrap_err();
    assert!(error.contains("belongs to the Endeavor app") && error.ends_with("Update the app instead."), "{error}");
    here.exe = dir.join("Endeavor.app/Contents/MacOS/endeavor");
    assert!(update(&here).unwrap_err().contains("belongs to the Endeavor app"));

    here.exe = here.cargo_home.join("bin/endeavor");
    assert_eq!(update(&here).unwrap_err(), format!("This copy of endeavor was installed with cargo. Update it the same way:\n    {CARGO_INSTALL}"));

    here.exe = dir.join("bin/endeavor");
    here.platform = None;
    let error = update(&here).unwrap_err();
    assert!(error.starts_with("There's no prebuilt endeavor for this platform (") && error.ends_with(CARGO_INSTALL), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

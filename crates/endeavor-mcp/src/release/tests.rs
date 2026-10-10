use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;

const KEY: &str = "0123456789ab";
const HELPER: &[u8] = b"the helper for another platform";

/// A scratch folder under the workspace's `target/tmp`.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("release-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// The `file://` address of `path` as curl wants it: on Windows `file:///D:/a/x`,
/// from `D:\a\x` or the canonical `\\?\D:\a\x`, and `file://server/share/x` from a UNC path.
fn file_url(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = match path.strip_prefix("//?/UNC/") {
        Some(unc) => format!("//{unc}"),
        None => path.strip_prefix("//?/").map_or(path.clone(), str::to_owned),
    };
    let encoded: String = path.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/:-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    match encoded.as_bytes() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => format!("file:///{encoded}"),
        _ if encoded.starts_with("//") => format!("file:{encoded}"),
        _ => format!("file://{encoded}"),
    }
}

/// The address of a folder on this computer.
fn folder_url(folder: &Path) -> String {
    file_url(&folder.display().to_string())
}

#[test]
fn file_addresses_are_valid_on_every_platform() {
    assert_eq!(file_url("/mnt/a b/target/tmp/x"), "file:///mnt/a%20b/target/tmp/x");
    assert_eq!(file_url(r"\\?\D:\a\EndeavorMCP\target\tmp\release"), "file:///D:/a/EndeavorMCP/target/tmp/release");
    assert_eq!(file_url(r"C:\Users\me\x y"), "file:///C:/Users/me/x%20y");
    assert_eq!(file_url(r"\\?\UNC\server\share\release"), "file://server/share/release");
}

/// A release as a folder, with `HELPER` for linux-aarch64 whose checksum file says `sum`; its `file://` address.
fn release(dir: &Path, sum: &str) -> String {
    let folder = dir.join("release");
    std::fs::create_dir_all(&folder).unwrap();
    let name = asset_name(KEY, "linux-aarch64");
    std::fs::write(folder.join(format!("endeavor-{KEY}.sha256")), format!("{}  endeavor-{KEY}-linux-x86_64\n{sum}  {name}\n", sha256_hex(b"other"))).unwrap();
    std::fs::write(folder.join(name), HELPER).unwrap();
    folder_url(&folder)
}

/// Everything under `dir`, as relative paths.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(next) = todo.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            found.push(path.strip_prefix(dir).unwrap().display().to_string().replace('\\', "/"));
            if path.is_dir() {
                todo.push(path);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn the_helper_is_fetched_checked_and_kept_for_the_owner() {
    let dir = scratch("fetch");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(HELPER)));
    let kept = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap();
    assert_eq!(kept, cache.join(KEY).join("linux-aarch64/endeavor"));
    assert_eq!(std::fs::read(&kept).unwrap(), HELPER);
    assert_eq!(std::fs::read_to_string(cache.join(KEY).join("linux-aarch64/endeavor.sha256")).unwrap(), sha256_hex(HELPER));
    assert_eq!(files(&cache), [KEY, &format!("{KEY}/linux-aarch64"), &format!("{KEY}/linux-aarch64/endeavor"), &format!("{KEY}/linux-aarch64/endeavor.lock"), &format!("{KEY}/linux-aarch64/endeavor.sha256")]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&kept), mode(kept.parent().unwrap()), mode(&cache.join(KEY))), (0o600, 0o700, 0o700));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_kept_helper_is_used_again_without_downloading_unless_it_changed() {
    let dir = scratch("reuse");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(HELPER)));
    let first = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap();
    // The release is gone, so a second call that succeeds didn't download.
    std::fs::remove_dir_all(dir.join("release")).unwrap();
    assert_eq!(fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap(), first);

    std::fs::write(&first, "changed on disk").unwrap();
    let error = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap_err();
    assert!(error.starts_with("Couldn't get the helper for linux aarch64 servers from the release"), "{error}");
    assert_eq!(std::fs::read(&first).unwrap(), b"changed on disk", "a kept file is only replaced by a checked download");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_download_that_fails_its_checksum_is_refused_and_nothing_is_kept() {
    let dir = scratch("checksum");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(b"something else")));
    let error = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap_err();
    assert!(error.contains("doesn't match its checksum") && error.ends_with("It was deleted."), "{error}");
    assert_eq!(files(&cache), [KEY, &format!("{KEY}/linux-aarch64"), &format!("{KEY}/linux-aarch64/endeavor.lock")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_release_without_the_helper_says_so() {
    let dir = scratch("missing");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(HELPER)));
    // Not in the checksum file.
    let error = fetch_helper("darwin", "aarch64", Some(KEY), &url, &cache).unwrap_err();
    assert_eq!(error, format!("The release has no helper for darwin aarch64 servers in build {KEY} (endeavor-{KEY}-darwin-aarch64 isn't in its checksum file)."));
    // In the checksum file, but the file isn't there.
    let error = fetch_helper("linux", "x86_64", Some(KEY), &url, &cache).unwrap_err();
    assert!(error.starts_with("Couldn't get the helper for linux x86_64 servers from the release: Couldn't download file://"), "{error}");
    // No checksum file for this key, and no LATEST.
    let error = fetch_helper("linux", "aarch64", Some("ffffffffffff"), &url, &cache).unwrap_err();
    assert!(error.starts_with("Couldn't get the helper for linux aarch64 servers from the release: Couldn't download file://"), "{error}");
    // No checksum file for this key, and LATEST names another: it was removed.
    std::fs::write(dir.join("release").join("LATEST"), format!("{KEY}\n")).unwrap();
    let error = fetch_helper("linux", "aarch64", Some("ffffffffffff"), &url, &cache).unwrap_err();
    assert_eq!(error, "Build ffffffffffff of endeavor is old and was removed from the release, so it can't get the helper for linux aarch64 servers. Update endeavor: update the plugin, or run `endeavor update` if you installed it with the install script.");
    assert!(files(&cache).iter().all(|f| !f.ends_with("endeavor") && !f.contains(".part")), "{:?}", files(&cache));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_build_without_a_release_key_says_what_it_needs() {
    let error = fetch_helper("darwin", "aarch64", None, "file:///nowhere", Path::new("/nowhere")).unwrap_err();
    assert_eq!(error, "A helper for darwin aarch64 servers needs a release build of Endeavor, installed with the install script, and this build has none.");
}

#[test]
fn a_platform_the_release_doesnt_build_for_says_so() {
    for (os, arch) in [("linux", "riscv64"), ("freebsd", "x86_64"), ("windows", "x86_64")] {
        assert_eq!(fetch_helper(os, arch, Some(KEY), "file:///nowhere", Path::new("/nowhere")).unwrap_err(), client::no_helper(os, arch));
    }
}

#[test]
fn platforms_get_the_releases_names() {
    let names: Vec<_> = [("linux", "x86_64"), ("linux", "aarch64"), ("darwin", "x86_64"), ("darwin", "aarch64"), ("windows", "x86_64"), ("windows", "aarch64"), ("linux", "arm"), ("freebsd", "amd64")].iter().map(|(os, arch)| platform_name(os, arch)).collect();
    assert_eq!(names, [Some("linux-x86_64"), Some("linux-aarch64"), Some("darwin-x86_64"), Some("darwin-aarch64"), Some("windows-x86_64"), None, None, None]);
    assert_eq!(asset_name(KEY, "linux-x86_64"), format!("endeavor-{KEY}-linux-x86_64"));
    assert_eq!(asset_name(KEY, "windows-x86_64"), format!("endeavor-{KEY}-windows-x86_64.exe"));
}

#[test]
fn a_checksum_file_is_read_as_sha256sum_writes_it() {
    let sums = "AB12  endeavor-k-linux-x86_64\ncd34 *endeavor-k-windows-x86_64.exe\n";
    assert_eq!(checksum_for(sums, "endeavor-k-linux-x86_64").as_deref(), Some("ab12"));
    assert_eq!(checksum_for(sums, "endeavor-k-windows-x86_64.exe").as_deref(), Some("cd34"));
    assert_eq!(checksum_for(sums, "endeavor-k-linux-aarch64"), None);
}

/// A release served on 127.0.0.1 that counts the requests for each path.
fn serve(files: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(HashMap::new()));
    let counts = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (files, counts) = (files.clone(), counts.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_owned();
                *counts.lock().unwrap().entry(path.clone()).or_default() += 1;
                let mut stream = stream;
                match files.get(&path) {
                    Some(body) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(body);
                    }
                    None => drop(write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")),
                }
            });
        }
    });
    (url, seen)
}

#[test]
fn helpers_fetched_at_once_are_downloaded_once_and_all_valid() {
    let dir = scratch("concurrent");
    let cache = dir.join("cache");
    let name = asset_name(KEY, "linux-aarch64");
    let sums = format!("{}  {name}\n", sha256_hex(HELPER));
    let (url, seen) = serve(HashMap::from([(format!("endeavor-{KEY}.sha256"), sums.into_bytes()), (name.clone(), HELPER.to_vec())]));
    let paths: Vec<_> = (0..6)
        .map(|_| {
            let (url, cache) = (url.clone(), cache.clone());
            std::thread::spawn(move || fetch_helper("linux", "aarch64", Some(KEY), &url, &cache))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap().unwrap())
        .collect();
    for path in &paths {
        assert_eq!(path, &cache.join(KEY).join("linux-aarch64/endeavor"));
        assert_eq!(std::fs::read(path).unwrap(), HELPER);
    }
    assert_eq!(seen.lock().unwrap().get(&name), Some(&1), "{:?}", seen.lock().unwrap());
    assert!(files(&cache).iter().all(|f| !f.contains(".part")), "{:?}", files(&cache));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_folders_of_other_keys_are_removed_when_a_helper_is_fetched_or_reused() {
    let dir = scratch("prune");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(HELPER)));
    let older = cache.join("ffffffffffff/linux-aarch64");
    std::fs::create_dir_all(&older).unwrap();
    std::fs::write(older.join("endeavor"), "old").unwrap();
    fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap();
    assert!(!cache.join("ffffffffffff").exists());

    std::fs::create_dir_all(&older).unwrap();
    std::fs::remove_dir_all(dir.join("release")).unwrap();
    let kept = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap();
    assert!(kept.exists() && !cache.join("ffffffffffff").exists());

    // A failed fetch leaves the others alone.
    std::fs::create_dir_all(&older).unwrap();
    fetch_helper("linux", "aarch64", Some("eeeeeeeeeeee"), &url, &cache).unwrap_err();
    assert!(older.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

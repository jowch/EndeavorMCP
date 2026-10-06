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

/// A release as a folder, with `HELPER` for linux-aarch64 whose checksum file says `sum`; its `file://` address.
fn release(dir: &Path, sum: &str) -> String {
    let folder = dir.join("release");
    std::fs::create_dir_all(&folder).unwrap();
    let name = asset_name(KEY, "linux-aarch64");
    std::fs::write(folder.join(format!("endeavor-{KEY}.sha256")), format!("{}  endeavor-{KEY}-linux-x86_64\n{sum}  {name}\n", sha256_hex(b"other"))).unwrap();
    std::fs::write(folder.join(name), HELPER).unwrap();
    format!("file://{}", folder.display())
}

/// Everything under `dir`, as relative paths.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_owned()];
    while let Some(next) = todo.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            found.push(path.strip_prefix(dir).unwrap().display().to_string());
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
    assert_eq!(files(&cache), [KEY, &format!("{KEY}/linux-aarch64"), &format!("{KEY}/linux-aarch64/endeavor"), &format!("{KEY}/linux-aarch64/endeavor.sha256")]);
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
    assert!(!first.exists(), "a kept file that no longer matches is deleted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_download_that_fails_its_checksum_is_refused_and_nothing_is_kept() {
    let dir = scratch("checksum");
    let (cache, url) = (dir.join("cache"), release(&dir, &sha256_hex(b"something else")));
    let error = fetch_helper("linux", "aarch64", Some(KEY), &url, &cache).unwrap_err();
    assert!(error.contains("doesn't match its checksum") && error.ends_with("It was deleted."), "{error}");
    assert_eq!(files(&cache), [KEY, &format!("{KEY}/linux-aarch64")]);
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
    // No checksum file for this key.
    let error = fetch_helper("linux", "aarch64", Some("ffffffffffff"), &url, &cache).unwrap_err();
    assert!(error.starts_with("Couldn't get the helper for linux aarch64 servers from the release: Couldn't download file://"), "{error}");
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

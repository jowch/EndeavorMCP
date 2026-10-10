//! Which R runs R notebooks: an Rscript the user gave, the one a shell line of
//! theirs sets up (`module load R`), or else Endeavor's own R when it's installed,
//! else the one on their login shell's PATH.
//!
//! Endeavor installs its own R only on a Mac, only when no R is found, and only
//! after the user agrees (`install_own`): CRAN's installer, unpacked into
//! Endeavor's folder rather than run, so it needs no admin rights, puts nothing
//! on the PATH and edits no shell startup file. On Linux and on servers, people
//! install R themselves (rig, the system's packages, or a cluster's module).
//!
//! Unlike Julia, R isn't looked for when the runtime starts: the core starts it
//! the first time an R notebook opens, and a runtime that never opens one never
//! needs R. Nor is it found once and then run by its path: the shell line and
//! the login shell run each time R starts, so what they set up besides the PATH
//! (a module's libraries, the compiler Ember's install builds with) is there too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Source {
    /// `--r PATH`: an Rscript, or the R beside one.
    Path(String),
    /// `--r auto`: Endeavor's own when it's installed, else the login shell's (on Windows the PATH's).
    #[default]
    Auto,
    /// `--r-shell LINE`: what `LINE` puts on the login shell's PATH.
    Shell(String),
}

impl Source {
    /// `--r VALUE` or `--r-shell VALUE`, as `flag` says.
    pub fn from_flag(flag: &str, value: String) -> Source {
        match (flag, value.as_str()) {
            ("--r-shell", _) => Source::Shell(value),
            (_, "auto") => Source::Auto,
            _ => Source::Path(value),
        }
    }

    /// The flag and value that give this to another `endeavor` command.
    pub fn args(&self) -> [String; 2] {
        match self {
            Source::Path(path) => ["--r".into(), path.clone()],
            Source::Auto => ["--r".into(), "auto".into()],
            Source::Shell(line) => ["--r-shell".into(), line.clone()],
        }
    }

    /// How the user would recognize it in an error.
    pub fn describe(&self) -> String {
        match self {
            Source::Path(path) => path.clone(),
            Source::Auto => match own_rscript() {
                Some(_) => format!("Endeavor's own R {OWN_VERSION} in {}", own_dir(&crate::paths::Env::here()).display()),
                None => "Rscript on the login shell's PATH".into(),
            },
            Source::Shell(line) => format!("Rscript after `{line}` in a login shell"),
        }
    }

    /// `Rscript ARGS`. Through the user's shell, the arguments are passed in the
    /// environment, which every shell reads the same way (sh, bash, zsh, fish,
    /// csh and tcsh), and not as the script's own arguments, which they don't. A
    /// shell that ran but found no Rscript ends with 127 (`not_found`); one that
    /// ended with 0 never started it (`ended_early`).
    pub fn command(&self, args: &[&Path]) -> Command {
        self.command_in(&std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into()), args)
    }

    /// `command` with `shell` as the user's shell.
    fn command_in(&self, shell: &str, args: &[&Path]) -> Command {
        let shell = |line: Option<&str>| {
            let quoted: String = (0..args.len()).map(|i| format!(" \"$ENDEAVOR_R_ARG{i}\"")).collect();
            let line = line.map(|line| line.replace('\n', "; "));
            let mut command = Command::new(shell);
            if is_csh(shell) {
                // csh and tcsh take `-l` only as their one flag, so they run as a plain `-c`, which still reads
                // ~/.cshrc or ~/.tcshrc, where module setup goes. Their `exit` doesn't end a `-c` early, and their
                // `exec` of a missing program ends with 1, so after the line, sh starts R.
                let line = line.map_or(String::new(), |line| format!("{line}; "));
                command.args(["-c", &format!("{line}exec /bin/sh -c 'command -v Rscript >/dev/null || exit 127; exec Rscript{quoted}'")]);
            } else {
                let line = line.map_or(String::new(), |line| format!("{line} && "));
                command.args(["-lc", &format!("{line}exec Rscript{quoted}")]);
            }
            for (i, arg) in args.iter().enumerate() {
                command.env(format!("ENDEAVOR_R_ARG{i}"), arg);
            }
            command
        };
        match self {
            Source::Path(path) => {
                let mut command = Command::new(rscript_at(path));
                command.args(args);
                command
            }
            // Its packages go in its own library, never in the user's R library.
            Source::Auto if let Some(rscript) = own_rscript() => {
                let mut command = Command::new(&rscript);
                command.args(args).env("R_LIBS_USER", own_dir(&crate::paths::Env::here()).join(USER_LIBRARY));
                command
            }
            Source::Auto if cfg!(windows) => {
                let mut command = Command::new("Rscript");
                command.args(args);
                command
            }
            Source::Auto => shell(None),
            Source::Shell(line) => shell(Some(line)),
        }
    }

    /// Whether `status` is a shell's that found no Rscript.
    pub fn not_found(&self, status: std::process::ExitStatus) -> bool {
        !matches!(self, Source::Path(_)) && status.code() == Some(127)
    }

    /// The folder of Endeavor's own R, when this is `--r auto` and it's installed: the R that runs.
    pub fn own(&self) -> Option<PathBuf> {
        (matches!(self, Source::Auto) && own_rscript().is_some()).then(|| own_dir(&crate::paths::Env::here()))
    }

    /// Endeavor's own R, which may be installed here, when this is `--r auto`, it isn't installed yet,
    /// and this is a Mac with an installer for its kind of processor: what installing it means.
    pub fn own_item(&self) -> Option<wire::Item> {
        (matches!(self, Source::Auto) && own_rscript().is_none()).then(|| own_item_for(macos_version(), &crate::julia::uname("-m"), &crate::paths::Env::here())).flatten()
    }

    /// Whether `status` is a shell's that ended well without starting R: a shell line
    /// that ends in a comment (`module load R  # 4.4`) comments out the rest of the script.
    pub fn ended_early(&self, status: std::process::ExitStatus) -> bool {
        matches!(self, Source::Shell(_)) && status.success()
    }
}

/// csh and tcsh, which read `-c` scripts their own way.
fn is_csh(shell: &str) -> bool {
    matches!(shell.rsplit('/').next(), Some("csh" | "tcsh" | "bsd-csh"))
}

/// `path` with `~/` expanded, and the Rscript beside it when it names R itself.
fn rscript_at(path: &str) -> String {
    let path = match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
        _ => path.to_owned(),
    };
    // As text, so the path keeps the separators it was given.
    match path.rsplit(['/', '\\']).next() {
        Some(name @ ("R" | "R.exe")) => format!("{}{}", &path[..path.len() - name.len()], name.replacen('R', "Rscript", 1)),
        _ => path,
    }
}

/// Endeavor's own R, pinned like its Julia: CRAN's installer for each kind of Mac, with the oldest
/// macOS it runs on, its SHA-256 and size.
pub const OWN_VERSION: &str = "4.6.1";
const INSTALLERS: [(&str, u32, &str, &str, u64); 2] = [
    (
        "arm64",
        14,
        "https://cloud.r-project.org/bin/macosx/sonoma-arm64/base/R-4.6.1-arm64.pkg",
        "67f6eea4ced4ce48f0a0d4fa3a1cac43d1859a05a88993ee3dff7c52e7edbc4b",
        105_066_342,
    ),
    (
        "x86_64",
        11,
        "https://cloud.r-project.org/bin/macosx/big-sur-x86_64/base/R-4.6.1-x86_64.pkg",
        "612bb00cb4c627721d6d80b0f5224227c0fcdefb4a5b6c917511480361c16571",
        108_253_722,
    ),
];

/// About how much room the installed R takes, in MB.
const INSTALLED_MB: u64 = 165;

/// Its packages' library, inside it: `R_LIBS_USER` when it runs.
const USER_LIBRARY: &str = "user-library";

/// Where the installer's R puts itself, which its scripts name.
const FRAMEWORK: &str = "/Library/Frameworks/R.framework";

/// The folder Endeavor's own R is in, installed or not: `~/.cache/endeavor/R-4.6.1`.
pub fn own_dir(env: &crate::paths::Env) -> PathBuf {
    env.server_root().join(format!("R-{OWN_VERSION}"))
}

/// Endeavor's own Rscript, if it's installed.
fn own_rscript() -> Option<PathBuf> {
    let env = crate::paths::Env::here();
    if env.home.as_os_str().is_empty() {
        return None;
    }
    Some(own_dir(&env).join("bin").join("Rscript")).filter(|rscript| rscript.is_file())
}

/// What installing Endeavor's own R means on macOS `macos` (its major version; `None` when this
/// isn't a Mac) with the processor `arch` (`uname -m`).
fn own_item_for(macos: Option<u32>, arch: &str, env: &crate::paths::Env) -> Option<wire::Item> {
    if env.home.as_os_str().is_empty() || !INSTALLERS.iter().any(|i| i.0 == arch && macos.is_some_and(|v| v >= i.1)) {
        return None;
    }
    Some(wire::Item { kind: wire::KIND_RUNTIME.into(), name: format!("R {OWN_VERSION}"), size_mb: Some(INSTALLED_MB), place: Some(own_dir(env).display().to_string()) })
}

/// Install Endeavor's own R on this Mac: download CRAN's installer, check it, and unpack it
/// into `own_dir` without running it (as rig's user mode does), then make it run from there.
/// Only after the user agreed. `progress` hears each step.
pub fn install_own(progress: &mut dyn FnMut(String)) -> Result<(), String> {
    let env = crate::paths::Env::here();
    let arch = crate::julia::uname("-m");
    if own_item_for(macos_version(), &arch, &env).is_none() {
        return Err(format!("Endeavor installs its own R only on a Mac, and has no R {OWN_VERSION} for this one ({arch})."));
    }
    if own_rscript().is_some() {
        return Ok(());
    }
    let &(_, _, url, sha256, size) = INSTALLERS.iter().find(|i| i.0 == arch).expect("own_item_for checked it");
    let (root, dir, name) = (env.server_root(), own_dir(&env), format!("R {OWN_VERSION}"));
    std::fs::create_dir_all(&root).map_err(|e| format!("Couldn't create {}: {e}", root.display()))?;
    let part = root.join(format!("R-{OWN_VERSION}-{arch}.pkg.part"));
    crate::julia::download(&part, url, sha256, size, &name, progress)?;
    progress(format!("Unpacking {name}…"));
    // pkgutil takes only a file named .pkg.
    let pkg = part.with_extension("");
    std::fs::rename(&part, &pkg).map_err(|e| e.to_string())?;
    // Unpacked beside the target, then renamed, so a half-unpacked R is never used.
    let staging = root.join(format!("R-{OWN_VERSION}.unpacking"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let unpacked = unpack(&pkg, &staging).and_then(|resources| {
        patch(&resources, &dir)?;
        std::fs::create_dir_all(resources.join(USER_LIBRARY)).map_err(|e| e.to_string())?;
        std::fs::rename(&resources, &dir).map_err(|e| format!("Couldn't move R into {}: {e}", dir.display()))
    });
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&pkg);
    unpacked.map_err(|why| format!("Couldn't unpack {name}: {why}"))?;
    // Fonts for plots, as R's installer does; R still runs if it fails.
    progress(format!("Setting up fonts for {name}…"));
    let _ = Command::new(dir.join("bin").join("fc-cache")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    Ok(())
}

/// macOS's major version (`sw_vers`), on a Mac.
fn macos_version() -> Option<u32> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("sw_vers").arg("-productVersion").stdin(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().split('.').next()?.parse().ok()
}

/// Expand the installer into `staging` and unpack its R framework: R's own folder (`Resources`).
fn unpack(pkg: &Path, staging: &Path) -> Result<PathBuf, String> {
    let expanded = staging.join("pkg");
    let status = Command::new("pkgutil").arg("--expand").arg(pkg).arg(&expanded).stdout(Stdio::null()).status().map_err(|e| format!("pkgutil: {e}"))?;
    if !status.success() {
        return Err(format!("pkgutil --expand ended with {status}"));
    }
    let payload = ["R-fw.pkg", "r.pkg"].iter().map(|part| expanded.join(part).join("Payload")).find(|p| p.is_file()).ok_or("the installer has no R framework")?;
    let framework = staging.join("framework");
    std::fs::create_dir_all(&framework).map_err(|e| e.to_string())?;
    let status = Command::new("sh")
        .args(["-c", "gzip -dcf \"$0\" | cpio -i 2>/dev/null"])
        .arg(&payload)
        .current_dir(&framework)
        .status()
        .map_err(|e| format!("cpio: {e}"))?;
    let resources = framework.join("R.framework/Versions/Current/Resources");
    if !status.success() || !resources.join("bin").join("R").is_file() {
        return Err(format!("unpacking the framework ended with {status}"));
    }
    Ok(resources)
}

/// Make the R in `resources` run from `home` (where it is moved next), not from `/Library/Frameworks`
/// where its installer would put it: what rig's user mode does.
fn patch(resources: &Path, home: &Path) -> Result<(), String> {
    let home = home.display().to_string();
    let quoted = sh_quoted(&home);
    let edit = |path: &str, change: &dyn Fn(String) -> String| -> Result<(), String> {
        let path = resources.join(path);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::fs::write(&path, change(text)).map_err(|e| format!("{}: {e}", path.display()))
    };
    // R's own script: its home, and the libraries beside it (they name /Library/Frameworks).
    edit("bin/R", &|text| {
        let home_line = format!("R_HOME_DIR={quoted}\nexport DYLD_LIBRARY_PATH=\"${{R_HOME_DIR}}/lib\"");
        replace_lines(&text, |line| line.starts_with("R_HOME_DIR=").then(|| home_line.clone())).replace(&format!("{FRAMEWORK}/Resources"), "${R_HOME}")
    })?;
    // Packages built from source link to its library here.
    edit("etc/Makeconf", &|text| replace_lines(&text, |line| line.strip_prefix("LIBR").is_some_and(|rest| rest.trim_start().starts_with('=')).then(|| "LIBR = -L\"$(R_HOME)/lib\" -lR".into())))?;
    let qpdf = sh_quoted(&format!("{home}/bin/qpdf"));
    edit("etc/Renviron", &|text| replace_lines(&text, |line| line.starts_with("R_QPDF=").then(|| format!("R_QPDF=${{R_QPDF-{qpdf}}}"))))?;
    if resources.join("fontconfig/fonts/fonts.conf").is_file() {
        let escaped = home.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        edit("fontconfig/fonts/fonts.conf", &|text| text.replace(&format!("{FRAMEWORK}/Resources"), &escaped))?;
    }
    if resources.join("lib/pkgconfig/libR.pc").is_file() {
        edit("lib/pkgconfig/libR.pc", &|text| {
            let text = replace_lines(&text, |line| line.starts_with("rincludedir=").then(|| "rincludedir=${rhome}/include".into()));
            replace_versioned(&text, &home)
        })?;
    }
    // Rscript is a program that finds R through RHOME when it's set, so each becomes a script that sets it.
    for (rscript, beside) in [("bin/Rscript", "bin"), ("Rscript", "")] {
        let path = resources.join(rscript);
        let original = path.with_file_name("Rscript.orig");
        std::fs::rename(&path, &original).map_err(|e| format!("{}: {e}", path.display()))?;
        let original = sh_quoted(&Path::new(&home).join(beside).join("Rscript.orig").display().to_string());
        write_script(&path, &format!("#!/bin/sh\nRHOME={quoted}\nexport RHOME\nexec {original} \"$@\"\n"))?;
    }
    let fc_cache = resources.join("bin/fc-cache");
    if fc_cache.is_file() {
        std::fs::rename(&fc_cache, fc_cache.with_file_name("fc-cache.orig")).map_err(|e| e.to_string())?;
        let original = sh_quoted(&Path::new(&home).join("bin/fc-cache.orig").display().to_string());
        let fonts = sh_quoted(&Path::new(&home).join("fontconfig/fonts/fonts.conf").display().to_string());
        write_script(&fc_cache, &format!("#!/bin/sh\nFONTCONFIG_FILE={fonts}\nexport FONTCONFIG_FILE\nexec {original} \"$@\"\n"))?;
    }
    // Older Rs carry these, which shadow the system's and crash once the libraries come from DYLD_LIBRARY_PATH.
    for shadow in ["libc++.1.dylib", "libc++abi.1.dylib", "libunwind.1.dylib"] {
        let _ = std::fs::remove_file(resources.join("lib").join(shadow));
    }
    Ok(())
}

/// `text` with each line `change` gives a new one for replaced.
fn replace_lines(text: &str, change: impl Fn(&str) -> Option<String>) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let (bare, end) = line.strip_suffix('\n').map_or((line, ""), |bare| (bare, "\n"));
            change(bare).map_or_else(|| line.to_owned(), |new| format!("{new}{end}"))
        })
        .collect()
}

/// `text` with each `/Library/Frameworks/R.framework/Versions/<version>/Resources` as `home`.
fn replace_versioned(text: &str, home: &str) -> String {
    let prefix = format!("{FRAMEWORK}/Versions/");
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(&prefix) {
        let after = &rest[at + prefix.len()..];
        match after.find('/').filter(|&slash| after[slash..].starts_with("/Resources")) {
            Some(slash) => {
                out.push_str(&rest[..at]);
                out.push_str(home);
                rest = &after[slash + "/Resources".len()..];
            }
            None => {
                out.push_str(&rest[..at + prefix.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` in single quotes, for sh.
fn sh_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn write_script(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_their_values() {
        assert_eq!(Source::from_flag("--r", "auto".into()), Source::Auto);
        assert_eq!(Source::from_flag("--r", "/opt/R/bin/Rscript".into()), Source::Path("/opt/R/bin/Rscript".into()));
        assert_eq!(Source::from_flag("--r-shell", "module load R".into()), Source::Shell("module load R".into()));
        for source in [Source::Auto, Source::Path("/x/Rscript".into()), Source::Shell("module load R/4.4".into())] {
            let [flag, value] = source.args();
            assert_eq!(Source::from_flag(&flag, value), source);
        }
    }

    #[test]
    fn a_path_to_r_itself_runs_the_rscript_beside_it() {
        assert_eq!(rscript_at("/opt/R/4.4/bin/R"), "/opt/R/4.4/bin/Rscript");
        assert_eq!(rscript_at("/opt/R/4.4/bin/Rscript"), "/opt/R/4.4/bin/Rscript");
        assert_eq!(rscript_at("/Rig/R"), "/Rig/Rscript");
    }

    #[test]
    fn a_path_keeps_its_separators() {
        assert_eq!(rscript_at(r"C:\Program Files\R\R-4.4.1\bin\R.exe"), r"C:\Program Files\R\R-4.4.1\bin\Rscript.exe");
    }

    /// In each shell this machine has: the line runs first and what it sets reaches R, the arguments
    /// arrive whole, a line that leaves no Rscript is `not_found`, and one that ends in a comment is `ended_early`.
    #[cfg(unix)]
    #[test]
    fn a_shell_line_runs_before_rscript_with_the_arguments_whole() {
        let dir = std::env::temp_dir().join(format!("endeavor-r-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("Rscript");
        // Prints what the shell line set, then its arguments one per line.
        std::fs::write(&fake, "#!/bin/sh\necho \"loaded=$LOADED\"\nfor a in \"$@\"; do echo \"$a\"; done\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let args = [Path::new("--vanilla"), Path::new("/a path/with 'quotes', \"double\", $dollars, `ticks` and !bang.R")];
        let expected = format!("loaded=yes\n--vanilla\n{}\n", args[1].display());
        let d = dir.display();
        let shells = [
            ("/bin/sh", format!("export PATH=\"{d}:$PATH\" LOADED=yes"), "PATH=/nonexistent"),
            ("/bin/bash", format!("export PATH=\"{d}:$PATH\" LOADED=yes"), "PATH=/nonexistent"),
            ("/bin/zsh", format!("export PATH=\"{d}:$PATH\" LOADED=yes"), "PATH=/nonexistent"),
            ("/usr/bin/fish", format!("set -gx PATH {d} $PATH; set -gx LOADED yes"), "set -gx PATH /nonexistent"),
            ("/bin/tcsh", format!("setenv PATH \"{d}:$PATH\"; setenv LOADED yes"), "setenv PATH /nonexistent"),
            ("/bin/csh", format!("setenv PATH \"{d}:$PATH\"; setenv LOADED yes"), "setenv PATH /nonexistent"),
        ];
        let mut tried = 0;
        for (shell, line, no_r) in shells {
            if !Path::new(shell).exists() {
                continue;
            }
            tried += 1;
            let source = Source::Shell(line);
            let output = source.command_in(shell, &args).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{shell}: {}", String::from_utf8_lossy(&output.stderr));
            let none = Source::Shell(no_r.into());
            let status = none.command_in(shell, &args).stderr(std::process::Stdio::null()).status().unwrap();
            assert!(none.not_found(status), "{shell}: {status}");
            let commented = Source::Shell("true # R 4.4".into());
            let status = commented.command_in(shell, &args).status().unwrap();
            assert!(commented.ended_early(status), "{shell}: {status}");
        }
        assert!(tried > 0);
        let output = Source::Path(fake.display().to_string()).command(&args).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected.replace("loaded=yes", "loaded="));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn own_r_is_offered_only_on_a_mac_it_has_an_installer_for() {
        let env = crate::paths::Env::from_vars(&|name| (name == "HOME").then(|| "/Users/ada".into()));
        let item = own_item_for(Some(14), "arm64", &env).unwrap();
        assert_eq!(item.to_string(), format!("R {OWN_VERSION} (about {INSTALLED_MB} MB, into {})", own_dir(&env).display()));
        assert!(own_dir(&env).ends_with(format!("R-{OWN_VERSION}")));
        assert!(own_item_for(Some(11), "x86_64", &env).is_some());
        assert_eq!(own_item_for(Some(13), "arm64", &env), None, "CRAN's arm64 R needs macOS 14");
        assert_eq!(own_item_for(None, "arm64", &env), None, "not a Mac");
        assert_eq!(own_item_for(Some(15), "ppc", &env), None);
        assert_eq!(Source::Shell("module load R".into()).own_item(), None);
        assert_eq!(Source::Path("/opt/R/bin/Rscript".into()).own_item(), None);
    }

    #[test]
    fn lines_and_framework_paths_are_replaced() {
        let text = "R_HOME_DIR=\"/x\"\n   R_HOME_DIR=\"/y\"\nLIBR0 = a\nLIBR = b\nlast";
        let changed = replace_lines(text, |line| line.starts_with("R_HOME_DIR=").then(|| "R_HOME_DIR=/z".into()));
        assert_eq!(changed, "R_HOME_DIR=/z\n   R_HOME_DIR=\"/y\"\nLIBR0 = a\nLIBR = b\nlast");
        let pc = "rhome=/Library/Frameworks/R.framework/Versions/4.6-arm64/Resources\nx=/Library/Frameworks/R.framework/Versions/4.6/lib\n";
        assert_eq!(replace_versioned(pc, "/h"), "rhome=/h\nx=/Library/Frameworks/R.framework/Versions/4.6/lib\n");
        assert_eq!(sh_quoted("/Users/a b/it's"), r"'/Users/a b/it'\''s'");
    }

    /// The installer's R, made to run from a home folder with a space and a quote in its path: its
    /// scripts name that folder, and each Rscript runs the original with RHOME set to it.
    #[cfg(unix)]
    #[test]
    fn the_installers_r_is_made_to_run_from_its_new_home() {
        let dir = std::env::temp_dir().join(format!("endeavor-own-r-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let resources = dir.join("Resources");
        let home = dir.join("Ada's R");
        let files = [
            ("bin/R", "#!/bin/sh\nR_HOME_DIR=\"/Library/Frameworks/R.framework/Resources\"\nif test \"${R_HOME_DIR}\" = \"/Library/Frameworks/lib/R\"; then\n  R_HOME_DIR=\"/Library/Frameworks/lib/R\"\nfi\nR_SHARE_DIR=\"/Library/Frameworks/R.framework/Resources/share\"\n"),
            ("etc/Makeconf", "LIBR0 = -L\"$(R_HOME)/lib$(R_ARCH)\"\nLIBR = -F/Library/Frameworks/R.framework/.. -framework R\n"),
            ("etc/Renviron", "R_PAPERSIZE=a4\nR_QPDF=${R_QPDF-'/Library/Frameworks/R.framework/Resources/bin/qpdf'}\n"),
            ("fontconfig/fonts/fonts.conf", "<cachedir>/Library/Frameworks/R.framework/Resources/fontconfig/cache</cachedir>\n"),
            ("lib/pkgconfig/libR.pc", "rhome=/Library/Frameworks/R.framework/Versions/4.6/Resources\nrincludedir=/Library/Frameworks/R.framework/Versions/4.6/Resources/include\n"),
            ("lib/libc++.1.dylib", ""),
            ("bin/Rscript", "#!/bin/sh\necho \"bin $RHOME $*\"\n"),
            ("Rscript", "#!/bin/sh\necho \"top $RHOME $*\"\n"),
            ("bin/fc-cache", "#!/bin/sh\necho \"$FONTCONFIG_FILE\"\n"),
        ];
        for (path, text) in files {
            let path = resources.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_script(&path, text).unwrap();
        }
        patch(&resources, &home).unwrap();
        std::fs::rename(&resources, &home).unwrap();
        let read = |path: &str| std::fs::read_to_string(home.join(path)).unwrap();
        let h = home.display().to_string();
        assert_eq!(
            read("bin/R"),
            format!("#!/bin/sh\nR_HOME_DIR='{}'\nexport DYLD_LIBRARY_PATH=\"${{R_HOME_DIR}}/lib\"\nif test \"${{R_HOME_DIR}}\" = \"/Library/Frameworks/lib/R\"; then\n  R_HOME_DIR=\"/Library/Frameworks/lib/R\"\nfi\nR_SHARE_DIR=\"${{R_HOME}}/share\"\n", h.replace('\'', "'\\''"))
        );
        assert_eq!(read("etc/Makeconf"), "LIBR0 = -L\"$(R_HOME)/lib$(R_ARCH)\"\nLIBR = -L\"$(R_HOME)/lib\" -lR\n");
        assert_eq!(read("etc/Renviron"), format!("R_PAPERSIZE=a4\nR_QPDF=${{R_QPDF-'{}/bin/qpdf'}}\n", h.replace('\'', "'\\''")));
        assert_eq!(read("fontconfig/fonts/fonts.conf"), format!("<cachedir>{h}/fontconfig/cache</cachedir>\n"));
        assert_eq!(read("lib/pkgconfig/libR.pc"), format!("rhome={h}\nrincludedir=${{rhome}}/include\n"));
        assert!(!home.join("lib/libc++.1.dylib").exists());
        let run = |path: &str| String::from_utf8(Command::new(home.join(path)).arg("--version").output().unwrap().stdout).unwrap();
        assert_eq!(run("bin/Rscript"), format!("bin {h} --version\n"));
        assert_eq!(run("Rscript"), format!("top {h} --version\n"));
        assert_eq!(run("bin/fc-cache"), format!("{h}/fontconfig/fonts/fonts.conf\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

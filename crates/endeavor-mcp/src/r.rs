use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Source {
    Path(String),
    #[default]
    Auto,
    Shell(String),
}

impl Source {
    pub fn from_flag(flag: &str, value: String) -> Source {
        match (flag, value.as_str()) {
            ("--r-shell", _) => Source::Shell(value),
            (_, "auto") => Source::Auto,
            _ => Source::Path(value),
        }
    }

    pub fn args(&self) -> [String; 2] {
        match self {
            Source::Path(path) => ["--r".into(), path.clone()],
            Source::Auto => ["--r".into(), "auto".into()],
            Source::Shell(line) => ["--r-shell".into(), line.clone()],
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Path(path) => path.clone(),
            Source::Auto => match own_in_use() {
                Some(_) => format!("Endeavor's own R {OWN_VERSION} in {}", own_dir(&crate::paths::Env::here()).display()),
                None => "Rscript on the login shell's PATH".into(),
            },
            Source::Shell(line) => format!("Rscript after `{line}` in a login shell"),
        }
    }

    pub fn rscript(&self) -> String {
        match self {
            Source::Path(path) => path.clone(),
            Source::Auto => match own_in_use() {
                Some(own) => own.display().to_string(),
                None => users_rscript().unwrap_or_else(|| self.describe()),
            },
            Source::Shell(_) => self.describe(),
        }
    }

    pub fn command(&self, args: &[&Path]) -> Command {
        self.command_in(&std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into()), args)
    }

    fn command_in(&self, shell: &str, args: &[&Path]) -> Command {
        let shell = |line: Option<&str>| {
            let quoted: String = (0..args.len()).map(|i| format!(" \"$ENDEAVOR_R_ARG{i}\"")).collect();
            let line = line.map(|line| line.replace('\n', "; "));
            let mut command = Command::new(shell);
            if is_csh(shell) {
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
            Source::Auto if let Some(rscript) = own_in_use() => {
                let own = own_dir(&crate::paths::Env::here());
                let mut command = Command::new(&rscript);
                command.args(args).env("R_LIBS_USER", own.join(USER_LIBRARY)).env("R_ENVIRON_USER", own.join("user-Renviron")).env("R_PROFILE_USER", own.join("user-Rprofile")).env_remove("R_LIBS");
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

    pub fn not_found(&self, status: std::process::ExitStatus) -> bool {
        !matches!(self, Source::Path(_)) && status.code() == Some(127)
    }

    pub fn own(&self) -> Option<PathBuf> {
        (matches!(self, Source::Auto) && own_in_use().is_some()).then(|| own_dir(&crate::paths::Env::here()))
    }

    pub fn own_item(&self) -> Option<wire::Item> {
        (matches!(self, Source::Auto) && own_rscript().is_none()).then(own_offered).flatten()
    }

    pub fn ended_early(&self, status: std::process::ExitStatus) -> bool {
        matches!(self, Source::Shell(_)) && status.success()
    }
}

pub(crate) fn is_csh(shell: &str) -> bool {
    matches!(shell.rsplit('/').next(), Some("csh" | "tcsh" | "bsd-csh"))
}

fn rscript_at(path: &str) -> String {
    let path = match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
        _ => path.to_owned(),
    };
    match path.rsplit(['/', '\\']).next() {
        Some(name @ ("R" | "R.exe")) => format!("{}{}", &path[..path.len() - name.len()], name.replacen('R', "Rscript", 1)),
        _ => path,
    }
}

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

const INSTALLED_MB: u64 = 165;

const USER_LIBRARY: &str = "user-library";

const FRAMEWORK: &str = "/Library/Frameworks/R.framework";

pub fn own_dir(env: &crate::paths::Env) -> PathBuf {
    env.server_root().join(format!("R-{OWN_VERSION}"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnR {
    pub rscript: PathBuf,
    pub version: &'static str,
}

pub fn own_installed() -> Option<OwnR> {
    own_rscript().map(|rscript| OwnR { rscript, version: OWN_VERSION })
}

pub fn own_offered() -> Option<wire::Item> {
    own_item_for(macos_version(), &crate::julia::uname("-m"), &crate::paths::Env::here())
}

fn own_in_use() -> Option<PathBuf> {
    static USERS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let own = own_rscript()?;
    (!*USERS.get_or_init(|| users_rscript().is_some_and(|theirs| Path::new(&theirs) != own))).then_some(own)
}

fn users_rscript() -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let shell = if is_csh(&shell) { "/bin/sh".to_owned() } else { shell };
    let output = Command::new(&shell).args(["-lc", "command -v Rscript"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    output.status.success().then(|| stdout.lines().rev().map(str::trim).find(|l| l.starts_with('/')).map(str::to_owned)).flatten()
}

fn own_rscript() -> Option<PathBuf> {
    let env = crate::paths::Env::here();
    if env.home.as_os_str().is_empty() {
        return None;
    }
    Some(own_dir(&env).join("bin").join("Rscript")).filter(|rscript| rscript.is_file())
}

fn own_item_for(macos: Option<u32>, arch: &str, env: &crate::paths::Env) -> Option<wire::Item> {
    if env.home.as_os_str().is_empty() || !INSTALLERS.iter().any(|i| i.0 == arch && macos.is_some_and(|v| v >= i.1)) {
        return None;
    }
    Some(wire::Item { kind: wire::KIND_RUNTIME.into(), name: format!("R {OWN_VERSION}"), size_mb: Some(INSTALLED_MB), place: Some(own_dir(env).display().to_string()) })
}

pub fn install_own(progress: &mut dyn FnMut(String)) -> Result<OwnR, String> {
    let env = crate::paths::Env::here();
    let arch = crate::julia::uname("-m");
    if own_item_for(macos_version(), &arch, &env).is_none() {
        return Err(format!("Endeavor installs its own R only on a Mac, and has no R {OWN_VERSION} for this one ({arch})."));
    }
    if let Some(own) = own_installed() {
        return Ok(own);
    }
    let &(_, _, url, sha256, size) = INSTALLERS.iter().find(|i| i.0 == arch).expect("own_item_for checked it");
    let (root, dir, name) = (env.server_root(), own_dir(&env), format!("R {OWN_VERSION}"));
    std::fs::create_dir_all(&root).map_err(|e| format!("Couldn't create {}: {e}", root.display()))?;
    let part = root.join(format!("R-{OWN_VERSION}-{arch}.pkg.part"));
    crate::julia::download(&part, url, sha256, size, &name, progress)?;
    progress(format!("Unpacking {name}…"));
    let pkg = part.with_extension("");
    std::fs::rename(&part, &pkg).map_err(|e| e.to_string())?;
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
    progress(format!("Setting up fonts for {name}…"));
    let _ = Command::new(dir.join("bin").join("fc-cache")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    own_installed().ok_or(format!("{name} was unpacked, but its Rscript isn't in {}", dir.display()))
}

pub fn remove_own() -> Result<(), String> {
    let env = crate::paths::Env::here();
    if env.home.as_os_str().is_empty() {
        return Ok(());
    }
    let root = env.server_root();
    let dir = own_dir(&env);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Couldn't remove {}: {e}", dir.display()))?;
    }
    let _ = std::fs::remove_dir_all(root.join(format!("R-{OWN_VERSION}.unpacking")));
    for arch in INSTALLERS.map(|i| i.0) {
        let _ = std::fs::remove_file(root.join(format!("R-{OWN_VERSION}-{arch}.pkg.part")));
        let _ = std::fs::remove_file(root.join(format!("R-{OWN_VERSION}-{arch}.pkg")));
    }
    Ok(())
}

fn macos_version() -> Option<u32> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let output = Command::new("sw_vers").arg("-productVersion").stdin(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().split('.').next()?.parse().ok()
}

fn unpack(pkg: &Path, staging: &Path) -> Result<PathBuf, String> {
    let expanded = staging.join("pkg");
    let mut pkgutil = Command::new("pkgutil");
    crate::host_tools::with_command_mask(pkgutil.arg("--expand").arg(pkg).arg(&expanded).stdout(Stdio::null()));
    let status = pkgutil.status().map_err(|e| format!("pkgutil: {e}"))?;
    if !status.success() {
        return Err(format!("pkgutil --expand ended with {status}"));
    }
    let payload = ["R-fw.pkg", "r.pkg"].iter().map(|part| expanded.join(part).join("Payload")).find(|p| p.is_file()).ok_or("the installer has no R framework")?;
    let framework = staging.join("framework");
    std::fs::create_dir_all(&framework).map_err(|e| e.to_string())?;
    let mut cpio = Command::new("sh");
    crate::host_tools::with_command_mask(cpio.args(["-c", "gzip -dcf \"$0\" | cpio -i 2>/dev/null"]).arg(&payload).current_dir(&framework));
    let status = cpio.status().map_err(|e| format!("cpio: {e}"))?;
    let resources = framework.join("R.framework/Versions/Current/Resources");
    if !status.success() || !resources.join("bin").join("R").is_file() {
        return Err(format!("unpacking the framework ended with {status}"));
    }
    Ok(resources)
}

fn patch(resources: &Path, home: &Path) -> Result<(), String> {
    let home = home.display().to_string();
    let quoted = sh_quoted(&home);
    let edit = |path: &str, change: &dyn Fn(String) -> String| -> Result<(), String> {
        let path = resources.join(path);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::fs::write(&path, change(text)).map_err(|e| format!("{}: {e}", path.display()))
    };
    edit("bin/R", &|text| {
        let home_line = format!("R_HOME_DIR={quoted}\nexport DYLD_LIBRARY_PATH=\"${{R_HOME_DIR}}/lib\"");
        replace_lines(&text, |line| line.starts_with("R_HOME_DIR=").then(|| home_line.clone())).replace(&format!("{FRAMEWORK}/Resources"), "${R_HOME}")
    })?;
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
    for shadow in ["libc++.1.dylib", "libc++abi.1.dylib", "libunwind.1.dylib"] {
        let _ = std::fs::remove_file(resources.join("lib").join(shadow));
    }
    Ok(())
}

fn replace_lines(text: &str, change: impl Fn(&str) -> Option<String>) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let (bare, end) = line.strip_suffix('\n').map_or((line, ""), |bare| (bare, "\n"));
            change(bare).map_or_else(|| line.to_owned(), |new| format!("{new}{end}"))
        })
        .collect()
}

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

    #[cfg(unix)]
    #[test]
    fn a_shell_line_runs_before_rscript_with_the_arguments_whole() {
        let dir = std::env::temp_dir().join(format!("endeavor-r-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("Rscript");
        crate::test_exec::write_executable(&fake, "#!/bin/sh\necho \"loaded=$LOADED\"\nfor a in \"$@\"; do echo \"$a\"; done\n");
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

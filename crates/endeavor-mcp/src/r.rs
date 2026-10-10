//! Which R runs R notebooks: an Rscript the user gave, the one a shell line of
//! theirs sets up (`module load R`), or else the one on their login shell's PATH.
//! Endeavor doesn't download R.
//!
//! Unlike Julia, R isn't looked for when the runtime starts: the core starts it
//! the first time an R notebook opens, and a runtime that never opens one never
//! needs R. Nor is it found once and then run by its path: the shell line and
//! the login shell run each time R starts, so what they set up besides the PATH
//! (a module's libraries, the compiler Ember's install builds with) is there too.

use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Source {
    /// `--r PATH`: an Rscript, or the R beside one.
    Path(String),
    /// `--r auto`: the login shell's (on Windows the PATH's).
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
            Source::Auto => "Rscript on the login shell's PATH".into(),
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
}

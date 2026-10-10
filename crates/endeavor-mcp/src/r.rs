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

    /// `Rscript ARGS`. Through a login shell, the arguments are passed in the
    /// environment, which every shell reads the same way (sh, bash, zsh, fish,
    /// csh), and not as the script's own arguments, which they don't. A shell
    /// that ran but found no Rscript ends with 127 (`not_found`).
    pub fn command(&self, args: &[&Path]) -> Command {
        let shell = |line: Option<&str>| {
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
            let quoted: String = (0..args.len()).map(|i| format!(" \"$ENDEAVOR_R_ARG{i}\"")).collect();
            let script = match line {
                Some(line) => format!("{} >/dev/null && exec Rscript{quoted}", line.replace('\n', "; ")),
                None => format!("exec Rscript{quoted}"),
            };
            let mut command = Command::new(shell);
            command.args(["-lc", &script]);
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

    /// Whether `status` is a login shell's that found no Rscript.
    pub fn not_found(&self, status: std::process::ExitStatus) -> bool {
        !matches!(self, Source::Path(_)) && status.code() == Some(127)
    }
}

/// `path` with `~/` expanded, and the Rscript beside it when it names R itself.
fn rscript_at(path: &str) -> String {
    let path = match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
        _ => path.to_owned(),
    };
    match Path::new(&path).file_name().and_then(|n| n.to_str()) {
        Some(name @ ("R" | "R.exe")) => Path::new(&path).with_file_name(name.replacen('R', "Rscript", 1)).display().to_string(),
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

    #[cfg(unix)]
    #[test]
    fn a_shell_line_runs_before_rscript_with_the_arguments_whole() {
        let dir = std::env::temp_dir().join(format!("endeavor-r-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("Rscript");
        // Prints its arguments one per line, and what the shell line set.
        std::fs::write(&fake, "#!/bin/sh\necho \"loaded=$LOADED\"\nfor a in \"$@\"; do echo \"$a\"; done\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let line = format!("export PATH=\"{}:$PATH\" LOADED=yes; echo noise", dir.display());
        let args = [Path::new("--vanilla"), Path::new("/a path/with 'quotes' and $dollars.R")];
        let output = Source::Shell(line).command(&args).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "loaded=yes\n--vanilla\n/a path/with 'quotes' and $dollars.R\n");
        let none = Source::Shell("PATH=/nonexistent".into());
        assert!(none.not_found(none.command(&args).status().unwrap()));
        let output = Source::Path(fake.display().to_string()).command(&args).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "loaded=\n--vanilla\n/a path/with 'quotes' and $dollars.R\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

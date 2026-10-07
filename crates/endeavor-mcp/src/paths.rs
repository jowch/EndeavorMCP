//! Every folder Endeavor uses, defined here and nowhere else. The desktop app
//! links this crate and takes its folders from here, so the app, the plugin
//! and a server share one state folder.
//!
//! Two shell scripts must repeat a rule because they run before the binary
//! exists: the bootstrap script (`STATE_DIR_SH`, for the state folder) and
//! `scripts/endeavor-mcp.sh` (the binary store, `Env::plugin_bin`). Tests here
//! and in `tests/launcher.rs` run the shell text and compare it with this module.
//!
//! On Unix a variable counts only if it holds an absolute path: a relative one
//! would put Endeavor's files under whichever folder the process started in.

use std::path::{Path, PathBuf};

/// `name`'s value, if it is set to an absolute path, else `None`.
fn absolute_var(read: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    read(name).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// What the folders come from: the environment, read once.
#[derive(Clone, Debug, Default)]
pub struct Env {
    /// Where Endeavor's own folders are when no XDG variable says otherwise:
    /// the user's home, and on Windows `%LOCALAPPDATA%\Endeavor`.
    pub(crate) home: PathBuf,
    /// The user's home folder, on Windows too.
    pub(crate) user_home: PathBuf,
    pub(crate) state_home: Option<PathBuf>,
    pub(crate) cache_home: Option<PathBuf>,
    pub(crate) config_home: Option<PathBuf>,
    pub(crate) data_home: Option<PathBuf>,
    pub(crate) appdata: Option<PathBuf>,
    pub(crate) scratch: Option<String>,
    pub(crate) cwd: PathBuf,
    /// This computer's host name.
    pub(crate) node: String,
}

impl Env {
    pub fn here() -> Env {
        Env::from_vars(&|name| std::env::var(name).ok())
    }

    /// The folders for the environment `read` gives.
    pub fn from_vars(read: &dyn Fn(&str) -> Option<String>) -> Env {
        let var = |name: &str| read(name).filter(|v| !v.is_empty());
        let user_home = absolute_var(read, "HOME").or_else(|| std::env::home_dir().filter(|h| h.is_absolute())).unwrap_or_default();
        #[cfg(windows)]
        let home = var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Endeavor");
        #[cfg(not(windows))]
        let home = user_home.clone();
        Env {
            home,
            user_home,
            state_home: absolute_var(read, "XDG_STATE_HOME"),
            cache_home: absolute_var(read, "XDG_CACHE_HOME"),
            config_home: absolute_var(read, "XDG_CONFIG_HOME"),
            data_home: absolute_var(read, "XDG_DATA_HOME"),
            appdata: var("APPDATA").map(PathBuf::from),
            scratch: var("SCRATCH").filter(|s| s.starts_with('/')),
            cwd: std::env::current_dir().unwrap_or_default(),
            node: crate::hostname(),
        }
    }

    fn state_base(&self) -> PathBuf {
        self.state_home.clone().unwrap_or_else(|| self.home.join(".local/state"))
    }

    fn cache_base(&self) -> PathBuf {
        self.cache_home.clone().unwrap_or_else(|| self.home.join(".cache"))
    }

    /// The runtime's state on this machine, `serve`'s, `mcp`'s and an app's. Per machine, since a home folder is often shared by a cluster's nodes.
    pub fn state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("serve").join(&self.node);
        }
        self.state_base().join("endeavor/serve").join(&self.node)
    }

    /// For `connect --launcher slurm`: one for the whole cluster, since a reconnect
    /// through another login node must find the same job.
    pub fn cluster_state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("cluster");
        }
        self.state_base().join("endeavor/cluster")
    }

    /// The one file that lists the machines, which the app reads and writes as well.
    /// By default `~/.config/endeavor/machines.json` (macOS too), and on Windows `%APPDATA%\Endeavor\machines.json`.
    pub fn machines_file(&self) -> PathBuf {
        if cfg!(windows) {
            return self.appdata.clone().unwrap_or_default().join("Endeavor").join("machines.json");
        }
        let config = self.config_home.clone().or_else(|| (!self.user_home.as_os_str().is_empty()).then(|| self.user_home.join(".config")));
        config.unwrap_or_default().join("endeavor").join("machines.json")
    }

    /// Where the plugins' launcher keeps binaries: `<data>/endeavor/bin` (`plugin_bin_from` for a release set by a variable).
    pub fn plugin_bin(&self) -> PathBuf {
        self.data_home.clone().unwrap_or_else(|| self.user_home.join(".local/share")).join("endeavor/bin")
    }

    /// What projects remember (`projects`).
    pub(crate) fn projects_path(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("projects.json");
        }
        self.state_base().join("endeavor/projects.json")
    }

    /// Where `serve` unpacks the runtime.
    pub(crate) fn cache(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("serve-runtime");
        }
        self.cache_base().join("endeavor/serve")
    }

    /// Helpers fetched from the release for servers of other platforms (`release::fetch_helper`).
    pub(crate) fn helpers_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("helpers");
        }
        self.cache_base().join("endeavor/helpers")
    }

    /// The depot the app's server installs use, so packages installed for one
    /// serve the other; the trailing separator stacks the user's own depots
    /// (~/.julia) behind it, read-only.
    pub(crate) fn depot(&self) -> String {
        if cfg!(windows) {
            return format!("{};", self.home.join("serve-depot").display());
        }
        match &self.scratch {
            Some(scratch) => scratch_depot(scratch),
            None => format!("{}/depot:", server_root(&self.home).display()),
        }
    }
}

/// Where the launcher keeps the binaries of a release set by `ENDEAVOR_RELEASE_URL`,
/// `<data>/endeavor/bin-from/<checksum of the address>`: beside `plugin_bin`.
pub(crate) fn plugin_bin_from(plugin_bin: &Path) -> PathBuf {
    plugin_bin.with_file_name("bin-from")
}

/// What the app and the bootstrap script install into on a server or this
/// computer: `~/.cache/endeavor`. It ignores `XDG_CACHE_HOME`, since the app
/// installs to the same folder.
pub fn server_root(home: &Path) -> PathBuf {
    home.join(".cache/endeavor")
}

/// A cluster's depot on its scratch folder, where home quotas are small.
pub(crate) fn scratch_depot(scratch: &str) -> String {
    format!("{scratch}/endeavor/depot:")
}

/// Shell for the bootstrap script: sets `pd` to the state folder a server keeps
/// when the client names none, as `Env::state_dir` and `Env::cluster_state_dir`
/// give it, for the launcher in `$ln`. It runs before any binary is installed,
/// so it can't ask one. `uname -n` is what `gethostname` returns.
pub(crate) const STATE_DIR_SH: &str = r#"case "${XDG_STATE_HOME:-}" in /*) pd="$XDG_STATE_HOME";; *) pd="$HOME/.local/state";; esac; case "$ln" in slurm) pd="$pd/endeavor/cluster";; *) pd="$pd/endeavor/serve/$(uname -n)";; esac"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(set: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| set.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn only_an_absolute_path_counts() {
        let abs = if cfg!(windows) { r"C:\data" } else { "/data" };
        let read = |name: &str| match name {
            "ABS" => Some(abs.to_owned()),
            "REL" => Some("data/share".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(absolute_var(&read, "ABS"), Some(abs.into()));
        for name in ["REL", "EMPTY", "UNSET"] {
            assert_eq!(absolute_var(&read, name), None, "{name}");
        }
    }

    #[test]
    fn the_machines_file_is_where_the_configuration_folder_says() {
        if cfg!(windows) {
            assert_eq!(Env::from_vars(&vars(&[("APPDATA", r"C:\Users\jc\AppData\Roaming")])).machines_file(), Path::new(r"C:\Users\jc\AppData\Roaming").join("Endeavor").join("machines.json"));
            return;
        }
        let file = |set| Env::from_vars(&vars(set)).machines_file();
        assert_eq!(file(&[("XDG_CONFIG_HOME", "/x/config"), ("HOME", "/h")]), Path::new("/x/config/endeavor/machines.json"));
        assert_eq!(file(&[("HOME", "/h")]), Path::new("/h/.config/endeavor/machines.json"));
        assert_eq!(file(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")]), Path::new("/h/.config/endeavor/machines.json"), "empty is unset");
        assert_eq!(file(&[("XDG_CONFIG_HOME", "relative"), ("HOME", "/h")]), Path::new("/h/.config/endeavor/machines.json"), "XDG says to ignore a relative one");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_xdg_or_home_variable_is_ignored() {
        let env = Env::from_vars(&vars(&[("HOME", "/home/ada"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "/var/cache/ada"), ("XDG_DATA_HOME", "share")]));
        assert_eq!(env.projects_path(), Path::new("/home/ada/.local/state/endeavor/projects.json"));
        assert_eq!(env.helpers_dir(), Path::new("/var/cache/ada/endeavor/helpers"));
        assert_eq!(env.plugin_bin(), Path::new("/home/ada/.local/share/endeavor/bin"));
        let relative_home = Env::from_vars(&|name| (name == "HOME").then(|| "home/ada".to_owned()));
        assert!(relative_home.home.is_absolute() || relative_home.home.as_os_str().is_empty(), "{:?}", relative_home.home);
    }

    #[cfg(unix)]
    #[test]
    fn the_bootstrap_script_finds_the_state_folder_this_module_does() {
        use std::process::Command;
        let uname = Command::new("uname").arg("-n").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&uname.stdout).trim_end(), crate::hostname(), "gethostname and `uname -n` name the same host");
        assert!(crate::client::bootstrap_script("v1", false).contains(STATE_DIR_SH), "the script holds the shell text that is tested");
        let path = std::env::var("PATH").unwrap();
        for xdg in [None, Some("/xdg/state"), Some("xdg/state"), Some("")] {
            for launcher in ["process", "slurm"] {
                let mut command = Command::new("sh");
                command.arg("-c").arg(format!("{STATE_DIR_SH}; printf %s \"$pd\"")).env_clear().env("PATH", &path).env("HOME", "/home/ada").env("ln", launcher);
                if let Some(xdg) = xdg {
                    command.env("XDG_STATE_HOME", xdg);
                }
                let said = String::from_utf8(command.output().unwrap().stdout).unwrap();
                let read = |name: &str| match name {
                    "HOME" => Some("/home/ada".to_owned()),
                    "XDG_STATE_HOME" => xdg.map(str::to_owned),
                    _ => None,
                };
                let env = Env::from_vars(&read);
                let want = if launcher == "slurm" { env.cluster_state_dir() } else { env.state_dir() };
                assert_eq!(Path::new(&said), want, "XDG_STATE_HOME {xdg:?}, {launcher}");
            }
        }
    }
}

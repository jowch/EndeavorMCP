use std::path::{Path, PathBuf};

fn absolute_var(read: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    read(name).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
}

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub(crate) home: PathBuf,
    pub(crate) local: PathBuf,
    pub(crate) state_home: Option<PathBuf>,
    pub(crate) cache_home: Option<PathBuf>,
    pub(crate) config_home: Option<PathBuf>,
    pub(crate) data_home: Option<PathBuf>,
    pub(crate) appdata: Option<PathBuf>,
    pub(crate) scratch: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) node: String,
}

impl Env {
    pub fn here() -> Env {
        Env::from_vars(&|name| std::env::var(name).ok())
    }

    pub fn from_vars(read: &dyn Fn(&str) -> Option<String>) -> Env {
        let var = |name: &str| read(name).filter(|v| !v.is_empty());
        let local = if cfg!(windows) { var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Endeavor") } else { PathBuf::new() };
        Env {
            home: absolute_var(read, "HOME").or_else(|| std::env::home_dir().filter(|h| h.is_absolute())).unwrap_or_default(),
            local,
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

    pub fn state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("serve").join(&self.node);
        }
        self.state_base().join("endeavor/serve").join(&self.node)
    }

    pub fn apps_old_state_dir(&self) -> Option<PathBuf> {
        (!cfg!(windows) && !self.home.as_os_str().is_empty()).then(|| self.server_root().join("state"))
    }

    pub fn cluster_state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("cluster");
        }
        self.state_base().join("endeavor/cluster")
    }

    pub fn machines_file(&self) -> PathBuf {
        if cfg!(windows) {
            return self.appdata.clone().unwrap_or_default().join("Endeavor").join("machines.json");
        }
        let config = self.config_home.clone().or_else(|| (!self.home.as_os_str().is_empty()).then(|| self.home.join(".config")));
        config.unwrap_or_default().join("endeavor").join("machines.json")
    }

    pub fn plugin_bin(&self) -> PathBuf {
        self.data_home.clone().unwrap_or_else(|| self.home.join(".local/share")).join("endeavor/bin")
    }

    pub(crate) fn claude_modes_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("claude-modes");
        }
        self.state_base().join("endeavor/claude-modes")
    }

    pub(crate) fn projects_path(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("projects.json");
        }
        self.state_base().join("endeavor/projects.json")
    }

    pub(crate) fn cache(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("serve-runtime");
        }
        self.cache_base().join("endeavor/serve")
    }

    pub(crate) fn helpers_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("helpers");
        }
        self.cache_base().join("endeavor/helpers")
    }

    pub fn depot(&self) -> String {
        if cfg!(windows) {
            return format!("{};", self.local.join("serve-depot").display());
        }
        match &self.scratch {
            Some(scratch) => scratch_depot(scratch),
            None if self.home.as_os_str().is_empty() => format!("/{}/depot:", SERVER_ROOT),
            None => format!("{}/depot:", self.server_root().display()),
        }
    }

    pub fn ember_folder(&self) -> PathBuf {
        if cfg!(windows) {
            return self.local.join("r").join("ember");
        }
        server_folders(self.scratch.as_deref(), &self.server_root()).ember
    }

    pub fn server_root(&self) -> PathBuf {
        self.home.join(SERVER_ROOT)
    }
}

const SERVER_ROOT: &str = ".cache/endeavor";

pub(crate) fn plugin_bin_from(plugin_bin: &Path) -> PathBuf {
    plugin_bin.with_file_name("bin-from")
}

pub(crate) fn scratch_depot(scratch: &str) -> String {
    format!("{scratch}/endeavor/depot:")
}

pub(crate) struct ServerFolders {
    pub depot: String,
    pub ember: PathBuf,
}

pub(crate) fn server_folders(scratch: Option<&str>, cache: &Path) -> ServerFolders {
    match scratch {
        Some(scratch) => ServerFolders { depot: scratch_depot(scratch), ember: Path::new(scratch).join("endeavor/r/ember") },
        None => ServerFolders { depot: format!("{}/depot:", cache.display()), ember: cache.join("r").join("ember") },
    }
}

pub fn with_user_depot(depot: &str) -> String {
    let home = std::env::home_dir().unwrap_or_default();
    with_user_depot_from(depot, std::env::var("JULIA_DEPOT_PATH").ok().as_deref(), &home, if cfg!(windows) { ';' } else { ':' })
}

fn with_user_depot_from(depot: &str, theirs: Option<&str>, home: &Path, sep: char) -> String {
    let Some(entries) = depot.strip_suffix(sep) else { return depot.to_owned() };
    let ours: Vec<&str> = entries.split(sep).filter(|e| !e.is_empty()).collect();
    if ours.is_empty() {
        return depot.to_owned();
    }
    let default = (!home.as_os_str().is_empty()).then(|| home.join(".julia").display().to_string());
    let user = theirs.into_iter().flat_map(|t| t.split(sep)).map(str::to_owned).find(|e| !e.is_empty() && !ours.contains(&e.as_str())).or(default);
    match user {
        Some(user) if !ours.contains(&user.as_str()) => format!("{entries}{sep}{user}{sep}"),
        _ => depot.to_owned(),
    }
}

pub(crate) const PICK_STATE_DIR_SH: &str = r#"if [ -n "$st" ]; then pd="$sd"; else case "${XDG_STATE_HOME:-}" in /*) pd="$XDG_STATE_HOME";; *) pd="$HOME/.local/state";; esac; case "$ln" in slurm) pd="$pd/endeavor/cluster";; *) pd="$pd/endeavor/serve/$(uname -n)"; po="$HOME/.cache/endeavor/state"; pj=$(cat "$po/runtime.json" 2>/dev/null); case "$pj" in *node?:?"$(uname -n)"?[,}]*pid?:[0-9]*) pk=${pj#*pid?:}; pk=$(printf %s "$pk" | tr ",}" "  " | cut -d" " -f1); if [ -n "$pk" ] && [ -z "$(printf %s "$pk" | tr -d 0-9)" ] && kill -0 "$pk" 2>/dev/null; then pa=$(ps -ww -p "$pk" -o args= 2>/dev/null || ps -p "$pk" -o args= 2>/dev/null) || pa=; case "$pa" in "" | *"--state-dir $po"*) pd="$po";; esac; fi;; esac;; esac; fi"#;

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
    fn the_depot_is_never_relative() {
        assert_eq!(Env::default().depot(), "/.cache/endeavor/depot:", "no home at all");
        assert_eq!(Env { home: "/home/ada".into(), ..Env::default() }.depot(), "/home/ada/.cache/endeavor/depot:");
        let cache = Path::new("/srv/endeavor");
        let home = server_folders(None, cache);
        assert_eq!((home.depot.as_str(), home.ember.as_path()), ("/srv/endeavor/depot:", Path::new("/srv/endeavor/r/ember")), "a server without $SCRATCH keeps both in its install folder");
        let scratch = server_folders(Some("/scratch/ada"), cache);
        assert_eq!((scratch.depot.as_str(), scratch.ember.as_path()), ("/scratch/ada/endeavor/depot:", Path::new("/scratch/ada/endeavor/r/ember")), "with $SCRATCH, Julia's packages and Ember sit side by side");
        let env = Env { home: "/home/ada".into(), scratch: Some("/scratch/ada".into()), ..Env::default() };
        assert_eq!((env.depot(), env.ember_folder()), (scratch.depot.clone(), scratch.ember.clone()), "serve follows the same rule");
    }

    #[test]
    fn the_users_depot_goes_behind_ours() {
        let home = Path::new("/home/ada");
        let user = |depot, theirs| with_user_depot_from(depot, theirs, home, ':');
        let julia = home.join(".julia").display().to_string();
        assert_eq!(user("/d:", None), format!("/d:{julia}:"), "~/.julia, then Julia's bundled depots");
        assert_eq!(user("/d:", Some("")), format!("/d:{julia}:"));
        assert_eq!(user("/d:", Some("/work/jl:/other")), "/d:/work/jl:", "the first of the user's own");
        assert_eq!(user("/d:", Some("/d:/work/jl")), "/d:/work/jl:", "not ours again (a shell this runtime started)");
        let listed = format!("/d:{julia}:");
        assert_eq!(with_user_depot_from(&listed, None, home, ':'), listed, "already there");
        assert_eq!(user("/d:/e", None), "/d:/e", "a full list is kept");
        assert_eq!(user(":", None), ":");
        assert_eq!(with_user_depot_from("/d:", None, Path::new(""), ':'), "/d:", "no home");
        assert_eq!(with_user_depot_from(r"C:\E\depot;", None, Path::new(r"C:\Users\ada"), ';'), format!(r"C:\E\depot;{};", Path::new(r"C:\Users\ada").join(".julia").display()));
    }

    #[cfg(unix)]
    #[test]
    fn the_bootstrap_script_finds_the_state_folder_this_module_does() {
        use std::process::Command;
        let uname = Command::new("uname").arg("-n").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&uname.stdout).trim_end(), crate::hostname(), "gethostname and `uname -n` name the same host");
        assert!(crate::client::bootstrap_script("v1", false, false, false).contains(PICK_STATE_DIR_SH), "the script holds the shell text that is tested");
        let path = std::env::var("PATH").unwrap();
        let pick = |st: &str, xdg: Option<&str>, launcher: &str| {
            let mut command = Command::new("sh");
            command.arg("-c").arg(format!("{PICK_STATE_DIR_SH}; printf %s \"$pd\"")).env_clear().env("PATH", &path).env("HOME", "/home/ada").env("ln", launcher).env("st", st).env("sd", "/given/state");
            if let Some(xdg) = xdg {
                command.env("XDG_STATE_HOME", xdg);
            }
            String::from_utf8(command.output().unwrap().stdout).unwrap()
        };
        assert_eq!(pick("given", Some("/xdg/state"), "slurm"), "/given/state", "a state folder the client names is used as it is");
        for xdg in [None, Some("/xdg/state"), Some("xdg/state"), Some("")] {
            for launcher in ["process", "slurm"] {
                let read = |name: &str| match name {
                    "HOME" => Some("/home/ada".to_owned()),
                    "XDG_STATE_HOME" => xdg.map(str::to_owned),
                    _ => None,
                };
                let env = Env::from_vars(&read);
                let want = if launcher == "slurm" { env.cluster_state_dir() } else { env.state_dir() };
                assert_eq!(Path::new(&pick("", xdg, launcher)), want, "XDG_STATE_HOME {xdg:?}, {launcher}");
            }
        }

        let home = crate::client::scratch("pick-apps-old-folder");
        let old = home.join(".cache/endeavor/state");
        std::fs::create_dir_all(&old).unwrap();
        let in_home = |launcher: &str| {
            let out = Command::new("sh").arg("-c").arg(format!("{PICK_STATE_DIR_SH}; printf %s \"$pd\"")).env_clear().env("PATH", &path).env("HOME", &home).env("ln", launcher).env("st", "").output().unwrap();
            PathBuf::from(String::from_utf8(out.stdout).unwrap())
        };
        let env = Env { home: home.clone(), node: crate::hostname(), ..Env::default() };
        let record = |node: &str, pid: i64| std::fs::write(old.join("runtime.json"), serde_json::json!({ "launcher": "process", "node": node, "pid": pid, "started": 1, "token": "t", "port": 1 }).to_string()).unwrap();
        let mut core = Command::new("sh").args(["-c", "sleep 60; :", "core", "--state-dir"]).arg(&old).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let core_pid = i64::from(core.id());
        record(&crate::hostname(), core_pid);
        assert_eq!(in_home("process"), old, "alive, of this node");
        assert_eq!(in_home("slurm"), env.cluster_state_dir(), "a cluster's is its own");
        record("another-node", core_pid);
        assert_eq!(in_home("process"), env.state_dir(), "another node's");
        record(&crate::hostname(), std::process::id() as i64);
        assert_eq!(in_home("process"), env.state_dir(), "alive, and not a core of that folder: a reused pid");
        let _ = core.kill();
        let _ = core.wait();
        record(&crate::hostname(), i64::from(i32::MAX));
        assert_eq!(in_home("process"), env.state_dir(), "gone");

        let real_ps = ["/usr/bin/ps", "/bin/ps"].into_iter().find(|p| Path::new(p).exists()).expect("ps");
        let mut core = Command::new("sh").args(["-c", "sleep 60; :", "core", "--state-dir"]).arg(&old).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        record(&crate::hostname(), i64::from(core.id()));
        for (name, body) in [("cuts", format!("[ \"$1\" = -ww ] && exec {real_ps} \"$@\"; {real_ps} \"$@\" | cut -c1-20")), ("refuses-ww", format!("[ \"$1\" = -ww ] && {{ echo 'ps: unknown option' >&2; exit 1; }}; exec {real_ps} \"$@\""))] {
            let fake = home.join(format!("ps-{name}"));
            std::fs::create_dir_all(&fake).unwrap();
            crate::test_exec::write_executable(&fake.join("ps"), &format!("#!/bin/sh\n{body}\n"));
            let out = Command::new("sh").arg("-c").arg(format!("{PICK_STATE_DIR_SH}; printf %s \"$pd\"")).env_clear().env("PATH", format!("{}:{path}", fake.display())).env("HOME", &home).env("ln", "process").env("st", "").output().unwrap();
            assert_eq!(PathBuf::from(String::from_utf8(out.stdout).unwrap()), old, "a ps that {name}");
        }
        let _ = core.kill();
        let _ = core.wait();
    }
}

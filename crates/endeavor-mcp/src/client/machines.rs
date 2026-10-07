//! The machines a runtime can run on besides this computer: a plain server, or
//! a cluster's login node. The JSON is the app's `hosts.json` entry for a
//! server, which this and the app read and write alike.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wire::slurm::{JobRequest, Partition, Resources};

/// How long a notebook may sit idle before the runtime stops it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleStop {
    Hours12,
    Hours24,
    #[default]
    Hours48,
    Week,
    Never,
}

/// A machine reached over SSH: a plain server, where the runtime runs as a
/// detached process, or a cluster's login node, where it runs in a Slurm job.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// Stable across renames; what sessions refer to.
    pub id: String,
    pub name: String,
    /// An alias from `~/.ssh/config`, or `user@host`.
    pub ssh_host: String,
    pub port: Option<u16>,
    /// A julia path, or a shell line such as `module load julia`; unset looks
    /// on the login shell's PATH, then downloads Endeavor's own Julia.
    pub julia: Option<String>,
    pub idle_stop: Option<IdleStop>,
    /// Set for a cluster: Julia runs in a Slurm job.
    pub cluster: Option<Cluster>,
    /// Fields of the record that this version doesn't know, kept when the file is rewritten.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// A cluster's Slurm settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cluster {
    /// The account jobs are charged to; None is the user's default.
    pub account: Option<String>,
    /// What a new session's job asks for.
    pub resources: Resources,
    /// Where Julia keeps packages; None is `$SCRATCH/endeavor/depot` if the
    /// cluster sets `$SCRATCH` (home quotas are small), else ~/.cache/endeavor/depot.
    pub depot: Option<String>,
    /// As the last test of the connection found them.
    pub partitions: Vec<Partition>,
    pub scratch: Option<String>,
}

impl Cluster {
    pub fn partition(&self, name: Option<&str>) -> Option<&Partition> {
        match name {
            Some(name) => self.partitions.iter().find(|p| p.name == name),
            None => self.partitions.iter().find(|p| p.default),
        }
    }

    /// The job a session with `resources` asks for.
    pub fn job(&self, resources: &Resources) -> JobRequest {
        JobRequest { resources: resources.clone(), account: self.account.clone(), depot: self.depot.clone() }
    }
}

impl Server {
    pub fn new_id() -> String {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        format!("server-{nanos:x}")
    }

    /// The helper's launcher and its state folder's name under the install
    /// root. A cluster's folder is its own, so the same machine can also be a
    /// plain server entry without the two sharing a runtime.
    pub fn launcher(&self) -> [String; 2] {
        match &self.cluster {
            None => ["process".into(), "state".into()],
            Some(_) => ["slurm".into(), format!("cluster-{}", self.id)],
        }
    }

    /// Whether a connection made from `other` is the one this record asks for: the same address,
    /// Julia and launcher. The rest of the record (names, job defaults, partitions) doesn't change a connection.
    pub fn same_connection(&self, other: &Server) -> bool {
        self.id == other.id && self.ssh_host == other.ssh_host && self.port == other.port && self.julia == other.julia && self.cluster.is_some() == other.cluster.is_some()
    }

    /// The SSH host as typed: `host`, or `host:port`. An IPv6 address, whose
    /// colons would run into the port, is bracketed (`[::1]:2222`, or
    /// `user@[::1]:2222`).
    pub fn ssh_target(&self) -> String {
        let Some(port) = self.port else { return self.ssh_host.clone() };
        match self.ssh_host.rsplit_once('@').unwrap_or(("", &self.ssh_host)) {
            (user, host) if host.contains(':') => format!("{}[{host}]:{port}", if user.is_empty() { String::new() } else { format!("{user}@") }),
            _ => format!("{}:{port}", self.ssh_host),
        }
    }

    /// Read `ssh_target`'s text back: a trailing `:port` is the port. A host
    /// with more colons (an IPv6 address) is taken whole, and needs brackets to
    /// have a port.
    pub fn parse_target(text: &str) -> Result<(String, Option<u16>), String> {
        let text = text.trim();
        let port = |port: &str| match port.parse::<u16>() {
            Ok(port) if port > 0 => Ok(Some(port)),
            _ => Err(format!("\"{port}\" isn't a port number.")),
        };
        let (user, address) = match text.rsplit_once('@') {
            Some((user, address)) => (format!("{user}@"), address),
            None => (String::new(), text),
        };
        let (host, port) = match address.strip_prefix('[') {
            Some(bracketed) => match bracketed.split_once(']') {
                Some((host, "")) => (format!("{user}{host}"), None),
                Some((host, rest)) if rest.starts_with(':') => (format!("{user}{host}"), port(&rest[1..])?),
                _ => return Err(format!("\"{text}\" isn't an SSH host name.")),
            },
            None => match text.rsplit_once(':') {
                Some((host, p)) if !host.is_empty() && !host.contains(':') => (host.to_owned(), port(p)?),
                _ => (text.to_owned(), None),
            },
        };
        super::ssh::valid_host(&host)?;
        Ok((host, port))
    }

    /// The helper's Julia arguments for this server. A path to a julia binary
    /// (which may hold spaces) is used as is; anything else is a shell line.
    pub fn julia_args(&self) -> [String; 2] {
        let is_path = |j: &str| (j.starts_with('/') || j.starts_with("~/")) && j.rsplit('/').next().is_some_and(|name| name.starts_with("julia"));
        match self.julia.as_deref().map(str::trim).filter(|j| !j.is_empty()) {
            None => ["--julia".into(), "auto".into()],
            Some(path) if is_path(path) => ["--julia".into(), path.into()],
            Some(line) => ["--julia-shell".into(), line.replace('\n', "; ")],
        }
    }
}

/// The machines file's place: `$XDG_CONFIG_HOME/endeavor/machines.json`, by
/// default `~/.config/endeavor/machines.json` (macOS too), and on Windows
/// `%APPDATA%\Endeavor\machines.json`. `var` reads the environment.
pub fn machines_path(var: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let set = |name: &str| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return set("APPDATA").unwrap_or_default().join("Endeavor").join("machines.json");
    }
    let config = crate::xdg::absolute_var(var, "XDG_CONFIG_HOME")
        .or_else(|| crate::xdg::absolute_var(var, "HOME").or_else(|| std::env::home_dir().filter(|h| h.is_absolute())).map(|home| home.join(".config")));
    config.unwrap_or_default().join("endeavor").join("machines.json")
}

/// The one file that lists the machines, which the app reads and writes as well:
/// `{"schema": 1, "machines": [Server, ...]}`. A bare list of records, the shape
/// before the schema number, is read as schema 1 and written as an object.
/// It is written whole to a temporary file that is then renamed, readable by this
/// user only. A file that can't be read or parsed is an error that names it, and is
/// never replaced. Fields this version doesn't know, in the file or in a record,
/// are kept when it is rewritten. A file with a higher schema number is read but
/// never written.
pub struct MachinesFile {
    path: PathBuf,
}

/// The schema number this version reads and writes.
pub const SCHEMA: u64 = 1;

#[derive(Serialize, Deserialize)]
struct Contents {
    #[serde(default = "current_schema")]
    schema: u64,
    machines: Vec<Server>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

fn current_schema() -> u64 {
    SCHEMA
}

impl MachinesFile {
    pub fn at(path: impl Into<PathBuf>) -> MachinesFile {
        MachinesFile { path: path.into() }
    }

    /// The file for this user, from the environment.
    pub fn here() -> MachinesFile {
        MachinesFile::at(machines_path(&|name| std::env::var(name).ok()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every machine, in the order they were added; none if there is no file yet.
    pub fn load(&self) -> Result<Vec<Server>, String> {
        Ok(self.read()?.machines)
    }

    /// An error if the file can't be changed: it can't be read, or a newer Endeavor wrote it.
    pub fn check_writable(&self) -> Result<(), String> {
        self.check_schema(&self.read()?)
    }

    fn check_schema(&self, contents: &Contents) -> Result<(), String> {
        if contents.schema > SCHEMA {
            return Err(format!("A newer Endeavor wrote the list of machines in {} (schema {}; this Endeavor knows {SCHEMA}), so this one doesn't change it. Update Endeavor.", self.path.display(), contents.schema));
        }
        Ok(())
    }

    fn read(&self) -> Result<Contents, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Contents { schema: SCHEMA, machines: Vec::new(), other: Map::new() }),
            Err(e) => return Err(format!("Couldn't read the list of machines in {}: {e}", self.path.display())),
        };
        let invalid = |e: serde_json::Error| format!("The list of machines in {} isn't valid ({e}). Fix or remove the file; Endeavor won't overwrite it.", self.path.display());
        match serde_json::from_str::<Value>(&text).map_err(invalid)? {
            Value::Array(machines) => Ok(Contents { schema: SCHEMA, machines: serde_json::from_value(Value::Array(machines)).map_err(invalid)?, other: Map::new() }),
            other => serde_json::from_value(other).map_err(invalid),
        }
    }

    pub fn find_by_id(&self, id: &str) -> Result<Option<Server>, String> {
        Ok(self.load()?.into_iter().find(|server| server.id == id))
    }

    /// By name, ignoring case.
    pub fn find_by_name(&self, name: &str) -> Result<Option<Server>, String> {
        Ok(self.load()?.into_iter().find(|server| server.name.eq_ignore_ascii_case(name)))
    }

    /// By id, else by name.
    pub fn find(&self, key: &str) -> Result<Option<Server>, String> {
        let servers = self.load()?;
        let by_name = || servers.iter().find(|server| server.name.eq_ignore_ascii_case(key));
        Ok(servers.iter().find(|server| server.id == key).or_else(by_name).cloned())
    }

    /// Add `server`, or replace the one with its id where it stands.
    pub fn save(&self, server: Server) -> Result<(), String> {
        self.change(|servers| match servers.iter_mut().find(|s| s.id == server.id) {
            Some(known) => {
                let other = std::mem::take(&mut known.other);
                *known = Server { other, ..server };
            }
            None => servers.push(server),
        })
        .map(|_| ())
    }

    /// Remove the machine with this id. Whether there was one.
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        self.change(|servers| {
            let before = servers.len();
            servers.retain(|s| s.id != id);
            servers.len() != before
        })
    }

    /// Read, change and write back under a lock, so two processes don't lose each other's change.
    fn change<T>(&self, change: impl FnOnce(&mut Vec<Server>) -> T) -> Result<T, String> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        crate::make_state_dir(dir)?;
        let lock_path = self.path.with_extension("lock");
        let lock = crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
            .open(&lock_path)
            .map_err(|e| format!("Couldn't open {}: {e}", lock_path.display()))?;
        let started = Instant::now();
        while !crate::try_lock(&lock) {
            if started.elapsed() > Duration::from_secs(10) {
                return Err(format!("Another Endeavor process has held {} for too long.", lock_path.display()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut contents = self.read()?;
        self.check_schema(&contents)?;
        let result = change(&mut contents.machines);
        let text = serde_json::to_string_pretty(&contents).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension(format!("json.tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        crate::owner_only(std::fs::OpenOptions::new().write(true).create_new(true))
            .open(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|_| f.sync_all()))
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("Couldn't write the list of machines to {}: {e}", self.path.display())
            })?;
        Ok(result)
    }
}

/// `Host` names in `~/.ssh/config` (and the files it `Include`s by plain
/// path), without patterns.
pub fn ssh_config_hosts() -> Vec<String> {
    let ssh = wire::files::home().join(".ssh");
    let mut hosts = Vec::new();
    collect_hosts(&ssh.join("config"), &ssh, &mut hosts, 0);
    hosts
}

fn collect_hosts(path: &Path, ssh_dir: &Path, hosts: &mut Vec<String>, depth: usize) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        let (keyword, rest) = line.split_once(|c: char| c.is_whitespace() || c == '=').unwrap_or((line, ""));
        let words = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=').split_whitespace();
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                for name in words {
                    if !name.contains(['*', '?', '!']) && !hosts.iter().any(|h| h == name) {
                        hosts.push(name.to_owned());
                    }
                }
            }
            "include" if depth < 4 => {
                for file in words.filter(|f| !f.contains(['*', '?'])) {
                    let file = match file.strip_prefix("~/") {
                        Some(rest) => ssh_dir.parent().unwrap_or(ssh_dir).join(rest),
                        None => ssh_dir.join(file),
                    };
                    collect_hosts(&file, ssh_dir, hosts, depth + 1);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;

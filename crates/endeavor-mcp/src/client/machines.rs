use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wire::slurm::{JobRequest, Partition, Resources};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launcher {
    Process,
    Slurm,
    Auto,
}

impl Launcher {
    pub fn word(self) -> &'static str {
        match self {
            Launcher::Process => "process",
            Launcher::Slurm => "slurm",
            Launcher::Auto => "auto",
        }
    }
}

pub fn valid_id(id: &str) -> Result<(), String> {
    let stem = id.split('.').next().unwrap_or_default();
    let device = matches!(stem, "con" | "prn" | "aux" | "nul") || (stem.len() == 4 && (stem.starts_with("com") || stem.starts_with("lpt")) && stem.ends_with(|c: char| c.is_ascii_digit() && c != '0'));
    let plain = !id.is_empty() && id.len() <= 100 && !id.starts_with('.') && !id.ends_with('.') && !device && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if plain { Ok(()) } else { Err(format!("\"{id}\" isn't a machine id: it has lower-case letters, digits, - _ and . only, doesn't start or end with a dot and isn't a name such as nul or com1.")) }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub ssh_host: String,
    pub port: Option<u16>,
    pub julia: Option<String>,
    pub r: Option<String>,
    pub idle_stop: Option<IdleStop>,
    pub cluster: Option<Cluster>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cluster {
    pub account: Option<String>,
    pub resources: Resources,
    pub depot: Option<String>,
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

    pub fn job(&self, resources: &Resources) -> JobRequest {
        JobRequest { resources: resources.clone(), account: self.account.clone(), depot: self.depot.clone() }
    }
}

impl Server {
    pub fn new_id() -> String {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        format!("server-{nanos:x}")
    }

    pub fn launcher(&self) -> Launcher {
        if self.cluster.is_some() { Launcher::Slurm } else { Launcher::Process }
    }

    pub fn same_connection(&self, other: &Server) -> bool {
        self.id == other.id && self.ssh_host == other.ssh_host && self.port == other.port && self.julia == other.julia && self.r == other.r && self.cluster.is_some() == other.cluster.is_some()
    }

    pub fn display_name(&self) -> String {
        [&self.name, &self.ssh_host, &self.id].into_iter().find(|n| !n.trim().is_empty()).cloned().unwrap_or_default()
    }

    pub fn ssh_target(&self) -> String {
        let Some(port) = self.port else { return self.ssh_host.clone() };
        match self.ssh_host.rsplit_once('@').unwrap_or(("", &self.ssh_host)) {
            (user, host) if host.contains(':') => format!("{}[{host}]:{port}", if user.is_empty() { String::new() } else { format!("{user}@") }),
            _ => format!("{}:{port}", self.ssh_host),
        }
    }

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

    pub fn julia_args(&self) -> [String; 2] {
        let is_path = |j: &str| (j.starts_with('/') || j.starts_with("~/")) && j.rsplit('/').next().is_some_and(|name| name.starts_with("julia"));
        match self.julia.as_deref().map(str::trim).filter(|j| !j.is_empty()) {
            None => ["--julia".into(), "auto".into()],
            Some(path) if is_path(path) => ["--julia".into(), path.into()],
            Some(line) => ["--julia-shell".into(), line.replace('\n', "; ")],
        }
    }

    pub fn r_args(&self) -> [String; 2] {
        let is_path = |r: &str| (r.starts_with('/') || r.starts_with("~/")) && r.rsplit('/').next().is_some_and(|name| name == "R" || name.starts_with("Rscript"));
        match self.r.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            None => ["--r".into(), "auto".into()],
            Some(path) if is_path(path) => ["--r".into(), path.into()],
            Some(line) => ["--r-shell".into(), line.replace('\n', "; ")],
        }
    }
}

pub struct MachinesFile {
    path: PathBuf,
}

pub const SCHEMA: u64 = 1;

#[derive(Serialize, Deserialize)]
struct Contents {
    #[serde(default = "current_schema")]
    schema: u64,
    machines: Vec<Value>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

fn current_schema() -> u64 {
    SCHEMA
}

fn restore_unknown(old: &Value, new: &mut Value) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, old) in old {
                match new.get_mut(key) {
                    Some(new) => restore_unknown(old, new),
                    None => {
                        new.insert(key.clone(), old.clone());
                    }
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for new in new {
                let name = new.get("name").and_then(Value::as_str).map(str::to_owned);
                if let Some(old) = name.and_then(|name| old.iter().find(|o| o.get("name").and_then(Value::as_str) == Some(name.as_str()))) {
                    restore_unknown(old, new);
                }
            }
        }
        _ => {}
    }
}

impl MachinesFile {
    pub fn at(path: impl Into<PathBuf>) -> MachinesFile {
        MachinesFile { path: path.into() }
    }

    pub fn here() -> MachinesFile {
        MachinesFile::at(crate::paths::Env::here().machines_file())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<Server>, String> {
        self.servers(&self.read()?)
    }

    pub fn load_writable(&self) -> Result<Vec<Server>, String> {
        let contents = self.read()?;
        self.check_schema(&contents, "doesn't change it")?;
        self.servers(&contents)
    }

    pub fn load_known(&self) -> Result<Vec<Server>, String> {
        let contents = self.read()?;
        self.check_schema(&contents, "doesn't list it")?;
        self.servers(&contents)
    }

    fn servers(&self, contents: &Contents) -> Result<Vec<Server>, String> {
        contents.machines.iter().map(|machine| serde_json::from_value(machine.clone()).map_err(|e| self.invalid(e))).collect()
    }

    fn invalid(&self, e: serde_json::Error) -> String {
        format!("The list of machines in {} isn't valid ({e}). Fix or remove the file; Endeavor won't overwrite it.", self.path.display())
    }

    fn newer(&self, schema: u64, what: &str) -> String {
        format!("A newer Endeavor wrote the list of machines in {} (schema {schema}; this Endeavor knows {SCHEMA}), so this one {what}. Update Endeavor.", self.path.display())
    }

    fn check_schema(&self, contents: &Contents, what: &str) -> Result<(), String> {
        if contents.schema > SCHEMA {
            return Err(self.newer(contents.schema, what));
        }
        Ok(())
    }

    fn read(&self) -> Result<Contents, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Contents { schema: SCHEMA, machines: Vec::new(), other: Map::new() }),
            Err(e) => return Err(format!("Couldn't read the list of machines in {}: {e}", self.path.display())),
        };
        match serde_json::from_str::<Value>(&text).map_err(|e| self.invalid(e))? {
            Value::Array(machines) => Ok(Contents { schema: SCHEMA, machines, other: Map::new() }),
            value => {
                let newer = value.get("schema").and_then(Value::as_u64).filter(|schema| *schema > SCHEMA);
                let contents: Contents = serde_json::from_value(value).map_err(|e| match newer {
                    Some(schema) => self.newer(schema, "can't read it"),
                    None => self.invalid(e),
                })?;
                Ok(contents)
            }
        }
    }

    pub fn find_by_id(&self, id: &str) -> Result<Option<Server>, String> {
        Ok(self.load()?.into_iter().find(|server| server.id == id))
    }

    pub fn find_by_name(&self, name: &str) -> Result<Option<Server>, String> {
        Ok(self.load()?.into_iter().find(|server| server.name.eq_ignore_ascii_case(name)))
    }

    pub fn find(&self, key: &str) -> Result<Option<Server>, String> {
        let servers = self.load()?;
        let by_name = || servers.iter().find(|server| server.name.eq_ignore_ascii_case(key));
        Ok(servers.iter().find(|server| server.id == key).or_else(by_name).cloned())
    }

    pub fn save(&self, server: Server) -> Result<(), String> {
        self.try_change(|servers| {
            match servers.iter_mut().find(|s| s.id == server.id) {
                Some(known) => *known = server,
                None => servers.push(server),
            }
            Ok(())
        })
    }

    pub fn save_expecting(&self, server: Server, expected: Option<&str>, find: &dyn Fn(&[Server]) -> Option<String>) -> Result<(), String> {
        self.try_change(|servers| {
            let taken = expected.is_none() && servers.iter().any(|s| s.id == server.id);
            if taken || find(servers).as_deref() != expected {
                return Err(format!("The list of machines in {} changed while Endeavor was connecting (another session or the app changed it), so nothing was saved. Call `add_machine` again.", self.path.display()));
            }
            match servers.iter_mut().find(|s| s.id == server.id) {
                Some(known) => *known = server,
                None => servers.push(server),
            }
            Ok(())
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool, String> {
        self.try_change(|servers| {
            let before = servers.len();
            servers.retain(|s| s.id != id);
            Ok(servers.len() != before)
        })
    }

    fn try_change<T>(&self, change: impl FnOnce(&mut Vec<Server>) -> Result<T, String>) -> Result<T, String> {
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
        self.check_schema(&contents, "doesn't change it")?;
        let mut servers = self.servers(&contents)?;
        let result = change(&mut servers)?;
        contents.machines = servers
            .iter()
            .map(|server| {
                let mut record = serde_json::to_value(server).map_err(|e| e.to_string())?;
                if let Some(old) = contents.machines.iter().find(|old| old.get("id").and_then(Value::as_str) == Some(server.id.as_str())) {
                    restore_unknown(old, &mut record);
                }
                Ok(record)
            })
            .collect::<Result<_, String>>()?;
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

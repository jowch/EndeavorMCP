//! The machines a runtime can run on besides this computer: a plain server, or
//! a cluster's login node. The JSON is the app's `hosts.json` entry for a
//! server, which this and the app read and write alike.

use std::path::Path;

use serde::{Deserialize, Serialize};
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
        let (user, address) = match text.split_once('@') {
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

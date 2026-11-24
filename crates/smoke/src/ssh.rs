use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Server {
    sshd: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

pub struct Folders {
    pub home: PathBuf,
    pub root: PathBuf,
    pub state: PathBuf,
    pub remote: PathBuf,
    pub bin: PathBuf,
}

impl Folders {
    pub fn of(work: &Path) -> Folders {
        Folders { home: work.join("ssh-home"), root: work.join("server/install"), state: work.join("server/state"), remote: work.join("server/notebooks"), bin: work.join("ssh/bin") }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn keygen(path: &Path) -> Result<(), String> {
    let status = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(path).stdout(Stdio::null()).status().map_err(|e| format!("ssh-keygen: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("ssh-keygen failed for {}", path.display())) }
}

pub fn start(work: &Path, without_uv: bool) -> Result<Server, String> {
    let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].iter().map(PathBuf::from).find(|p| p.is_file()).ok_or("no sshd on this computer (install openssh-server)")?;
    let dir = work.join("ssh");
    let folders = Folders::of(work);
    let ssh = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(|p| p.join("ssh")).find(|p| p.is_file()).ok_or("no ssh on the PATH (install openssh-client)")?;
    for d in [&dir, &folders.home.join(".ssh"), &folders.root, &folders.state, &folders.remote, &folders.bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    keygen(&dir.join("host_key"))?;
    keygen(&dir.join("client_key"))?;
    std::fs::copy(dir.join("client_key.pub"), dir.join("authorized_keys")).unwrap();
    let user = match std::env::var("USER").or_else(|_| std::env::var("LOGNAME")) {
        Ok(user) if !user.is_empty() => user,
        _ => {
            let out = Command::new("id").arg("-un").output().map_err(|e| format!("id -un: {e}"))?;
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
    };
    if user.is_empty() { return Err("couldn't tell this account's user name".into()); }
    let (port, closed) = (free_port(), free_port());
    std::fs::write(
        dir.join("sshd_config"),
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nAuthorizedKeysFile {d}/authorized_keys\nPidFile {d}/sshd.pid\n\
             StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin prohibit-password\nAllowUsers {user}\n",
            d = dir.display()
        ),
    )
    .unwrap();
    if without_uv {
        let set_env = server_env_without_uv(&work.join("server/home"))?;
        let mut config = std::fs::OpenOptions::new().append(true).open(dir.join("sshd_config")).unwrap();
        std::io::Write::write_all(&mut config, format!("SetEnv {set_env}\n").as_bytes()).unwrap();
    }
    let host_key = std::fs::read_to_string(dir.join("host_key.pub")).unwrap();
    std::fs::write(dir.join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).unwrap();
    std::fs::write(
        folders.home.join(".ssh/config"),
        format!(
            "Host smoke-host\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {d}/client_key\n  IdentitiesOnly yes\n  UserKnownHostsFile {d}/known_hosts\n  StrictHostKeyChecking yes\n\n\
             Host gone-host\n  HostName 127.0.0.1\n  Port {closed}\n  User {user}\n  IdentityFile {d}/client_key\n  IdentitiesOnly yes\n  UserKnownHostsFile {d}/known_hosts\n  StrictHostKeyChecking yes\n  ConnectTimeout 5\n",
            d = dir.display()
        ),
    )
    .unwrap();
    let wrapper = folders.bin.join("ssh");
    std::fs::write(&wrapper, format!("#!/bin/sh\nexec '{}' -F '{}' \"$@\"\n", ssh.display(), folders.home.join(".ssh/config").display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = std::fs::File::create(dir.join("sshd.log")).unwrap();
    let child = Command::new(sshd).args(["-D", "-e", "-f"]).arg(dir.join("sshd_config")).stdout(Stdio::null()).stderr(log).spawn().map_err(|e| format!("sshd: {e}"))?;
    let server = Server { sshd: child };
    let began = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        if began.elapsed() > Duration::from_secs(10) {
            return Err(format!("sshd didn't listen on {port}: {}", std::fs::read_to_string(dir.join("sshd.log")).unwrap_or_default()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(server)
}

const PASSED_TO_THE_SERVER: [&str; 6] = ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy", "SSL_CERT_FILE", "UV_NATIVE_TLS"];

const WHERE_ENDEAVOR_ALSO_LOOKS_FOR_UV: [&str; 2] = ["/usr/local/bin", "/opt/homebrew/bin"];

fn server_env_without_uv(home: &Path) -> Result<String, String> {
    std::fs::create_dir_all(home).unwrap();
    let outer = std::env::var_os("PATH").unwrap_or_default();
    let path: Vec<PathBuf> = std::env::split_paths(&outer).filter(|dir| !dir.join("uv").is_file()).collect();
    if let Some(uv) = WHERE_ENDEAVOR_ALSO_LOOKS_FOR_UV.iter().map(|dir| Path::new(dir).join("uv")).find(|uv| uv.is_file()) {
        return Err(format!("server_without_uv: {} is where Endeavor looks for uv on any server, so the server can't be without one here", uv.display()));
    }
    let path = std::env::join_paths(path).map_err(|e| format!("PATH: {e}"))?;
    let mut set = vec![format!("HOME={}", home.display()), format!("PATH={}", path.to_string_lossy())];
    set.extend(PASSED_TO_THE_SERVER.iter().filter_map(|name| std::env::var(name).ok().map(|value| format!("{name}={value}"))));
    Ok(set.iter().map(|entry| format!("\"{entry}\"")).collect::<Vec<_>>().join(" "))
}

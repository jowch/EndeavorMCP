use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--version") {
        println!("julia version 1.12.0");
        return;
    }
    if args.iter().any(|a| a == "-e") {
        let exe = std::env::current_exe().unwrap();
        print!("{}", exe.parent().unwrap().display());
        return;
    }
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let dir = PathBuf::from(env("STAND_IN_DIR"));
    let pid = std::process::id();
    std::fs::write(dir.join("julia.args"), format!("{}\n", args.join(" "))).unwrap();
    std::fs::write(dir.join("engine_token"), env("ENDEAVOR_TOKEN")).unwrap();
    let mut pids = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("julia.pids")).unwrap();
    writeln!(pids, "{pid}").unwrap();
    println!("booting");
    while dir.join("hold").exists() {
        std::thread::sleep(Duration::from_millis(100));
    }
    let node = std::process::Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default();
    let state = serde_json::json!({
        "launcher": env("ENDEAVOR_LAUNCHER"), "node": node, "pid": pid,
        "pluto_port": env("STAND_IN_PLUTO_PORT").parse::<u16>().unwrap(), "mcp_port": env("STAND_IN_MCP_PORT").parse::<u16>().unwrap(),
        "pluto_secret": "s3cret", "job": env("SLURM_JOB_ID"),
    });
    let path = PathBuf::from(env("ENDEAVOR_STATE"));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, state.to_string()).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    std::thread::sleep(Duration::from_secs(600));
}

//! A stand-in for Julia under `endeavor core` on Windows, where the tests' `#!/bin/sh` one
//! (`tests/common/mod.rs`, `serving_julia`) can't run. The Windows tests (`tests/windows.rs`) copy
//! it to `julia.exe`. Like the shell one, it says it's 1.12, records its arguments, the token it was
//! given and its pid in `STAND_IN_DIR`, writes the state boot.jl would, naming the test's stand-in
//! bridge and Pluto (`STAND_IN_MCP_PORT`, `STAND_IN_PLUTO_PORT`), and stays up. It waits first while
//! the file `hold` is in `STAND_IN_DIR`. Asked for `Sys.BINDIR` (`-e`), it names its own folder, so the
//! core's look for the real julia.exe behind a launcher finds this one. Not part of any build that ships.

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

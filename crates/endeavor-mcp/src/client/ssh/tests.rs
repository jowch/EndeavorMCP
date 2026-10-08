use super::*;

fn args(command: &Command) -> Vec<String> {
    command.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn lab() -> Transport {
    Transport::Ssh { host: "lab".into(), port: None }
}

fn no_helper_here(os: &str, arch: &str) -> Result<PathBuf, String> {
    Err(no_helper(os, arch))
}

fn options(root: &str, state: &str, depot: &str) -> Options<'static> {
    Options { auth: Auth::Batch, root: root.into(), state: state.into(), depot: depot.into(), exit_idle: false, allow_install: true, helper: &no_helper_here, launcher: None }
}

#[test]
fn batch_mode_adds_batchmode_and_env_does_not() {
    let batch = lab().command("true", &Auth::Batch).unwrap();
    let args_batch = args(&batch);
    assert_eq!(batch.get_program(), "ssh");
    assert!(args_batch.windows(2).any(|w| w == ["-o", "BatchMode=yes"]), "{args_batch:?}");
    assert!(batch.get_envs().next().is_none());

    let env = Auth::Env(vec![("SSH_ASKPASS".into(), "/x/askpass".into()), ("SSH_ASKPASS_REQUIRE".into(), "force".into())]);
    let asking = lab().command("true", &env).unwrap();
    let args_env = args(&asking);
    assert!(!args_env.iter().any(|a| a.contains("BatchMode")), "{args_env:?}");
    let set: Vec<_> = asking.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
    assert_eq!(set, [("SSH_ASKPASS".to_owned(), Some("/x/askpass".to_owned())), ("SSH_ASKPASS_REQUIRE".to_owned(), Some("force".to_owned()))]);
}

#[test]
fn ssh_gets_its_usual_arguments_and_the_script_last() {
    let command = Transport::Ssh { host: "jc@lab".into(), port: Some(2222) }.command("echo hi", &Auth::Batch).unwrap();
    assert_eq!(
        args(&command),
        ["-T", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=2", "-o", "ConnectTimeout=20", "-o", "ForwardX11=no", "-o", "BatchMode=yes", "-p", "2222", "--", "jc@lab", "sh -c 'echo hi'"]
    );
}

#[test]
fn a_host_that_is_not_a_host_never_reaches_ssh() {
    for bad in ["", "-oProxyCommand=x", "-p", "lab host", "lab;rm", "lab\nx", "$(x)", "a'b", "lab/", "lab*", "jc@", "@lab", "@", ":22", "jc@:22", "lab:22"] {
        assert!(valid_host(bad).is_err(), "{bad:?}");
        assert!(Transport::Ssh { host: bad.into(), port: None }.command("true", &Auth::Batch).is_err(), "{bad:?}");
    }
    for good in ["lab", "jc@lab.example.edu", "10.0.0.2", "fe80::1", "::1", "jc@fe80::1", "a@b@lab", "hoffman2_login-1"] {
        assert!(valid_host(good).is_ok(), "{good:?}");
    }
    assert_eq!(valid_host("-x").unwrap_err(), "\"-x\" isn't an SSH host name.");
}

#[test]
fn the_bootstrap_holds_nothing_a_login_shell_would_change() {
    for exit_idle in [false, true] {
        let script = bootstrap_script(crate::embedded::BUILD_VERSION, exit_idle);
        for bad in ['\'', '\\', '!', '\n'] {
            assert!(!script.contains(bad), "{bad:?} in {script}");
        }
    }
}

/// `script` run as a login shell would run ssh's command, with `preamble` on stdin.
#[cfg(unix)]
fn run_script(script: &str, preamble: &str, home: &Path) -> String {
    run_script_in(script, preamble, home, &[])
}

/// `run_script`, with more variables set (a `PATH`, for one).
#[cfg(unix)]
fn run_script_in(script: &str, preamble: &str, home: &Path, env: &[(&str, &str)]) -> String {
    let mut shell = Command::new("sh").arg("-c").arg(format!("sh -c '{script}'")).env("HOME", home).envs(env.iter().copied()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    shell.stdin.take().unwrap().write_all(preamble.as_bytes()).unwrap();
    let out = shell.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The shells that stand for the server's `sh` here: dash, bash and busybox's, whichever are installed.
#[cfg(unix)]
fn shells() -> Vec<(&'static str, PathBuf)> {
    let found: Vec<_> = ["dash", "bash", "busybox"].into_iter().filter_map(|name| ["/usr/bin", "/bin"].iter().map(|dir| Path::new(dir).join(name)).find(|path| path.exists()).map(|path| (name, path))).collect();
    eprintln!("the bootstrap runs under: {}", found.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", "));
    found
}

/// A `PATH` folder where `sh` is `shell`, the tools the script uses are the system's, and `squeue` lists
/// `listed` (is absent for `None`), so that the real scheduler isn't asked. `ps` is the system's unless `ps` says otherwise.
#[cfg(unix)]
fn server_path(dir: &Path, shell: &Path, listed: Option<&str>, ps: Option<&str>) -> String {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    std::os::unix::fs::symlink(shell, dir.join("sh")).unwrap();
    for tool in ["cat", "tr", "cut", "uname", "id", "grep", "ps", "rm", "mkdir", "head", "tar", "mv", "sleep"] {
        let found = ["/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(tool)).find(|path| path.exists()).expect(tool);
        std::os::unix::fs::symlink(found, dir.join(tool)).ok();
    }
    let script = |name: &str, body: &str| {
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    if let Some(listed) = listed {
        script("squeue", &format!("printf '%s\\n' {listed}"));
    }
    if let Some(ps) = ps {
        script("ps", ps);
    }
    dir.display().to_string()
}

#[test]
#[cfg(unix)]
fn the_bootstrap_runs_under_a_shell_and_asks_for_an_install() {
    for (name, shell) in shells() {
        let home = crate::client::scratch(&format!("bootstrap-need-{name}"));
        let path = server_path(&home.join("bin"), &shell, None, None);
        let said = run_script_in(&bootstrap_script("v1", false), "\n\n\n--julia\nauto\nprocess\n", &home, &[("PATH", &path)]);
        assert!(said.starts_with("ENDEAVOR ") && said.trim_end().contains(" need none first ") && said.trim_end().ends_with("/.cache/endeavor/v1"), "{said}");
        assert!(!home.join(".cache").exists());
    }
}

/// A `v1` install under `root` whose `endeavor` says how it was run.
#[cfg(unix)]
fn fake_install(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let dir = root.join("v1");
    std::fs::create_dir_all(dir.join("runtime")).unwrap();
    std::fs::write(dir.join("runtime/boot.jl"), "").unwrap();
    let helper = dir.join("endeavor");
    std::fs::write(&helper, "#!/bin/sh\nfor a in \"$@\"; do echo \"arg:$a\"; done\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The `connect` arguments the script gives the helper, as lines.
#[cfg(unix)]
fn connect_args(home: &Path, preamble: &str) -> Vec<String> {
    let said = run_script(&bootstrap_script("v1", false), preamble, home);
    let mut lines = said.lines();
    assert!(lines.next().is_some_and(|l| l.starts_with("ENDEAVOR ") && l.ends_with(" have")), "{said}");
    lines.map(|l| l.strip_prefix("arg:").unwrap_or(l).to_owned()).collect()
}

#[test]
#[cfg(unix)]
fn root_state_and_depot_land_where_given() {
    let home = crate::client::scratch("bootstrap-where");
    let root = home.join("my root with spaces");
    fake_install(&root);
    let root = root.display();
    let args = connect_args(&home, &format!("{root}\nstate-a\n/depots/mine:\n--julia\n/opt/julia\nslurm\n"));
    assert_eq!(
        args,
        ["connect", "--state-dir", &format!("{root}/state-a"), "--launcher", "slurm", "--julia", "/opt/julia", "--runtime", &format!("{root}/v1/runtime"), "--depot", "/depots/mine:", "--build", "v1"]
    );
    let args = connect_args(&home, &format!("{root}\n/var/endeavor/state\n\n--julia\nauto\nprocess\n"));
    assert_eq!(args[2], "/var/endeavor/state", "an absolute state folder is used as it is");
    assert_eq!(args[10], format!("{root}/depot:"), "no depot means one in the install folder");
}

#[test]
#[cfg(unix)]
fn exit_idle_reaches_the_helper_with_the_same_six_lines() {
    let home = crate::client::scratch("bootstrap-exit-idle");
    fake_install(&home.join("root"));
    let preamble = format!("{}/root\nstate\n/d:\n--julia\nauto\nprocess\n", home.display());
    let args_of = |exit_idle| run_script(&bootstrap_script("v1", exit_idle), &preamble, &home).lines().skip(1).map(str::to_owned).collect::<Vec<_>>();
    assert!(!args_of(false).iter().any(|a| a == "arg:--exit-idle"));
    let with = args_of(true);
    let at = with.iter().position(|a| a == "arg:--exit-idle").expect("--exit-idle is passed");
    assert_eq!(with[at + 1], "arg:--launcher", "{with:?}");
    assert_eq!(with.len(), args_of(false).len() + 1);
}

/// What the script says about a server with no helper, with `launcher` and the state folder given.
#[cfg(unix)]
fn needs_line(home: &Path, state: &Path, launcher: &str, path: &str) -> String {
    let mut shell = Command::new("sh").arg("-c").arg(format!("sh -c '{}'", bootstrap_script("v1", false))).env("HOME", home).env("PATH", path).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    // It waits for the byte count, and the end of its input ends it.
    shell.stdin.take().unwrap().write_all(format!("{}/root\n{}\n\n--julia\nauto\n{launcher}\n", home.display(), state.display()).as_bytes()).unwrap();
    String::from_utf8_lossy(&shell.wait_with_output().unwrap().stdout).into_owned()
}

/// A stand-in process that is ended when it is dropped, whatever a test did before.
#[cfg(unix)]
struct KillOnDrop(std::process::Child);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[cfg(unix)]
fn a_server_without_the_helper_is_described_and_nothing_is_written() {
    for (name, shell) in shells() {
        let home = crate::client::scratch(&format!("bootstrap-describe-{name}"));
        let state = home.join("st");
        std::fs::create_dir_all(&state).unwrap();
        let bin = |listed: Option<&str>, ps: Option<&str>| server_path(&home.join("bin"), &shell, listed, ps);
        // A process whose command is a core's, a quoted node name that holds "pid", and a recorded job.
        let core = KillOnDrop(std::os::unix::process::CommandExt::arg0(Command::new("sleep").arg("60"), "endeavor core --state-dir fake").spawn().unwrap());
        let pid = core.0.id();
        let write = |text: String| std::fs::write(state.join("runtime.json"), text).unwrap();
        write(format!(r#"{{"launcher":"process","node":"rapid-pid1","pid":{pid},"port":5,"token":"t"}}"#));
        let line = needs_line(&home, &state, "process", &bin(None, None));
        // Busybox's own `ps` has no -p, so it can only say that the process is alive.
        let seen = if name == "busybox" { "process-recorded" } else { "process" };
        assert!(line.contains(&format!(" need {seen}:{pid} first {}/root/v1", home.display())), "{name}: {line}");
        assert!(needs_line(&home, &state, "slurm", &bin(None, None)).contains(" need none first "), "{name}: a plain runtime isn't a job");
        // A `ps` that can't say: alive, and recorded only. One that says another command: not it.
        assert!(needs_line(&home, &state, "process", &bin(None, Some("exit 1"))).contains(&format!(" need process-recorded:{pid} first ")), "{name}");
        if name != "busybox" {
            assert!(needs_line(&home, &state, "process", &bin(None, Some("echo sleep 60"))).contains(" need none first "), "{name}: a pid that was reused");
        }
        drop(core);
        write(format!(r#"{{"launcher":"process","node":"n","pid":{pid},"port":5,"token":"t"}}"#));
        assert!(needs_line(&home, &state, "process", &bin(None, None)).contains(" need none "), "{name}: gone");

        std::fs::write(state.join("job.json"), r#"{"job":"77","summary":"x"}"#).unwrap();
        assert!(needs_line(&home, &state, "slurm", &bin(Some("5 77"), None)).contains(" need job:77 "), "{name}");
        assert!(needs_line(&home, &state, "slurm", &bin(Some("5 6"), None)).contains(" need none "), "{name}: not listed any more");
        assert!(needs_line(&home, &state, "slurm", &bin(None, None)).contains(" need job-recorded:77 "), "{name}: no squeue");
        std::fs::remove_file(state.join("job.json")).unwrap();
        write(r#"{"launcher":"slurm","job":null,"pid":5,"token":"t"}"#.into());
        assert!(needs_line(&home, &state, "slurm", &bin(Some("5"), None)).contains(" need none "));
        write(r#"{"launcher":"slurm","job":"123","pid":5,"token":"t"}"#.into());
        assert!(needs_line(&home, &state, "slurm", &bin(Some("123"), None)).contains(" need job:123 "));
        assert!(!home.join("root").exists(), "nothing was written");
    }
}

#[test]
#[cfg(unix)]
fn ids_that_are_not_digits_are_not_reported_and_values_are_never_read_as_escapes() {
    for (name, shell) in shells() {
        let home = crate::client::scratch(&format!("bootstrap-values-{name}"));
        let state = home.join("st");
        std::fs::create_dir_all(&state).unwrap();
        let path = server_path(&home.join("bin"), &shell, Some("77"), None);
        for text in [r#"{"pid":12ab,"x":1}"#, r#"{"pid":1;2}"#, r#"{"pid":$(touch pwned)}"#] {
            std::fs::write(state.join("runtime.json"), text).unwrap();
            assert!(needs_line(&home, &state, "process", &path).contains(" need none "), "{name}: {text}");
        }
        for text in [r#"{"job":"7x7"}"#, r#"{"job":"7 7"}"#, r#"{"job":"77;x"}"#] {
            std::fs::write(state.join("job.json"), text).unwrap();
            assert!(needs_line(&home, &state, "slurm", &path).contains(" need none "), "{name}: {text}");
        }
        assert!(!home.join("pwned").exists() && !state.join("pwned").exists());
        // `\c` ends an echo's output in dash and busybox, and `\n` is a line break there: the line is printed as it is.
        for odd in [r"x\cy", r"a\nb", "with space", "$HOME"] {
            let root = home.join(odd);
            let mut sh = Command::new("sh").arg("-c").arg(format!("sh -c '{}'", bootstrap_script("v1", false))).env("HOME", &home).env("PATH", &path).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            sh.stdin.take().unwrap().write_all(format!("{}\n\n\n--julia\nauto\nprocess\n", root.display()).as_bytes()).unwrap();
            let said = String::from_utf8_lossy(&sh.wait_with_output().unwrap().stdout).into_owned();
            assert_eq!(said.lines().count(), 1, "{name}: {said:?}");
            assert!(said.ends_with(&format!(" first {}/v1\n", root.display())), "{name}: {said:?}");
        }
    }
}

#[test]
#[cfg(unix)]
fn an_empty_state_folder_leaves_the_flag_out() {
    let home = crate::client::scratch("bootstrap-default-state");
    fake_install(&home.join("root"));
    let args = connect_args(&home, &format!("{}/root\n\n/d:\n--julia\nauto\nprocess\n", home.display()));
    assert_eq!(args[..2], ["connect", "--launcher"], "{args:?}");
    assert!(!args.iter().any(|a| a == "--state-dir"), "{args:?}");
}

#[test]
#[cfg(unix)]
fn an_empty_root_means_the_cache_folder_in_home() {
    let home = crate::client::scratch("bootstrap-home");
    let cache = crate::paths::Env::from_vars(&|name| (name == "HOME").then(|| home.display().to_string())).server_root();
    fake_install(&cache);
    let args = connect_args(&home, "\nstate\n\n--julia\nauto\nprocess\n");
    assert_eq!((args[2].as_str(), args[8].as_str(), args[10].as_str()), (cache.join("state").to_str().unwrap(), cache.join("v1/runtime").to_str().unwrap(), cache.join("depot:").to_str().unwrap()));
}

#[test]
fn the_preamble_carries_the_parameters_not_the_script() {
    let server = Server { julia: Some("module load julia".into()), ..Default::default() };
    let sent = String::from_utf8(preamble(&server, &options("/opt/e", "st", "/d:")).unwrap()).unwrap();
    assert_eq!(sent, "/opt/e\nst\n/d:\n--julia-shell\nmodule load julia\nprocess\n");
    let cluster = Server { id: "c1".into(), cluster: Some(Default::default()), ..Default::default() };
    assert_eq!(String::from_utf8(preamble(&cluster, &options("", "mine", "")).unwrap()).unwrap(), "\nmine\n\n--julia\nauto\nslurm\n");
}

#[test]
fn a_line_break_in_a_parameter_is_refused() {
    let server = Server::default();
    for (root, state, depot) in [("a\nb", "s", ""), ("", "s\nt", ""), ("", "s", "x\ny")] {
        let err = preamble(&server, &options(root, state, depot)).unwrap_err();
        assert!(err.contains("can't hold a line break"), "{err}");
    }
    assert!(preamble(&server, &options("", "", "")).is_ok(), "an empty state folder is the helper's default");
    let err = connect(&server, &Transport::Shell { env: Vec::new(), ask: None }, &options("a\nb", "s", ""), &Cancel::default(), &|_| {}).err().expect("refused").message;
    assert!(err.contains("install folder"), "{err}");
}

#[test]
#[cfg(unix)]
fn tar_holds_the_helper_and_runtime() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = crate::client::scratch("tar");
    let helper = tmp.join("fake-helper");
    std::fs::write(&helper, "#!/bin/sh\necho hi\n").unwrap();
    let tar = install_tar(&helper).unwrap();
    assert_eq!(tar.len() % 512, 0);
    let out = tmp.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut untar = Command::new("tar").arg("xf").arg("-").current_dir(&out).stdin(Stdio::piped()).spawn().unwrap();
    untar.stdin.take().unwrap().write_all(&tar).unwrap();
    assert!(untar.wait().unwrap().success());
    let unpacked = out.join("endeavor");
    assert_eq!(std::fs::read_to_string(&unpacked).unwrap(), "#!/bin/sh\necho hi\n");
    assert_eq!(std::fs::metadata(&unpacked).unwrap().permissions().mode() & 0o777, 0o755);
    for (path, contents, _) in runtime_files() {
        assert_eq!(std::fs::read(out.join(&path)).unwrap(), contents, "{path}");
    }
}

#[test]
fn a_missing_helper_file_is_named() {
    let err = install_tar(Path::new("/no/such/helper")).unwrap_err();
    assert!(err.starts_with("/no/such/helper: "), "{err}");
}

#[test]
fn long_paths_use_the_prefix_field() {
    let mut tar = Vec::new();
    let long = format!("{}/{}", "d".repeat(120), "f".repeat(60));
    tar_entry(&mut tar, &long, b"x", 0o644, b'0').unwrap();
    assert_eq!(&tar[..60], "f".repeat(60).as_bytes());
    assert_eq!(&tar[345..465], "d".repeat(120).as_bytes());
    assert!(tar_entry(&mut Vec::new(), &"x".repeat(300), b"", 0o644, b'0').is_err());
}

#[test]
fn the_platform_is_named_as_helper_folders_are() {
    assert_eq!(platform("Linux", "x86_64"), ("linux".to_owned(), "x86_64".to_owned()));
    assert_eq!(platform("Darwin", "arm64"), ("darwin".to_owned(), "aarch64".to_owned()));
    assert_eq!(no_helper("linux", "riscv64"), "Endeavor has no runtime helper for linux riscv64 servers.");
}

fn say_in(auth: &Auth, line: &str) -> String {
    explain(&Transport::Ssh { host: "lab".into(), port: None }, auth, &[line.to_owned()], None, false, false)
}

#[test]
fn explains_ssh_failures_plainly() {
    let ask = Auth::Env(Vec::new());
    let say = |line: &str| say_in(&ask, line);
    assert!(say("ssh: Could not resolve hostname lab: nodename nor servname provided").contains("Couldn't find a server called lab"));
    assert!(say("jc@lab: Permission denied (publickey,password).").contains("refused the sign-in. Check the user name"));
    assert!(say("ssh: connect to host lab port 22: Operation timed out").contains("timed out"));
    assert!(say("Host key verification failed.").contains("wasn't confirmed"));
    assert!(say("ssh: connect to host lab port 22: Connection refused").contains("refused the connection"));
    assert!(say("ssh: connect to host lab port 22: No route to host").contains("Couldn't reach lab"));
    assert!(say("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@").contains("ssh-keygen -R lab"));
    assert_eq!(say("bash: line 1: sh: command not found"), "The connection to lab ended: bash: line 1: sh: command not found");
    assert_eq!(explain(&lab(), &ask, &[], None, true, false), "Cancelled.");
    assert_eq!(explain(&lab(), &ask, &[], None, false, false), "The connection to lab ended before Endeavor could start.");
}

#[test]
fn in_batch_mode_a_sign_in_failure_says_what_to_do() {
    let say = |line: &str| say_in(&Auth::Batch, line);
    let host_key = say("Host key verification failed.");
    assert!(host_key.contains("Run `ssh lab` once in a terminal and accept its host key"), "{host_key}");
    let denied = say("jc@lab: Permission denied (publickey).");
    assert!(denied.contains("ssh-add") && denied.contains("password or a code, which Endeavor can't ask for yet"), "{denied}");
    // The same words for a password server: ssh lists its methods.
    assert_eq!(say("jc@lab: Permission denied (publickey,password,keyboard-interactive)."), denied);
    let with_port = explain(&Transport::Ssh { host: "lab".into(), port: Some(2222) }, &Auth::Batch, &["Host key verification failed.".to_owned()], None, false, false);
    assert!(with_port.contains("`ssh -p 2222 lab`"), "{with_port}");
    // The rest reads the same as without batch mode.
    assert!(say("ssh: connect to host lab port 22: Operation timed out").contains("timed out"));
}

#[test]
fn after_sign_in_the_servers_own_words_are_shown() {
    let said = |lines: &[&str]| explain(&lab(), &Auth::Batch, &lines.iter().map(|l| l.to_string()).collect::<Vec<_>>(), None, false, true);
    assert_eq!(
        said(&["mkdir: cannot create directory '/opt/x': Permission denied", "Endeavor: installing into /opt/x/v failed"]),
        "The connection to lab ended: Endeavor: installing into /opt/x/v failed"
    );
    assert_eq!(said(&["tar: write error: Connection timed out"]), "The connection to lab ended: tar: write error: Connection timed out");
    assert_eq!(said(&[]), "The connection to lab ended before Endeavor could start.");
    assert_eq!(explain(&lab(), &Auth::Batch, &[], None, true, true), "Cancelled.");
}

#[cfg(unix)]
fn exited_with(code: i32) -> Option<ExitStatus> {
    Some(std::os::unix::process::ExitStatusExt::from_raw(code << 8))
}

#[test]
#[cfg(unix)]
fn only_ssh_own_failures_are_explained_as_sign_in_or_network_problems() {
    let denied = ["jc@lab: Permission denied (publickey).".to_owned()];
    let says = |status: Option<ExitStatus>, signed_in: bool| explain(&lab(), &Auth::Batch, &denied, status, false, signed_in);
    // ssh's own status, or none known (killed, or not asked).
    for status in [exited_with(255), None, Some(std::os::unix::process::ExitStatusExt::from_raw(9))] {
        assert!(says(status, false).contains("refused the sign-in"), "{status:?}");
    }
    // Any other status is the remote command's: its own words.
    assert_eq!(says(exited_with(1), false), "The connection to lab ended: jc@lab: Permission denied (publickey).");
    assert_eq!(says(exited_with(0), false), says(exited_with(1), false));
    // And never once signed in, whatever the status.
    assert_eq!(says(exited_with(255), true), "The connection to lab ended: jc@lab: Permission denied (publickey).");
    assert_eq!(explain(&lab(), &Auth::Batch, &[], exited_with(3), false, false), "The connection to lab ended before Endeavor could start (exit status: 3).");
}

#[test]
fn the_collected_stderr_survives_a_line_that_is_not_utf8() {
    let stderr = Stderr::collect(&b"caf\xe9 banner\r\nssh: Permission denied (publickey).\nlast, no newline"[..]);
    assert_eq!(stderr.finish(), ["caf\u{fffd} banner", "ssh: Permission denied (publickey).", "last, no newline"]);
}

#[test]
#[cfg(unix)]
fn a_cancel_holds_ssh_while_the_helper_lives_and_lets_go_once_it_has_exited() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::client::scratch("cancel-pid");
    std::fs::write(dir.join("frames"), ToApp::Hello { protocol: wire::PROTOCOL, version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: false, launcher: String::new() }.frame().encode()).unwrap();
    // Says hello, then stays until the client sends it anything (a detach).
    let script = dir.join("helper");
    std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\nhead -c 1 >/dev/null\n", dir.join("frames").display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake = |_: &str, _: &str| Ok(script.clone());
    let options = Options { helper: &fake, root: dir.join("root").display().to_string(), ..options("", "", "") };
    let transport = Transport::Shell { env: vec![("HOME".into(), dir.display().to_string())], ask: None };
    let cancel = Cancel::default();
    let (channel, _) = connect(&Server::default(), &transport, &options, &cancel, &|_| {}).unwrap();
    assert!(cancel.pid.lock().unwrap().is_some(), "after the connect, for as long as the helper lives");
    channel.detach();
    assert!(cancel.pid.lock().unwrap().is_none(), "not after it has exited");
    cancel.cancel();
}

#[test]
#[cfg(unix)]
fn a_helper_of_another_protocol_is_refused_at_once_and_not_retried() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::client::scratch("other-protocol");
    let with_protocol = ToApp::Hello { protocol: 0, version: "0".into(), node: "n".into(), home: "/".into(), slurm: false, uploads: false, launcher: String::new() }.frame().encode();
    let without = wire::Frame::Control(br#"{"type":"Hello","version":"0","node":"n","home":"/"}"#.to_vec()).encode();
    for (name, hello) in [("says 0", with_protocol), ("says none", without)] {
        std::fs::write(dir.join("frames"), hello).unwrap();
        let script = dir.join("helper");
        std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\nhead -c 1 >/dev/null\n", dir.join("frames").display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = |_: &str, _: &str| Ok(script.clone());
        let options = Options { helper: &fake, root: dir.join("root").display().to_string(), ..options("", "", "") };
        let transport = Transport::Shell { env: vec![("HOME".into(), dir.display().to_string())], ask: None };
        let began = std::time::Instant::now();
        let Err(error) = connect(&Server::default(), &transport, &options, &Cancel::default(), &|_| {}) else { panic!("{name}: it was accepted") };
        assert!(began.elapsed() < Duration::from_secs(10), "{name}: it hung");
        assert!(!error.retry && error.needs.is_none(), "{name}: {error:?}");
        assert!(error.message.contains("another version of Endeavor"), "{name}: {}", error.message);
    }
}

#[test]
fn a_cancel_forgets_ssh_once_it_has_exited() {
    let slot: Mutex<Option<u32>> = Mutex::new(Some(7));
    Cancel::finished(&slot, 8);
    assert_eq!(*slot.lock().unwrap(), Some(7), "another connect's ssh isn't forgotten");
    Cancel::finished(&slot, 7);
    assert_eq!(*slot.lock().unwrap(), None);
}

/// What the script hands the helper for `root`, `state` and `depot` under `home`.
#[cfg(unix)]
fn where_they_land(home: &Path, root: &str, state: &str, depot: &str) -> (String, String, String) {
    let args = connect_args(home, &format!("{root}\n{state}\n{depot}\n--julia\nauto\nprocess\n"));
    let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    (at("--state-dir"), at("--runtime"), at("--depot"))
}

#[test]
#[cfg(unix)]
fn a_leading_tilde_is_the_servers_home() {
    let home = crate::client::scratch("bootstrap-tilde");
    let h = home.display().to_string();
    fake_install(&home.join("r"));
    assert_eq!(where_they_land(&home, "~/r", "~/s", "~/d:"), (format!("{h}/s"), format!("{h}/r/v1/runtime"), format!("{h}/d:")));
    // Only the depot's first entry is the home's; a `~` that is not first or not leading stays as it is.
    assert_eq!(where_they_land(&home, "~/r", "rel", "~/d:/x/~/y:").2, format!("{h}/d:/x/~/y:"));
    assert_eq!(where_they_land(&home, "~/r", "rel", "/a:~/b:").2, "/a:~/b:");
    assert_eq!(where_they_land(&home, "~/r", "rel", "~:").2, format!("{h}:"));
    assert_eq!(where_they_land(&home, "~/r", "~", "").0, h);
    fake_install(&home);
    assert_eq!(where_they_land(&home, "~", "st", "").1, format!("{h}/v1/runtime"));
    fake_install(&home.join("a~b"));
    assert_eq!(where_they_land(&home, "~/a~b", "~x/y", "").0, format!("{h}/a~b/~x/y"));
}

#[test]
fn only_sign_in_and_host_key_failures_end_a_reconnect() {
    let ask = Auth::Batch;
    let retry = |line: &str| explain_retry(&lab(), &ask, &[line.to_owned()], None, false, false).1;
    for temporary in [
        "ssh: Could not resolve hostname lab: Temporary failure in name resolution",
        "ssh: connect to host lab port 22: Connection refused",
        "ssh: connect to host lab port 22: Operation timed out",
        "ssh: connect to host lab port 22: Network is unreachable",
        "ssh: connect to host lab port 22: No route to host",
    ] {
        assert!(retry(temporary), "{temporary}");
    }
    for needs_the_user in ["jc@lab: Permission denied (publickey).", "Host key verification failed.", "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@"] {
        assert!(!retry(needs_the_user), "{needs_the_user}");
    }
    assert!(!explain_retry(&lab(), &ask, &[], None, true, false).1, "a cancel");
}

#[test]
fn the_bootstrap_script_settles_auto_as_the_helper_does() {
    assert!(bootstrap_script("v1", false).contains(PICK_LAUNCHER_SH), "the script holds the shell text that is tested");
    let dir = std::env::temp_dir().join(format!("endeavor-pick-launcher-{}", std::process::id()));
    let (with, without) = (dir.join("with bin"), dir.join("without"));
    std::fs::create_dir_all(&with).unwrap();
    std::fs::create_dir_all(&without).unwrap();
    std::fs::write(with.join("sinfo"), "").unwrap();
    let pick = |ln: &str, path: &str| {
        let output = Command::new("/bin/sh").arg("-c").arg(format!("{PICK_LAUNCHER_SH}; printf %s \"$ln\"")).env_clear().env("PATH", path).env("ln", ln).output().unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    let fixed = ["/usr/bin", "/usr/local/bin", "/opt/slurm/bin"].iter().any(|d| Path::new(d).join("sinfo").is_file());
    let (with, without) = (with.display().to_string(), without.display().to_string());
    assert_eq!(pick("auto", &format!("{without}:{with}")), "slurm", "sinfo in a folder of PATH, one with a space in its name");
    assert_eq!(pick("auto", &format!("{without}::")), if fixed { "slurm" } else { "process" }, "else the fixed folders decide");
    assert_eq!(pick("process", &with), "process", "a launcher that is named is left as it is");
    assert_eq!(pick("slurm", &without), "slurm");
    let _ = std::fs::remove_dir_all(&dir);
}

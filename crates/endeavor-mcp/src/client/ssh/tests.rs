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
    Options { auth: Auth::Batch, root: root.into(), state: state.into(), depot: depot.into(), helper: &no_helper_here }
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
    for bad in ["", "-oProxyCommand=x", "-p", "lab host", "lab;rm", "lab\nx", "$(x)", "a'b", "lab/", "lab*"] {
        assert!(valid_host(bad).is_err(), "{bad:?}");
        assert!(Transport::Ssh { host: bad.into(), port: None }.command("true", &Auth::Batch).is_err(), "{bad:?}");
    }
    for good in ["lab", "jc@lab.example.edu", "10.0.0.2", "fe80::1", "hoffman2_login-1"] {
        assert!(valid_host(good).is_ok(), "{good:?}");
    }
    assert_eq!(valid_host("-x").unwrap_err(), "\"-x\" isn't an SSH host name.");
}

#[test]
fn the_bootstrap_holds_nothing_a_login_shell_would_change() {
    let script = bootstrap_script(crate::embedded::BUILD_VERSION);
    for bad in ['\'', '\\', '!', '\n'] {
        assert!(!script.contains(bad), "{bad:?} in {script}");
    }
}

/// `script` run as a login shell would run ssh's command, with `preamble` on stdin.
#[cfg(unix)]
fn run_script(script: &str, preamble: &str, home: &Path) -> String {
    let mut shell = Command::new("sh").arg("-c").arg(format!("sh -c '{script}'")).env("HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    shell.stdin.take().unwrap().write_all(preamble.as_bytes()).unwrap();
    let out = shell.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[cfg(unix)]
fn the_bootstrap_runs_under_a_shell_and_asks_for_an_install() {
    let home = crate::client::scratch("bootstrap-need");
    let said = run_script(&bootstrap_script("v1"), "\n\n\n--julia\nauto\nprocess\n", &home);
    assert!(said.starts_with("ENDEAVOR ") && said.trim_end().ends_with(" need"), "{said}");
    assert!(!home.join(".cache").exists());
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
    let said = run_script(&bootstrap_script("v1"), preamble, home);
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
    fake_install(&home.join(".cache/endeavor"));
    let args = connect_args(&home, "\nstate\n\n--julia\nauto\nprocess\n");
    let cache = home.join(".cache/endeavor");
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
    let err = connect(&server, &Transport::Shell { env: Vec::new(), ask: None }, &options("a\nb", "s", ""), &Cancel::default(), &|_| {}).err().expect("refused");
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

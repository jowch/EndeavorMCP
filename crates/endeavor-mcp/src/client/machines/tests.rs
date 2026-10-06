use super::*;

#[test]
fn reads_the_ssh_host_field() {
    assert_eq!(Server::parse_target(" lab "), Ok(("lab".into(), None)));
    assert_eq!(Server::parse_target("jc@lab.example.edu:2222"), Ok(("jc@lab.example.edu".into(), Some(2222))));
    assert_eq!(Server::parse_target("fe80::1"), Ok(("fe80::1".into(), None)));
    assert!(Server::parse_target("lab:ssh").is_err());
    assert!(Server::parse_target("").is_err());
    assert!(Server::parse_target("-oProxyCommand=x").is_err());
    assert!(Server::parse_target("two words").is_err());
    let s = Server { ssh_host: "lab".into(), port: Some(2222), ..Default::default() };
    assert_eq!(Server::parse_target(&s.ssh_target()), Ok(("lab".into(), Some(2222))));
}

#[test]
fn julia_setting_becomes_helper_arguments() {
    let with = |j: Option<&str>| Server { julia: j.map(String::from), ..Default::default() }.julia_args();
    assert_eq!(with(None), ["--julia", "auto"]);
    assert_eq!(with(Some("  ")), ["--julia", "auto"]);
    assert_eq!(with(Some("/opt/julia/bin/julia")), ["--julia", "/opt/julia/bin/julia"]);
    assert_eq!(with(Some("~/julia-1.11/bin/julia")), ["--julia", "~/julia-1.11/bin/julia"]);
    assert_eq!(with(Some("/Users/jc/Library/Application Support/julia/bin/julia")), ["--julia", "/Users/jc/Library/Application Support/julia/bin/julia"]);
    assert_eq!(with(Some("module load julia/1.11")), ["--julia-shell", "module load julia/1.11"]);
    assert_eq!(with(Some("/opt/lmod/setup.sh && module load julia")), ["--julia-shell", "/opt/lmod/setup.sh && module load julia"]);
    assert_eq!(with(Some("module load a\nmodule load b")), ["--julia-shell", "module load a; module load b"]);
}

#[test]
fn a_cluster_keeps_its_own_state_folder() {
    let server = Server { id: "server-1".into(), ..Default::default() };
    assert_eq!(server.launcher(), ["process", "state"]);
    let cluster = Server { cluster: Some(Cluster::default()), ..server };
    assert_eq!(cluster.launcher(), ["slurm", "cluster-server-1"]);
    let saved = serde_json::to_string(&cluster).unwrap();
    assert_eq!(serde_json::from_str::<Server>(&saved).unwrap(), cluster);
    assert!(serde_json::from_str::<Server>(r#"{"id":"a","ssh_host":"lab"}"#).unwrap().cluster.is_none());
}

#[test]
fn a_server_reads_and_writes_the_apps_json() {
    let saved = r#"{"id":"a","name":"lab-server","ssh_host":"lab","port":2222,"julia":null,"idle_stop":"week","cluster":null}"#;
    let server: Server = serde_json::from_str(saved).unwrap();
    assert_eq!((server.idle_stop, server.port), (Some(IdleStop::Week), Some(2222)));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&serde_json::to_string(&server).unwrap()).unwrap(), serde_json::from_str::<serde_json::Value>(saved).unwrap());
    for (stop, text) in [(IdleStop::Hours12, "hours12"), (IdleStop::Hours24, "hours24"), (IdleStop::Hours48, "hours48"), (IdleStop::Week, "week"), (IdleStop::Never, "never")] {
        assert_eq!(serde_json::to_string(&stop).unwrap(), format!("\"{text}\""));
    }
    assert_eq!(IdleStop::default(), IdleStop::Hours48);
}

#[test]
fn a_cluster_picks_partitions_and_builds_its_job() {
    let cluster: Cluster = serde_json::from_str(r#"{"account":"lab","depot":"/scratch/d","partitions":[{"name":"gpu","default":false,"max_minutes":null,"cpus":8,"mem_mb":65536},{"name":"cpu","default":true,"max_minutes":60,"cpus":4,"mem_mb":8192}]}"#).unwrap();
    assert_eq!(cluster.partition(None).map(|p| p.name.as_str()), Some("cpu"));
    assert_eq!(cluster.partition(Some("gpu")).map(|p| p.name.as_str()), Some("gpu"));
    assert!(cluster.partition(Some("none")).is_none());
    let job = cluster.job(&Resources::default());
    assert_eq!((job.account.as_deref(), job.depot.as_deref()), (Some("lab"), Some("/scratch/d")));
}

#[test]
fn ids_differ() {
    assert!(Server::new_id().starts_with("server-"));
}

#[test]
fn suggests_plain_hosts_from_ssh_config() {
    let dir = crate::client::scratch("sshconfig");
    std::fs::create_dir_all(dir.join("conf.d")).unwrap();
    std::fs::write(
        dir.join("config"),
        "Include conf.d/lab\nInclude conf.d/*\n\nHost *\n  ForwardAgent no\nHost hoffman2 h2\n  HostName hoffman2.idre.ucla.edu\nhost=gpu-box\nHost *.cluster !bad lab-server\nMatch host x\n",
    )
    .unwrap();
    std::fs::write(dir.join("conf.d/lab"), "Host lab-server\nHost bench\n").unwrap();
    let mut hosts = Vec::new();
    collect_hosts(&dir.join("config"), &dir, &mut hosts, 0);
    assert_eq!(hosts, ["lab-server", "bench", "hoffman2", "h2", "gpu-box"]);
}

#[test]
fn targets_round_trip_with_and_without_a_port() {
    for (host, port, text) in [
        ("lab", None, "lab"),
        ("lab", Some(2222), "lab:2222"),
        ("jc@lab.example.edu", Some(22), "jc@lab.example.edu:22"),
        ("::1", None, "::1"),
        ("::1", Some(2222), "[::1]:2222"),
        ("jc@fe80::1", Some(2222), "jc@[fe80::1]:2222"),
        ("jc@fe80::1", None, "jc@fe80::1"),
    ] {
        let server = Server { ssh_host: host.into(), port, ..Default::default() };
        assert_eq!(server.ssh_target(), text);
        assert_eq!(Server::parse_target(text), Ok((host.to_owned(), port)), "{text}");
    }
    assert_eq!(Server::parse_target("[::1]"), Ok(("::1".into(), None)));
}

#[test]
fn a_user_name_with_an_at_sign_round_trips() {
    for (host, port, text) in [
        ("a@b@lab", None, "a@b@lab"),
        ("a@b@lab", Some(22), "a@b@lab:22"),
        ("a@b@fe80::1", Some(2222), "a@b@[fe80::1]:2222"),
        ("a@b@fe80::1", None, "a@b@fe80::1"),
    ] {
        let server = Server { ssh_host: host.into(), port, ..Default::default() };
        assert_eq!(server.ssh_target(), text);
        assert_eq!(Server::parse_target(text), Ok((host.to_owned(), port)), "{text}");
    }
}

#[test]
fn a_target_without_a_host_or_without_a_user_before_its_at_sign_is_refused() {
    for bad in ["jc@", "@lab", ":22", "jc@:22", "jc@[]:22", "[]:22", "@", "@@", "@lab:22"] {
        assert!(Server::parse_target(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_bad_target_says_what_is_wrong() {
    assert_eq!(Server::parse_target("  ").unwrap_err(), "Enter an SSH host: an alias from ~/.ssh/config, or user@host.");
    assert_eq!(Server::parse_target("[::1]:ssh").unwrap_err(), "\"ssh\" isn't a port number.");
    assert_eq!(Server::parse_target("lab:0").unwrap_err(), "\"0\" isn't a port number.");
    for bad in ["[::1", "[::1]x", "[]", "[::1;x]:22", "lab;rm", "-x"] {
        assert!(Server::parse_target(bad).is_err(), "{bad}");
    }
}

fn machine(id: &str, name: &str) -> Server {
    Server { id: id.into(), name: name.into(), ssh_host: name.into(), ..Default::default() }
}

#[test]
fn the_machines_file_is_where_the_configuration_folder_says() {
    let env = |vars: &'static [(&'static str, &'static str)]| move |name: &str| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string());
    if cfg!(windows) {
        assert_eq!(machines_path(&env(&[("APPDATA", r"C:\Users\jc\AppData\Roaming")])), Path::new(r"C:\Users\jc\AppData\Roaming").join("Endeavor").join("machines.json"));
        return;
    }
    assert_eq!(machines_path(&env(&[("XDG_CONFIG_HOME", "/x/config"), ("HOME", "/h")])), Path::new("/x/config/endeavor/machines.json"));
    assert_eq!(machines_path(&env(&[("HOME", "/h")])), Path::new("/h/.config/endeavor/machines.json"));
    assert_eq!(machines_path(&env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/h")])), Path::new("/h/.config/endeavor/machines.json"), "empty is unset");
    assert_eq!(machines_path(&env(&[("XDG_CONFIG_HOME", "relative"), ("HOME", "/h")])), Path::new("/h/.config/endeavor/machines.json"), "XDG says to ignore a relative one");
}

#[test]
fn machines_are_added_found_replaced_and_removed() {
    let file = MachinesFile::at(crate::client::scratch("machines-file").join("config/endeavor/machines.json"));
    assert_eq!(file.load(), Ok(Vec::new()), "no file is no machines");
    file.save(machine("server-1", "Hoffman2")).unwrap();
    file.save(machine("server-2", "lab")).unwrap();
    assert_eq!(file.load().unwrap().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["server-1", "server-2"]);
    assert_eq!(file.find_by_id("server-2").unwrap().unwrap().name, "lab");
    assert_eq!(file.find_by_name("hoffman2").unwrap().unwrap().id, "server-1", "names ignore case");
    assert_eq!(file.find("server-1").unwrap().unwrap().name, "Hoffman2");
    assert_eq!(file.find("lab").unwrap().unwrap().id, "server-2");
    assert_eq!(file.find("nope"), Ok(None));

    let renamed = Server { name: "Lab box".into(), port: Some(2222), ..machine("server-2", "lab") };
    file.save(renamed.clone()).unwrap();
    assert_eq!(file.load().unwrap(), [machine("server-1", "Hoffman2"), renamed], "replaced in place");
    assert_eq!(file.remove("server-1"), Ok(true));
    assert_eq!(file.remove("server-1"), Ok(false));
    assert_eq!(file.load().unwrap().len(), 1);
    let left: Vec<_> = std::fs::read_dir(file.path().parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(left.iter().all(|name| !name.contains(".tmp")), "{left:?}");
}

#[test]
#[cfg(unix)]
fn the_machines_file_is_for_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let file = MachinesFile::at(crate::client::scratch("machines-private").join("endeavor/machines.json"));
    file.save(machine("server-1", "lab")).unwrap();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(file.path()), 0o600);
    assert_eq!(mode(file.path().parent().unwrap()), 0o700);
}

#[test]
fn a_file_that_cant_be_read_is_an_error_that_names_it_and_stays_as_it_is() {
    let dir = crate::client::scratch("machines-broken");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    for broken in ["{not json", "", "{\"id\":\"a\"}"] {
        std::fs::write(&path, broken).unwrap();
        let error = file.load().unwrap_err();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(file.save(machine("server-1", "lab")).unwrap_err().contains(&path.display().to_string()));
        assert!(file.remove("server-1").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken, "never replaced");
    }
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(file.load().unwrap_err().contains(&path.display().to_string()), "a folder in its place is not 'no machines'");
}

#[test]
fn the_apps_entries_load_with_fields_this_version_lacks() {
    let dir = crate::client::scratch("machines-app");
    let path = dir.join("machines.json");
    std::fs::write(&path, r#"[{"id":"server-1","name":"lab","ssh_host":"lab","port":2222,"julia":"module load julia","idle_stop":"week","later":true}]"#).unwrap();
    let server = MachinesFile::at(&path).find("lab").unwrap().unwrap();
    assert_eq!((server.port, server.julia.as_deref(), server.idle_stop), (Some(2222), Some("module load julia"), Some(IdleStop::Week)));
}

use super::*;
use serde_json::json;

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

fn raw(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_file_is_an_object_with_a_schema_and_a_bare_list_still_reads() {
    let dir = crate::client::scratch("machines-shape");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    file.save(machine("server-1", "lab")).unwrap();
    assert_eq!(raw(&path)["schema"], 1);
    assert_eq!(raw(&path)["machines"][0]["id"], "server-1");

    std::fs::write(&path, r#"[{"id":"server-1","name":"lab","ssh_host":"lab","later":1}]"#).unwrap();
    assert_eq!(file.find("lab").unwrap().unwrap().id, "server-1", "a bare list reads");
    file.save(machine("server-2", "box")).unwrap();
    let written = raw(&path);
    assert_eq!((written["schema"].clone(), written["machines"][0]["later"].clone()), (json!(1), json!(1)), "written as an object, keeping the fields");
    for broken in ["{}", r#"{"schema":1}"#, r#"{"schema":"one","machines":[]}"#, r#"{"machines":{}}"#] {
        std::fs::write(&path, broken).unwrap();
        assert!(file.load().unwrap_err().contains("isn't valid"), "{broken}");
    }
}

#[test]
fn fields_this_version_lacks_survive_a_rewrite_and_go_with_a_removed_machine() {
    let dir = crate::client::scratch("machines-unknown");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    let before = json!({
        "schema": 1, "theme": {"dark": true},
        "machines": [
            {"id": "a", "name": "a", "ssh_host": "a", "color": "red", "tags": ["x"]},
            {"id": "b", "name": "b", "ssh_host": "b", "color": "blue"},
        ],
    });
    std::fs::write(&path, before.to_string()).unwrap();

    file.save(machine("c", "c")).unwrap();
    let now = raw(&path);
    assert_eq!(now["theme"], json!({"dark": true}));
    assert_eq!((now["machines"][0]["color"].clone(), now["machines"][0]["tags"].clone(), now["machines"][1]["color"].clone()), (json!("red"), json!(["x"]), json!("blue")), "{now}");

    let mut changed = file.find_by_id("a").unwrap().unwrap();
    changed.port = Some(2222);
    file.save(changed).unwrap();
    let now = raw(&path);
    assert_eq!((now["machines"][0]["port"].clone(), now["machines"][0]["color"].clone(), now["machines"][1]["color"].clone()), (json!(2222), json!("red"), json!("blue")), "{now}");
    // A record built anew for a known id keeps the file's own extra fields.
    file.save(machine("b", "b")).unwrap();
    assert_eq!(raw(&path)["machines"][1]["color"], "blue");

    assert_eq!(file.remove("a"), Ok(true));
    let now = raw(&path);
    assert!(!now.to_string().contains("red") && now["theme"] == json!({"dark": true}) && now["machines"][0]["color"] == "blue", "{now}");
}

#[test]
fn a_file_of_a_newer_schema_is_read_and_never_written() {
    let dir = crate::client::scratch("machines-newer");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    let text = r#"{"schema":2,"machines":[{"id":"a","name":"a","ssh_host":"a","new_thing":{"x":1}}],"extra":true}"#;
    std::fs::write(&path, text).unwrap();
    assert_eq!(file.find("a").unwrap().unwrap().id, "a");
    for error in [file.save(machine("b", "b")).unwrap_err(), file.remove("a").unwrap_err(), file.load_writable().unwrap_err()] {
        assert!(error.contains("newer Endeavor") && error.contains(&path.display().to_string()), "{error}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "untouched");
}

#[test]
fn a_newer_file_of_another_shape_says_so_and_never_advises_removing_it() {
    let dir = crate::client::scratch("machines-newer-shape");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    let text = r#"{"schema":2,"hosts":[]}"#;
    std::fs::write(&path, text).unwrap();
    for error in [file.load().unwrap_err(), file.load_writable().unwrap_err(), file.save(machine("b", "b")).unwrap_err()] {
        assert!(error.contains("A newer Endeavor wrote") && error.contains("schema 2") && !error.contains("remove the file"), "{error}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    std::fs::write(&path, r#"{"schema":1,"hosts":[]}"#).unwrap();
    assert!(file.load().unwrap_err().contains("isn't valid"), "the same shape at a schema this version knows is a broken file");
}

#[test]
fn fields_inside_a_cluster_survive_and_a_dropped_cluster_takes_them_along() {
    let dir = crate::client::scratch("machines-nested");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    let before = json!({"schema": 1, "machines": [{
        "id": "a", "name": "a", "ssh_host": "a",
        "cluster": {"account": null, "qos": "long", "resources": {"cpus": 4, "priority": 3},
                    "partitions": [{"name": "p", "default": true, "max_minutes": null, "cpus": 2, "mem_mb": 1024, "features": ["x"]}]},
    }]});
    std::fs::write(&path, before.to_string()).unwrap();
    let mut a = file.find_by_id("a").unwrap().unwrap();
    a.cluster.as_mut().unwrap().resources.cpus = 8;
    a.cluster.as_mut().unwrap().partitions[0].cpus = 16;
    file.save(a.clone()).unwrap();
    let now = raw(&path);
    let cluster = &now["machines"][0]["cluster"];
    assert_eq!((cluster["qos"].clone(), cluster["resources"]["priority"].clone(), cluster["partitions"][0]["features"].clone()), (json!("long"), json!(3), json!(["x"])), "{now}");
    assert_eq!((cluster["resources"]["cpus"].clone(), cluster["partitions"][0]["cpus"].clone()), (json!(8), json!(16)), "the changes are made");
    a.cluster = None;
    file.save(a).unwrap();
    assert!(!raw(&path).to_string().contains("qos"), "a cluster that was dropped takes its fields with it");
}

#[test]
fn a_save_that_expected_another_list_writes_nothing() {
    let dir = crate::client::scratch("machines-expecting");
    let path = dir.join("machines.json");
    let file = MachinesFile::at(&path);
    let by_name = |name: &'static str| move |servers: &[Server]| servers.iter().find(|s| s.name == name).map(|s| s.id.clone());
    file.save_expecting(machine("a", "lab"), None, &by_name("lab")).unwrap();
    file.save(machine("b", "other")).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    // A new machine whose id is taken now, and one whose name now picks another record.
    assert!(file.save_expecting(machine("b", "lab2"), None, &by_name("lab2")).unwrap_err().contains("changed while Endeavor was connecting"));
    assert!(file.save_expecting(machine("c", "lab"), None, &by_name("lab")).unwrap_err().contains("changed"));
    assert!(file.save_expecting(machine("b", "other"), Some("a"), &by_name("other")).unwrap_err().contains("changed"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "untouched");
    file.save_expecting(machine("a", "lab"), Some("a"), &by_name("lab")).unwrap();
}

#[test]
fn ids_that_cannot_be_folders_on_every_system_are_refused() {
    for good in ["server-18f3a9c2b", "lab", "a.b", "com10", "console", "lab_2"] {
        assert!(valid_id(good).is_ok(), "{good}");
    }
    for bad in ["", ".hidden", "lab.", "..", "a/b", "a b", "Lab", "LAB", "con", "nul", "aux", "prn", "com1", "lpt9", "nul.txt", "com3.x", "é"] {
        assert!(valid_id(bad).is_err(), "{bad:?}");
    }
    assert!(valid_id("Lab").unwrap_err().contains("lower-case letters"));
}

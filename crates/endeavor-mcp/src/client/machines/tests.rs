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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::checks::{self, Evidence};

static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

const AGENT_LIMIT: Duration = Duration::from_secs(20 * 60);

const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);

const BUILT_IN_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Skill", "ToolSearch"];

const OUR_SERVER: &str = "mcp__plugin_endeavor_endeavor";

fn agent_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR", "TZ",
        "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "no_proxy", "ALL_PROXY", "all_proxy",
        "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
        "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN",
        "SYSTEMROOT", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "HOMEDRIVE", "HOMEPATH",
    ];
    std::env::vars().filter(|(k, _)| KEEP.contains(&k.as_str())).collect()
}

fn isolation_problems(init: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let tools: Vec<&str> = init["tools"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let extra: Vec<&&str> = tools.iter().filter(|t| !BUILT_IN_TOOLS.contains(t) && !t.starts_with(&format!("{OUR_SERVER}__"))).collect();
    if !extra.is_empty() {
        problems.push(format!("the agent had other tools: {extra:?}"));
    }
    let servers: Vec<&str> = init["mcp_servers"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).filter(|n| *n != "plugin:endeavor:endeavor").collect();
    if !servers.is_empty() {
        problems.push(format!("the agent had other MCP servers: {servers:?}"));
    }
    let plugins: Vec<&str> = init["plugins"].as_array().into_iter().flatten().filter(|p| p["path"] != "builtin").filter_map(|p| p["name"].as_str()).filter(|n| *n != "endeavor").collect();
    if !plugins.is_empty() {
        problems.push(format!("the agent had other plugins: {plugins:?}"));
    }
    if init["memory_paths"].get("team").is_some() {
        problems.push(format!("the agent had a shared memory folder: {}", init["memory_paths"]));
    }
    problems
}

struct Options {
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
    julia: String,
    r: String,
    skip_r: bool,
    depot: Option<String>,
    retries: Option<u32>,
    long: bool,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut o = Options { only: None, out: None, julia: std::env::var("ENDEAVOR_E2E_JULIA").unwrap_or_else(|_| "auto".into()), r: std::env::var("ENDEAVOR_E2E_R").unwrap_or_else(|_| "auto".into()), skip_r: false, depot: None, retries: None, long: false };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--only" => o.only = Some(value()?.split(',').map(|s| s.trim().to_owned()).collect()),
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--julia" => o.julia = value()?,
            "--r" => o.r = value()?,
            "--skip-r" => o.skip_r = true,
            "--long" => o.long = true,
            "--depot" => o.depot = Some(value()?),
            "--retries" => o.retries = Some(value()?.parse().map_err(|e| format!("--retries: {e}"))?),
            "--model" => MODEL.set(value()?).map_err(|_| "--model given twice".to_owned())?,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(o)
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository")
}

pub fn runtime_args(work: &Path) -> Vec<String> {
    let settings: Value = std::fs::read_to_string(work.join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut args = vec!["--state-dir".to_owned(), work.join("state").display().to_string()];
    for key in ["julia", "r", "depot"] {
        if let Some(v) = settings[key].as_str() {
            args.extend([format!("--{key}"), v.to_owned()]);
        }
    }
    args
}

pub fn runtime_env(work: &Path) -> Vec<(String, PathBuf)> {
    let mut env: Vec<(String, PathBuf)> = ["XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME"].iter().map(|k| (k.to_string(), work.join("home").join(k.to_lowercase()))).collect();
    let settings: Value = std::fs::read_to_string(work.join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if settings["browser"] == true {
        env.push(("ENDEAVOR_TEST_BROWSER".to_owned(), work.join("browser.txt")));
    }
    if let Some(seconds) = settings["python_wait"].as_u64() {
        env.push(("ENDEAVOR_TEST_MARIMO_WAIT_SECS".to_owned(), PathBuf::from(seconds.to_string())));
    }
    if settings["ssh"] == true {
        let f = crate::ssh::Folders::of(work);
        env.extend([("HOME".to_owned(), f.home), ("ENDEAVOR_TEST_ROOT".to_owned(), f.root), ("ENDEAVOR_TEST_STATE".to_owned(), f.state)]);
        env.push(("ENDEAVOR_TEST_DEPOT".to_owned(), PathBuf::from(settings["depot"].as_str().unwrap_or_default())));
        let outer = std::env::var_os("PATH").unwrap_or_default();
        let path = std::iter::once(f.bin).chain(std::env::split_paths(&outer));
        env.push(("PATH".to_owned(), PathBuf::from(std::env::join_paths(path).unwrap())));
    }
    env
}

pub fn main(args: &[String]) -> i32 {
    let o = match options(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("endeavor-smoke: {e}");
            return 2;
        }
    };
    let repo = repo();
    let exe = std::env::current_exe().unwrap();
    let endeavor = exe.with_file_name(format!("endeavor{}", std::env::consts::EXE_SUFFIX));
    if !endeavor.is_file() {
        eprintln!("endeavor-smoke: no {} next to this binary; build it first: cargo build -p endeavor-mcp -p endeavor-smoke", endeavor.display());
        return 2;
    }
    let mut commit = git(&repo, &["rev-parse", "--short", "HEAD"]);
    if git(&repo, &["status", "--porcelain"]) != "unknown" {
        commit += "-dirty";
    }
    let out = o.out.clone().unwrap_or_else(|| repo.join("target/smoke").join(format!("{}-{commit}", utc_stamp())));
    std::fs::create_dir_all(&out).unwrap();
    let out = out.canonicalize().unwrap();
    let depot = o.depot.clone().unwrap_or_else(|| format!("{}:", repo.join("target/smoke/depot").display()));
    let agent = claude_version();
    let mut tasks: Vec<PathBuf> = std::fs::read_dir(repo.join("smoke/tasks")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("prompt.md").is_file()).collect();
    tasks.sort();
    if let Some(only) = &o.only {
        tasks.retain(|t| only.iter().any(|id| t.file_name().is_some_and(|n| n == id.as_str())));
    } else if !o.long {
        tasks.retain(|t| !long_task(t));
    }
    if tasks.is_empty() {
        eprintln!("endeavor-smoke: no tasks to run");
        return 2;
    }
    eprintln!("endeavor-smoke: {} task(s), results in {}", tasks.len(), out.display());
    let scratch = std::env::temp_dir().join(format!("endeavor-smoke-{}-{}", utc_stamp(), std::process::id()));
    let have_r = have_r(&o.r);
    let warm_r = have_r && tasks.iter().any(|t| r_task(t));
    if let Err(e) = warm(&repo, &scratch.join("warm"), &endeavor, &o.julia, &o.r, &depot, warm_r) {
        eprintln!("endeavor-smoke: warming the depot failed: {e}");
        return 2;
    }
    let mut rows = Vec::new();
    for task in &tasks {
        let id = task.file_name().unwrap().to_string_lossy().into_owned();
        if r_task(task) && !have_r {
            eprintln!("── {id}: no R here ({})", if o.r == "auto" { "no Rscript on the login shell's PATH".to_owned() } else { o.r.clone() });
            rows.push(no_r_row(&id, &o.r, o.skip_r));
            continue;
        }
        let mut attempts = Vec::new();
        loop {
            let n = attempts.len() + 1;
            eprintln!("── {id}, attempt {n}");
            let work = scratch.join(&id).join(format!("attempt-{n}"));
            let result = attempt(&id, task, &work, &endeavor, &exe, &repo, &o.julia, &o.r, &depot);
            eprintln!("   {} in {:.0}s", if result["passed"] == true { "passed" } else { "FAILED" }, result["metrics"]["seconds"].as_f64().unwrap_or(0.0));
            let _ = std::fs::write(work.join("result.json"), serde_json::to_string_pretty(&result).unwrap());
            let kept = out.join(&id).join(format!("attempt-{n}"));
            let _ = std::fs::remove_dir_all(&kept);
            std::fs::create_dir_all(&kept).unwrap();
            copy_dir(&work, &kept, &["state", "home", "rerun", "depot", "ssh-home", "server", "ssh"]);
            remove_work(&work);
            let passed = result["passed"] == true;
            attempts.push(result);
            let retries = o.retries.unwrap_or(if long_task(task) { 0 } else { 2 });
            if (n == 1 && passed) || n as u32 > retries || expected_failure(task).is_some() {
                break;
            }
        }
        rows.push(row(&id, &attempts, expected_failure(task).as_deref()));
    }
    remove_work(&scratch);
    let mut models: Vec<String> = rows.iter().flat_map(|r| r["models"].as_array().cloned().unwrap_or_default()).filter_map(|m| m.as_str().map(str::to_owned)).collect();
    models.sort();
    models.dedup();
    let summary = json!({ "commit": commit, "agent": agent, "models": models, "tasks": rows });
    let _ = std::fs::write(out.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap());
    let md = summary_md(&summary);
    let _ = std::fs::write(out.join("summary.md"), &md);
    println!("{md}");
    if rows.iter().all(|r| r["status"] == "pass" || r["status"] == "expected failure" || r["status"] == SKIPPED_NO_R) { 0 } else { 1 }
}

#[allow(clippy::too_many_arguments)]
fn warm(repo: &Path, work: &Path, endeavor: &Path, julia: &str, r: &str, depot: &str, with_r: bool) -> Result<(), String> {
    eprintln!("── warming the depot (the first time, this installs Pluto and the packages, and takes several minutes)");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    copy_dir(&repo.join("smoke/warm"), &project, &[]);
    std::fs::write(work.join("settings.json"), json!({ "julia": julia, "r": r, "depot": depot }).to_string()).unwrap();
    let began = Instant::now();
    let result = crate::mcp::Session::start(endeavor, work, &project).and_then(|mut s| {
        crate::mcp::open_and_run(&mut s, "warm.jl", RUN_LIMIT)?;
        if with_r {
            eprintln!("   and Ember, for R notebooks");
            let mut r = crate::mcp::Session::start(endeavor, work, &project)?;
            crate::mcp::open_and_run(&mut r, "warm.R", RUN_LIMIT)?;
            let errored: Vec<String> = crate::mcp::notebooks(&mut r)?.iter().flat_map(|nb| nb.cells.iter().filter(|c| c.errored).map(|c| format!("{}: {}", c.code, c.output))).collect();
            if !errored.is_empty() {
                return Err(errored.join("; "));
            }
        }
        let errored: Vec<String> = crate::mcp::notebooks(&mut s)?.iter().flat_map(|nb| nb.cells.iter().filter(|c| c.errored).map(|c| format!("{}: {}", c.code, c.output))).collect();
        if errored.is_empty() { Ok(()) } else { Err(errored.join("; ")) }
    });
    stop_runtime(endeavor, work);
    remove_work(work);
    eprintln!("   done in {:.0}s", began.elapsed().as_secs_f64());
    result
}

fn expected_failure(task: &Path) -> Option<String> {
    let specs: Value = serde_json::from_str(&std::fs::read_to_string(task.join("checks.json")).ok()?).ok()?;
    specs["expected_to_fail"].as_str().map(str::to_owned)
}

fn long_task(task: &Path) -> bool {
    let setup: Value = std::fs::read_to_string(task.join("setup.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    setup["long"] == true
}

fn r_task(task: &Path) -> bool {
    let setup: Value = std::fs::read_to_string(task.join("setup.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    setup["r"] == true
}

fn have_r(r: &str) -> bool {
    let mut command = if r == "auto" {
        let mut c = Command::new(std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into()));
        c.args(["-lc", "command -v Rscript"]);
        c
    } else {
        let mut c = Command::new(r);
        c.arg("--version");
        c
    };
    command.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

const SKIPPED_NO_R: &str = "skipped (no R)";

fn no_r_row(id: &str, r: &str, skip: bool) -> Value {
    let failure = format!("no R here ({}); install R, or give --skip-r on a computer known to have none", if r == "auto" { "no Rscript on the login shell's PATH" } else { r });
    let (status, failed, failures) = if skip { (SKIPPED_NO_R, 0, vec![]) } else { ("failing", 1, vec![failure]) };
    json!({ "task": id, "status": status, "expected_to_fail": null, "attempts": 0, "failed": failed, "failures": failures, "first": {}, "models": [] })
}

#[allow(clippy::too_many_arguments)]
fn attempt(id: &str, task: &Path, work: &Path, endeavor: &Path, exe: &Path, repo: &Path, julia: &str, r: &str, depot: &str) -> Value {
    let _ = std::fs::remove_dir_all(work);
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    if task.join("project").is_dir() {
        copy_dir(&task.join("project"), &project, &[]);
    }
    let project = project.canonicalize().unwrap();
    let setup: Value = std::fs::read_to_string(task.join("setup.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let depot = if setup["depot"] == "empty" { work.join("depot").display().to_string() } else { depot.to_owned() };
    let ssh = setup["ssh"] == true;
    let limit = setup["minutes"].as_u64().map_or(AGENT_LIMIT, |m| Duration::from_secs(m * 60));
    let mut problems_before: Vec<String> = Vec::new();
    let browser = setup["browser"] == true;
    let python_wait = setup["python_wait"].as_u64();
    std::fs::write(work.join("settings.json"), json!({ "julia": julia, "r": r, "depot": depot, "ssh": ssh, "browser": browser, "python_wait": python_wait }).to_string()).unwrap();
    if task.join("inject.json").is_file() {
        std::fs::copy(task.join("inject.json"), work.join("inject.json")).unwrap();
    }
    if !setup["dialog"].is_null()
        && let Err(e) = answer_dialogs(work, &project, exe, &setup["dialog"])
    {
        problems_before.push(format!("setting up the dialog's answer: {e}"));
    }
    let mut turns = messages(&std::fs::read_to_string(task.join("prompt.md")).unwrap(), &std::fs::read_to_string(task.join("followup.md")).unwrap_or_default());
    let server_folder = crate::ssh::Folders::of(work).remote;
    for turn in &mut turns {
        *turn = turn.replace("{server_folder}", &server_folder.display().to_string());
    }
    let specs: Value = std::fs::read_to_string(task.join("checks.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({ "checks": [] }));
    let mut problems: Vec<String> = std::mem::take(&mut problems_before);
    if !setup["minutes"].is_null() && setup["minutes"].as_u64().is_none() {
        problems.push(format!("setup.json: minutes must be a whole number, not {}", setup["minutes"]));
    }
    let server = if ssh {
        match crate::ssh::start(work) {
            Ok(server) => Some(server),
            Err(e) => {
                problems.push(format!("starting the ssh server: {e}"));
                None
            }
        }
    } else {
        None
    };

    let opens: Vec<&str> = setup["open"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !opens.is_empty() {
        let opened = crate::mcp::Session::start(endeavor, work, &project).and_then(|mut s| opens.iter().map(|path| crate::mcp::open_and_run(&mut s, path, RUN_LIMIT)).collect::<Result<Vec<_>, _>>());
        match opened {
            Ok(ids) => {
                for turn in &mut turns {
                    *turn = turn.replace("{opened_notebook_id}", &ids[0]);
                }
            }
            Err(e) => problems.push(format!("setting up: {e}")),
        }
    }

    let began = Instant::now();
    let agent = if problems.is_empty() { run_claude(&turns, work, &project, endeavor, exe, repo, limit) } else { AgentRun::default() };
    let seconds = began.elapsed().as_secs_f64();
    let calls = crate::log::calls(&work.join("mcp.jsonl"));
    let injections = crate::log::injections(&work.join("mcp.jsonl"));
    let read = crate::mcp::Session::start(endeavor, work, &project).and_then(|mut s| crate::mcp::notebooks(&mut s));
    stop_runtime(endeavor, work);
    let server_files = if ssh { files_under(&server_folder) } else { Vec::new() };
    if ssh {
        stop_state(endeavor, work, &crate::ssh::Folders::of(work).state);
        drop(server);
    }
    let turns_at = crate::log::turns(&work.join("mcp.jsonl"));
    let questions = crate::log::questions(&work.join("mcp.jsonl"));
    let pages: Vec<String> = std::fs::read_to_string(work.join("browser.txt")).unwrap_or_default().lines().map(str::to_owned).collect();

    let (notebooks, read_error) = match read {
        Ok(n) => (n, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let wants_rerun = specs["checks"].as_array().into_iter().flatten().any(wants_reproducible);
    let rerun = (wants_rerun && read_error.is_none()).then(|| rerun(&notebooks, task, work, &project, endeavor));
    let ev = Evidence { calls: &calls, notebooks: &notebooks, agent_tools: &agent.tools, final_message: &agent.final_message, injections: &injections, rerun: rerun.as_ref(), turns: &turns_at, server_files: &server_files, questions: &questions, pages: &pages };
    let outcomes: Vec<checks::Outcome> = specs["checks"].as_array().into_iter().flatten().map(|spec| checks::run(spec, &ev)).collect();
    if let Some(e) = &agent.problem {
        problems.push(e.clone());
    }
    if let Some(e) = read_error {
        problems.push(format!("reading the notebooks afterwards: {e}"));
    }
    let passed = problems.is_empty() && outcomes.iter().all(|o| o.passed || o.soft);
    json!({
        "task": id,
        "passed": passed,
        "problems": problems,
        "checks": checks::to_json(&outcomes),
        "metrics": {
            "seconds": seconds,
            "tool_calls": calls.len(),
            "tool_errors": calls.iter().filter(|c| c.is_error).count(),
            "turns": agent.turns,
            "cost_usd": agent.cost_usd,
            "requests": agent.tokens.requests,
            "context_tokens": agent.tokens.context,
            "input_tokens": agent.tokens.input,
            "output_tokens": agent.tokens.output,
            "compactions": agent.compactions,
            "reply_bytes": calls.iter().map(|c| c.reply_bytes).sum::<usize>(),
        },
        "model": agent.model,
        "models_used": agent.models_used,
        "final_message": agent.final_message,
        "dialogs": std::fs::read_to_string(work.join("dialogs.jsonl")).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|d| json!({ "message": d["asked"]["message"], "answer": d["answer"], "why": d["why"] })).collect::<Vec<_>>(),
        "notebooks": notebooks_json(&notebooks),
        "rerun": rerun.as_ref().map(|r| r.as_ref().map_or_else(|e| json!(e), |n| notebooks_json(n))),
    })
}

fn wants_reproducible(check: &Value) -> bool {
    check["check"] == "reproducible" || check["checks"].as_array().into_iter().flatten().any(wants_reproducible)
}

fn messages(prompt: &str, followups: &str) -> Vec<String> {
    let mut turns = vec![prompt.trim().to_owned()];
    let mut next = String::new();
    for line in followups.lines().chain(["---"]) {
        if line.trim_end() == "---" {
            if !next.trim().is_empty() {
                turns.push(next.trim().to_owned());
            }
            next.clear();
        } else {
            next.push_str(line);
            next.push('\n');
        }
    }
    turns
}

fn rerun(notebooks: &[crate::mcp::Notebook], task: &Path, work: &Path, project: &Path, endeavor: &Path) -> Result<Vec<crate::mcp::Notebook>, String> {
    let fresh = work.join("rerun");
    let copy = fresh.join("project");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::copy(work.join("settings.json"), fresh.join("settings.json")).unwrap();
    if task.join("project").is_dir() {
        copy_dir(&task.join("project"), &copy, &[]);
    }
    let copy = copy.canonicalize().unwrap();
    let mut paths = Vec::new();
    for nb in notebooks {
        let relative = Path::new(&nb.path).strip_prefix(project).map_err(|_| format!("{} isn't in the project folder", nb.path))?;
        let target = copy.join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(&nb.path, &target).map_err(|e| format!("{}: {e}", nb.path))?;
        paths.push((target.display().to_string(), nb.path.clone()));
    }
    let result = crate::mcp::Session::start(endeavor, &fresh, &copy).and_then(|mut s| {
        for (path, _) in &paths {
            crate::mcp::open_and_run(&mut s, path, RUN_LIMIT)?;
        }
        crate::mcp::notebooks(&mut s)
    });
    stop_runtime(endeavor, &fresh);
    let mut again = result?;
    for nb in &mut again {
        if let Some((_, agents)) = paths.iter().find(|(path, _)| Path::new(path) == Path::new(&nb.path)) {
            nb.path = agents.clone();
        }
        for cell in &mut nb.cells {
            cell.output = in_agents_folder(&cell.output, &copy, project);
        }
    }
    Ok(again)
}

fn in_agents_folder(output: &str, copy: &Path, project: &Path) -> String {
    let (copy, project) = (copy.display().to_string(), project.display().to_string());
    let mut out = output.replace(&copy, &project);
    if let Some(short) = copy.strip_prefix("/private") {
        out = out.replace(short, project.strip_prefix("/private").unwrap_or(&project));
    }
    out
}

fn notebooks_json(notebooks: &[crate::mcp::Notebook]) -> Value {
    json!(notebooks.iter().map(|nb| json!({
        "path": nb.path,
        "execution_allowed": nb.execution_allowed,
        "cells": nb.cells.iter().map(|c| json!({ "id": c.id, "code": checks::clip(c.code.lines().next().unwrap_or_default(), 80), "errored": c.errored, "busy": c.busy, "output": checks::clip(&c.output, 300) })).collect::<Vec<_>>(),
    })).collect::<Vec<_>>())
}

#[derive(Default)]
struct AgentRun {
    final_message: String,
    model: Option<String>,
    models_used: Vec<String>,
    tools: Vec<String>,
    turns: Option<u64>,
    cost_usd: Option<f64>,
    tokens: Tokens,
    compactions: u64,
    problem: Option<String>,
}

#[derive(Default)]
struct Tokens {
    requests: u64,
    context: u64,
    input: u64,
    output: u64,
}

fn run_claude(turns: &[String], work: &Path, project: &Path, endeavor: &Path, exe: &Path, repo: &Path, limit: Duration) -> AgentRun {
    let mut transcript = std::fs::File::create(work.join("transcript.jsonl")).unwrap();
    let stderr = std::fs::File::create(work.join("agent-stderr.txt")).unwrap();
    let spawned = Command::new("claude")
        .args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--max-turns", "80"])
        .arg("--plugin-dir")
        .arg(repo.join("claude-plugin"))
        .args(["--tools", &BUILT_IN_TOOLS.join(",")])
        .args(["--allowedTools", &format!("{OUR_SERVER},{}", BUILT_IN_TOOLS.join(","))])
        .args(["--setting-sources", "project,local"])
        .args(MODEL.get().map(|m| ["--model", m.as_str()]).into_iter().flatten())
        .current_dir(project)
        .env_clear()
        .envs(agent_env())
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        .env("ENDEAVOR_BIN", exe)
        .env("SMOKE_ENDEAVOR", endeavor)
        .env("SMOKE_WORK", work)
        .env("SMOKE_PROJECT", project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return AgentRun { problem: Some(format!("can't start claude: {e}")), ..AgentRun::default() },
    };
    use std::io::{BufRead, Write};
    let log = std::sync::Mutex::new(crate::log::Log::create(&work.join("mcp.jsonl")));
    let say = |stdin: &mut std::process::ChildStdin, turn: &str| {
        log.lock().unwrap().write("user", &json!(turn).to_string());
        writeln!(stdin, "{}", json!({ "type": "user", "message": { "role": "user", "content": turn } }))
    };
    let mut stdin = child.stdin.take();
    let mut sent = 0;
    if let Some(stdin) = stdin.as_mut() {
        let _ = say(stdin, &turns[0]);
        sent = 1;
    }
    if sent == turns.len() {
        stdin = None;
    }
    let (lines, events) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let began = Instant::now();
    let mut problem = None;
    let mut answers = 0;
    loop {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => {
                let _ = writeln!(transcript, "{line}");
                if serde_json::from_str::<Value>(&line).is_ok_and(|e| e["type"] == "result") {
                    answers += 1;
                    if let Some(input) = stdin.as_mut().filter(|_| sent < turns.len()) {
                        let _ = say(input, &turns[sent]);
                        sent += 1;
                    }
                    if sent == turns.len() {
                        stdin = None;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if began.elapsed() > limit => {
                let _ = child.kill();
                problem = Some(format!("the agent ran over {} minutes and was ended", limit.as_secs() / 60));
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    drop(stdin);
    let _ = child.wait();
    if problem.is_none() && answers < turns.len() {
        problem = Some(format!("the agent answered {answers} of the user's {} messages", turns.len()));
    }
    let mut run = AgentRun { problem, ..AgentRun::default() };
    let text = std::fs::read_to_string(work.join("transcript.jsonl")).unwrap_or_default();
    let mut last_message: Option<String> = None;
    for event in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "compact_boundary" => run.compactions += 1,
            Some("system") if event["subtype"] == "init" => {
                run.model = run.model.take().or_else(|| event["model"].as_str().map(str::to_owned));
                let ours = event["mcp_servers"].as_array().into_iter().flatten().find(|s| s["name"] == "plugin:endeavor:endeavor");
                let mut problems = isolation_problems(&event);
                if ours.is_none_or(|s| s["status"] != "connected") {
                    problems.insert(0, format!("the plugin's server didn't connect: {}", ours.cloned().unwrap_or(Value::Null)));
                }
                if run.problem.is_none() && !problems.is_empty() {
                    run.problem = Some(problems.join("; "));
                }
            }
            Some("assistant") => {
                if last_message.as_deref() != event["message"]["id"].as_str() {
                    last_message = event["message"]["id"].as_str().map(str::to_owned);
                    let usage = &event["message"]["usage"];
                    let read = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"].iter().map(|k| usage[*k].as_u64().unwrap_or(0)).sum::<u64>();
                    run.tokens.requests += 1;
                    run.tokens.context = read;
                    run.tokens.input += read;
                }
                for part in event["message"]["content"].as_array().into_iter().flatten().filter(|p| p["type"] == "tool_use") {
                    run.tools.push(part["name"].as_str().unwrap_or_default().to_owned());
                }
            }
            Some("result") => {
                run.final_message = event["result"].as_str().unwrap_or_default().to_owned();
                run.turns = Some(run.turns.unwrap_or(0) + event["num_turns"].as_u64().unwrap_or(0));
                run.cost_usd = event["total_cost_usd"].as_f64();
                run.tokens.output = event["modelUsage"].as_object().into_iter().flat_map(|m| m.values()).map(|u| u["outputTokens"].as_u64().unwrap_or(0)).sum();
                for model in event["modelUsage"].as_object().into_iter().flat_map(|m| m.keys()) {
                    if !run.models_used.contains(model) {
                        run.models_used.push(model.clone());
                    }
                }
                if event["is_error"] == true && run.problem.is_none() {
                    run.problem = Some(format!("the agent's run ended in an error: {}", event["subtype"]));
                }
            }
            _ => {}
        }
    }
    let foreign = foreign_tools_used(&text);
    if run.problem.is_none() && !foreign.is_empty() {
        run.problem = Some(format!("the agent used other tools: {foreign:?}"));
    }
    run
}

fn foreign_tools_used(transcript: &str) -> Vec<String> {
    let events: Vec<Value> = transcript.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let parts = |kind: &'static str| {
        events.iter().filter(move |e| e["type"] == kind).flat_map(|e| e["message"]["content"].as_array().cloned().unwrap_or_default())
    };
    let refused: std::collections::HashSet<String> = parts("user")
        .filter(|p| p["type"] == "tool_result" && p["content"].as_str().is_some_and(|c| c.starts_with("<tool_use_error>Error: No such tool available:")))
        .filter_map(|p| p["tool_use_id"].as_str().map(str::to_owned))
        .collect();
    let mut used: Vec<String> = parts("assistant")
        .filter(|p| p["type"] == "tool_use" && !p["id"].as_str().is_some_and(|id| refused.contains(id)))
        .filter_map(|p| p["name"].as_str().map(str::to_owned))
        .filter(|t| !BUILT_IN_TOOLS.contains(&t.as_str()) && !t.starts_with(&format!("{OUR_SERVER}__")))
        .collect();
    used.sort();
    used.dedup();
    used
}

const PRECOMPILE_WAIT: Duration = Duration::from_secs(10 * 60);

fn remove_work(work: &Path) {
    let mut started = None;
    loop {
        let users = processes_naming(work);
        if users.is_empty() {
            break;
        }
        let at = *started.get_or_insert_with(|| {
            eprintln!("   waiting for {} process(es) still using {} (a background precompile, most likely)", users.len(), work.display());
            Instant::now()
        });
        if at.elapsed() > PRECOMPILE_WAIT {
            eprintln!("   still running after {} min, so ending them: {}", PRECOMPILE_WAIT.as_secs() / 60, users.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
            for pid in &users {
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
            std::thread::sleep(Duration::from_secs(5));
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let _ = std::fs::remove_dir_all(work);
}

fn processes_naming(folder: &Path) -> Vec<u32> {
    if cfg!(windows) {
        return Vec::new();
    }
    let Ok(out) = Command::new("ps").args(["axww", "-o", "pid=,args="]).stderr(Stdio::null()).output() else { return Vec::new() };
    let mut names = vec![folder.display().to_string()];
    if let Ok(real) = folder.canonicalize() {
        names.push(real.display().to_string());
    }
    naming(&String::from_utf8_lossy(&out.stdout), &names, std::process::id())
}

fn naming(ps: &str, folders: &[String], own: u32) -> Vec<u32> {
    ps.lines()
        .filter_map(|line| {
            let (pid, args) = line.trim_start().split_once(char::is_whitespace)?;
            let pid: u32 = pid.parse().ok()?;
            let under = folders.iter().any(|f| args.match_indices(f.as_str()).any(|(i, _)| matches!(args[i + f.len()..].chars().next(), None | Some('/') | Some(' ') | Some('"') | Some('\''))));
            (pid != own && under).then_some(pid)
        })
        .collect()
}

fn answer_dialogs(work: &Path, project: &Path, exe: &Path, dialog: &Value) -> Result<(), String> {
    let valid = match dialog {
        Value::String(answer) => matches!(answer.as_str(), "accept" | "decline"),
        Value::Object(o) => o.len() == 1 && (o["new_packages"] == "any" || o["new_packages"].as_array().is_some_and(|p| !p.is_empty() && p.iter().all(Value::is_string))),
        _ => false,
    };
    if !valid {
        return Err(format!("setup.json: dialog must be \"accept\", \"decline\" or {{ \"new_packages\": [names] or \"any\" }}, not {dialog}"));
    }
    std::fs::write(work.join("dialog.json"), dialog.to_string()).map_err(|e| e.to_string())?;
    let settings = project.join(".claude/settings.json");
    let mut current: Value = std::fs::read_to_string(&settings).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    current["hooks"]["Elicitation"] = json!([{ "hooks": [{ "type": "command", "command": format!("'{}' dialog '{}'", exe.display(), work.display()) }] }]);
    std::fs::create_dir_all(project.join(".claude")).map_err(|e| e.to_string())?;
    std::fs::write(&settings, current.to_string()).map_err(|e| e.to_string())
}

pub fn dialog_hook(args: &[String]) -> i32 {
    let Some(work) = args.first().map(PathBuf::from) else {
        eprintln!("usage: endeavor-smoke dialog WORK");
        return 2;
    };
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    let asked: Value = serde_json::from_str(&input).unwrap_or_else(|_| json!(input));
    let policy: Value = std::fs::read_to_string(work.join("dialog.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let (answer, why) = dialog_answer(&policy, asked["message"].as_str().unwrap_or_default());
    use std::io::Write;
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(work.join("dialogs.jsonl")) {
        let _ = writeln!(log, "{}", json!({ "asked": asked, "answer": answer, "why": why }));
    }
    println!("{}", json!({ "hookSpecificOutput": { "hookEventName": "Elicitation", "action": answer, "content": {} } }));
    0
}

fn dialog_answer(policy: &Value, message: &str) -> (&'static str, String) {
    if let Some(answer) = policy.as_str() {
        return (if answer == "accept" { "accept" } else { "decline" }, format!("the task answers every dialog with {answer}"));
    }
    let allowed: Vec<&str> = policy["new_packages"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let Some((language, names)) = ["Julia", "Python"].into_iter().find_map(|language| {
        let rest = message.strip_prefix(language)?.strip_prefix(" needs to install ")?;
        Some((language, rest.split_once(", new to the notebook ")?.0))
    }) else {
        return ("decline", "not a question about new Julia or Python packages".to_owned());
    };
    if policy["new_packages"] == "any" {
        return ("accept", format!("new {language} packages, any the agent picks: {names}"));
    }
    let others: Vec<&str> = names.split(", ").filter(|n| !allowed.contains(n)).collect();
    if others.is_empty() { ("accept", format!("new {language} packages the task allows: {names}")) } else { ("decline", format!("packages the task doesn't allow: {}", others.join(", "))) }
}

fn stop_runtime(endeavor: &Path, work: &Path) {
    stop_state(endeavor, work, &work.join("state"));
}

fn stop_state(endeavor: &Path, work: &Path, state: &Path) {
    let stop = |force: bool| {
        let mut c = Command::new(endeavor);
        c.arg("stop").args(["--state-dir".to_owned(), state.display().to_string()]).envs(runtime_env(work)).stdout(Stdio::null()).stderr(Stdio::null());
        if force {
            c.arg("--force");
        }
        c.status().is_ok_and(|s| s.success())
    };
    if !stop(false) {
        stop(true);
    }
}

fn row(id: &str, attempts: &[Value], expected: Option<&str>) -> Value {
    let failed = attempts.iter().filter(|a| a["passed"] != true).count();
    let status = match expected {
        Some(_) if failed > 0 => "expected failure".to_owned(),
        Some(issue) => format!("pass: {issue} may be fixed"),
        None if failed == 0 => "pass".to_owned(),
        None if failed == attempts.len() => "failing".to_owned(),
        None => "flaky".to_owned(),
    };
    let mut models: Vec<String> = attempts.iter().flat_map(|a| a["models_used"].as_array().cloned().unwrap_or_default()).filter_map(|m| m.as_str().map(str::to_owned)).collect();
    models.sort();
    models.dedup();
    let failures: Vec<String> = attempts
        .iter()
        .filter(|a| a["passed"] != true)
        .flat_map(|a| {
            let problems = a["problems"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|p| p.as_str().map(str::to_owned));
            let checks = a["checks"].as_array().cloned().unwrap_or_default().into_iter().filter(|c| c["passed"] != true && c["soft"] != true).map(|c| format!("{}: {}", c["check"].as_str().unwrap_or_default(), c["detail"].as_str().unwrap_or_default()));
            problems.chain(checks).collect::<Vec<_>>()
        })
        .collect();
    let first = &attempts[0]["metrics"];
    json!({ "task": id, "status": status, "expected_to_fail": expected, "attempts": attempts.len(), "failed": failed, "failures": failures, "first": first, "models": models })
}

fn thousands(n: &Value) -> String {
    n.as_u64().filter(|n| *n > 0).map_or("?".into(), |n| format!("{:.1}k", n as f64 / 1000.0))
}

fn summary_md(summary: &Value) -> String {
    let models: Vec<&str> = summary["models"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let models = if models.is_empty() { "unknown".to_owned() } else { models.join(", ") };
    let mut md = format!("# Smoke run at {}\n\nAgent: {}\n\nModels: {models}\n\n| Task | Status | Failed attempts | Tool calls | Tool errors | Time | Cost | Requests | Input tokens | Output tokens | Compactions | Tool replies |\n|---|---|---|---|---|---|---|---|---|---|---|---|\n", summary["commit"].as_str().unwrap_or("?"), summary["agent"].as_str().unwrap_or("?"));
    for r in summary["tasks"].as_array().into_iter().flatten() {
        let m = &r["first"];
        if r["attempts"] == 0 {
            md += &format!("| {} | {} | 0/0 | | | | | | | | | |\n", r["task"].as_str().unwrap_or_default(), r["status"].as_str().unwrap_or_default());
            continue;
        }
        md += &format!(
            "| {} | {} | {}/{} | {} | {} | {:.0} s | {} | {} | {} | {} | {} | {} |\n",
            r["task"].as_str().unwrap_or_default(),
            r["status"].as_str().unwrap_or_default(),
            r["failed"],
            r["attempts"],
            m["tool_calls"],
            m["tool_errors"],
            m["seconds"].as_f64().unwrap_or(0.0),
            m["cost_usd"].as_f64().map_or("?".into(), |c| format!("${c:.2}")),
            m["requests"],
            thousands(&m["input_tokens"]),
            thousands(&m["output_tokens"]),
            m["compactions"].as_u64().map_or("?".into(), |c| c.to_string()),
            m["reply_bytes"].as_u64().map_or("?".into(), |b| format!("{:.1} KB", b as f64 / 1000.0)),
        );
    }
    md += "\nEach number is from the task's first attempt. Input tokens add up what every request read, cached or not; tool replies are the notebook tools' text, apart from pictures.\n";
    for r in summary["tasks"].as_array().into_iter().flatten().filter(|r| r["failed"] != 0) {
        md += &format!("\n## {}\n\n", r["task"].as_str().unwrap_or_default());
        for f in r["failures"].as_array().into_iter().flatten() {
            md += &format!("- {}\n", checks::clip(f.as_str().unwrap_or_default(), 400));
        }
    }
    md
}

fn claude_version() -> String {
    Command::new("claude").arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).filter(|v| !v.is_empty()).map_or("Claude Code (version unknown)".into(), |v| format!("claude {v}"))
}

fn git(repo: &Path, args: &[&str]) -> String {
    Command::new("git").args(args).current_dir(repo).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into())
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else if let Ok(rel) = entry.path().strip_prefix(dir) {
                out.push(rel.display().to_string());
            }
        }
    }
    out.sort();
    out
}

fn copy_dir(from: &Path, to: &Path, skip: &[&str]) {
    for entry in std::fs::read_dir(from).into_iter().flatten().flatten() {
        if skip.iter().any(|s| entry.file_name() == *s) {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy_dir(&entry.path(), &target, &[]);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}{:02}Z", rest / 3600, rest % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rerun_prints_its_folder_as_the_agents() {
        let (copy, project) = (Path::new("/private/var/f/L1J/attempt-1/rerun/project"), Path::new("/private/var/f/L1J/attempt-1/project"));
        assert_eq!(super::in_agents_folder("/private/var/f/L1J/attempt-1/rerun/project/fig.png", copy, project), "/private/var/f/L1J/attempt-1/project/fig.png");
        assert_eq!(super::in_agents_folder("\"/var/f/L1J/attempt-1/rerun/project/fig.png\"", copy, project), "\"/var/f/L1J/attempt-1/project/fig.png\"");
        assert_eq!(super::in_agents_folder("74.08", copy, project), "74.08");
    }

    #[test]
    fn a_task_folder_waits_for_the_processes_that_name_a_path_in_it() {
        let ps = "  101 /usr/bin/nice -n 10 julia --startup-file=no --project=/tmp/smoke/N1/attempt-1/home/xdg_cache_home/endeavor/serve/abc/runtime -e x\n\
                    102 julia --project=/tmp/smoke/N1/attempt-10/runtime\n\
                    103 julia --project=/private/tmp/smoke/N1/attempt-1\n\
                    104 endeavor-smoke run --only N1\n\
                    105 cat /tmp/smoke/N1/attempt-1/log.txt\n";
        let folders = ["/tmp/smoke/N1/attempt-1".to_owned(), "/private/tmp/smoke/N1/attempt-1".to_owned()];
        assert_eq!(naming(ps, &folders, 105), vec![101, 103]);
        assert!(naming("", &folders, 1).is_empty());
    }

    #[test]
    fn a_task_expected_to_fail_doesnt_fail_the_run_and_says_when_it_passes() {
        let passed = json!({ "passed": true, "metrics": {}, "models_used": ["m"] });
        let failed = json!({ "passed": false, "metrics": {}, "models_used": ["m", "n"] });
        assert_eq!(row("N1", std::slice::from_ref(&passed), None)["status"], "pass");
        assert_eq!(row("N1", &[failed.clone(), passed.clone()], None)["status"], "flaky");
        assert_eq!(row("N1", &[failed.clone(), failed.clone()], None)["status"], "failing");
        assert_eq!(row("N9", std::slice::from_ref(&failed), Some("#58"))["status"], "expected failure");
        assert_eq!(row("N9", &[passed], Some("#58"))["status"], "pass: #58 may be fixed");
        assert_eq!(row("N1", &[failed], None)["models"], json!(["m", "n"]));
    }

    #[test]
    fn a_tool_the_agent_hasnt_got_is_its_mistake_but_another_tool_that_ran_is_a_leak() {
        let call = |id: &str, name: &str| json!({ "type": "assistant", "message": { "id": id, "content": [{ "type": "tool_use", "id": id, "name": name, "input": {} }] } }).to_string();
        let answer = |id: &str, content: &str| json!({ "type": "user", "message": { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": content, "is_error": content.starts_with('<') }] } }).to_string();
        let transcript = [
            call("t1", "Bash"),
            answer("t1", "<tool_use_error>Error: No such tool available: Bash</tool_use_error>"),
            call("t2", "mcp__plugin_endeavor_endeavor__new_notebook"),
            answer("t2", "{}"),
            call("t3", "Read"),
            answer("t3", "text"),
            call("t4", "mcp__claude_ai_x__y"),
            answer("t4", "done"),
        ]
        .join("\n");
        assert_eq!(foreign_tools_used(&transcript), ["mcp__claude_ai_x__y"]);
        assert!(foreign_tools_used(&transcript.replace("mcp__claude_ai_x__y", "Read")).is_empty());
    }

    #[test]
    fn a_reproducible_check_inside_any_of_or_all_of_asks_for_the_rerun() {
        assert!(wants_reproducible(&json!({ "check": "reproducible" })));
        let nested = json!({ "check": "any_of", "checks": [{ "check": "all_of", "checks": [{ "check": "notebooks" }, { "check": "reproducible" }] }] });
        assert!(wants_reproducible(&nested));
        assert!(!wants_reproducible(&json!({ "check": "any_of", "checks": [{ "check": "notebooks" }] })));
        let n8: Value = serde_json::from_str(include_str!("../../../smoke/tasks/N8-one-notebook/checks.json")).unwrap();
        assert!(n8["checks"].as_array().unwrap().iter().any(wants_reproducible));
    }

    #[test]
    fn followups_are_one_message_each_between_lines_of_dashes() {
        assert_eq!(messages(" Hi.\n", ""), ["Hi."]);
        assert_eq!(messages("Hi.", "Next.\n"), ["Hi.", "Next."]);
        assert_eq!(messages("Hi.", "Two\nlines.\n---\nThird.\r\n---\n"), ["Hi.", "Two\nlines.", "Third."]);
    }

    #[test]
    fn an_r_task_without_r_fails_unless_skip_r_is_given() {
        let failing = no_r_row("R1", "auto", false);
        assert_eq!(failing["status"], "failing");
        assert!(failing["failures"][0].as_str().unwrap().contains("--skip-r"));
        assert_eq!(no_r_row("R1", "/opt/R/bin/Rscript", true)["status"], SKIPPED_NO_R);
        let md = summary_md(&json!({ "commit": "c", "agent": "a", "models": [], "tasks": [failing] }));
        assert!(md.contains("| R1 | failing | 0/0 |") && md.contains("no R here"), "{md}");
        assert!(!have_r("/no/such/Rscript"));
    }

    #[test]
    fn the_user_says_yes_only_to_the_new_julia_packages_the_task_allows() {
        let policy = json!({ "new_packages": ["Plots", "PlotlyJS"] });
        let ask = |names: &str| format!("Julia needs to install {names}, new to the notebook /p/plot.jl, before its cells can run. Install it now?");
        assert_eq!(super::dialog_answer(&policy, &ask("Plots")).0, "accept");
        assert_eq!(super::dialog_answer(&policy, &ask("PlotlyJS, Plots")).0, "accept");
        let (answer, why) = super::dialog_answer(&policy, &ask("Plots, Makie"));
        assert!(answer == "decline" && why.contains("Makie") && !why.contains("Plots"), "{why}");
        assert_eq!(super::dialog_answer(&policy, "R needs to download pkg (2 GB) before the cells of the notebook /p/a.R can run. Download it now?").0, "decline", "not an R download");
        assert_eq!(super::dialog_answer(&policy, "Endeavor needs to install Endeavor's helper (about 20 MB) on lab. Install it now?").0, "decline", "nor a server's helper");
        let any = json!({ "new_packages": "any" });
        assert_eq!(super::dialog_answer(&any, &ask("Optim, DataInterpolations")).0, "accept");
        assert_eq!(super::dialog_answer(&any, "R needs to download pkg (2 GB) before the cells of the notebook /p/a.R can run. Download it now?").0, "decline", "any package, but still only the new-package question");
        assert_eq!(super::dialog_answer(&any, "Endeavor needs to install Endeavor's helper (about 20 MB) on lab. Install it now?").0, "decline");
        let python = |names: &str| format!("Python needs to install {names}, new to the notebook /p/growth.py, before its cells can run. Install it now?");
        assert_eq!(super::dialog_answer(&any, &python("pandas, matplotlib")).0, "accept", "a Python notebook's new packages too");
        assert_eq!(super::dialog_answer(&policy, &python("Plots")).0, "accept");
        assert_eq!(super::dialog_answer(&policy, &python("pandas")).0, "decline", "but only the ones a list allows");
        assert_eq!(super::dialog_answer(&json!("accept"), "Endeavor needs to install Endeavor's helper on lab. Install it now?").0, "accept");
        assert_eq!(super::dialog_answer(&json!("decline"), &ask("Plots")).0, "decline");
    }

    #[test]
    fn a_dialog_answer_must_be_one_the_runner_knows() {
        let work = std::env::temp_dir().join(format!("smoke-dialog-test-{}", std::process::id()));
        let project = work.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let exe = Path::new("/bin/endeavor-smoke");
        for bad in [json!("maybe"), json!({ "new_packages": [] }), json!({ "new_packages": "Plots" }), json!({ "new_packages": "all" }), json!({ "new_packages": ["Plots"], "also": 1 }), json!(true)] {
            assert!(super::answer_dialogs(&work, &project, exe, &bad).is_err(), "{bad}");
        }
        super::answer_dialogs(&work, &project, exe, &json!({ "new_packages": ["Plots"] })).unwrap();
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(project.join(".claude/settings.json")).unwrap()).unwrap();
        let command = settings["hooks"]["Elicitation"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.starts_with("'/bin/endeavor-smoke' dialog '"), "{command}");
        assert_eq!(std::fs::read_to_string(work.join("dialog.json")).unwrap(), r#"{"new_packages":["Plots"]}"#);
        let _ = std::fs::remove_dir_all(&work);
    }
}

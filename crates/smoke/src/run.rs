//! The runner: for each task, a fresh state and project folder, the agent
//! started on the task's prompt with the plugin and this binary as its
//! server, then the checks, then the runtime stopped. A task that fails runs
//! twice more, to tell a flaky failure from a real one.
//!
//! The agent runs in a folder under the system's temporary folder, outside any
//! checkout, so it sees no CLAUDE.md of ours; with an environment of its own,
//! so it can't reach the session that started the run; and with only the
//! notebook tools and the file-reading ones. Its first event is checked for
//! that before the task counts.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::checks::{self, Evidence};

/// How long one agent run may take before it is ended.
/// The model the agent runs (`--model`), if not its default.
static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

const AGENT_LIMIT: Duration = Duration::from_secs(20 * 60);

/// The built-in tools the agent has.
const BUILT_IN_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Skill", "ToolSearch"];

/// The plugin's MCP server, as Claude Code names its tools (`mcp__plugin_endeavor_endeavor__new_notebook`).
const OUR_SERVER: &str = "mcp__plugin_endeavor_endeavor";

/// The variables the agent keeps from this process's environment: enough to
/// run, sign in and reach the network. Anything else (a Claude Code session's
/// own variables above all, which would connect the agent to that session) is
/// left out.
fn agent_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR", "TZ",
        "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "no_proxy", "ALL_PROXY", "all_proxy",
        "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE",
        "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN",
        // Windows.
        "SYSTEMROOT", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "HOMEDRIVE", "HOMEPATH",
    ];
    std::env::vars().filter(|(k, _)| KEEP.contains(&k.as_str())).collect()
}

/// What in the agent's first event says the run isn't the isolated one this
/// harness promises: a tool beyond ours, a plugin or server beyond the
/// plugin's, or the starting session's memory.
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
    // Claude Code's own built-in plugins come along; any other doesn't belong.
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
    depot: Option<String>,
    retries: u32,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut o = Options { only: None, out: None, julia: std::env::var("ENDEAVOR_E2E_JULIA").unwrap_or_else(|_| "auto".into()), depot: None, retries: 2 };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--only" => o.only = Some(value()?.split(',').map(|s| s.trim().to_owned()).collect()),
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--julia" => o.julia = value()?,
            "--depot" => o.depot = Some(value()?),
            "--retries" => o.retries = value()?.parse().map_err(|e| format!("--retries: {e}"))?,
            "--model" => MODEL.set(value()?).map_err(|_| "--model given twice".to_owned())?,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(o)
}

/// The repository this binary was built from: the tasks and the plugin are there.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository")
}

/// The arguments the agent's server and the checker's both get, so they share one runtime.
pub fn runtime_args(work: &Path) -> Vec<String> {
    let settings: Value = std::fs::read_to_string(work.join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut args = vec!["--state-dir".to_owned(), work.join("state").display().to_string()];
    for key in ["julia", "depot"] {
        if let Some(v) = settings[key].as_str() {
            args.extend([format!("--{key}"), v.to_owned()]);
        }
    }
    args
}

/// The runtime's own folders, inside the task's.
pub fn runtime_env(work: &Path) -> Vec<(String, PathBuf)> {
    ["XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME"].iter().map(|k| (k.to_string(), work.join("home").join(k.to_lowercase()))).collect()
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
    let commit = git(&repo, &["rev-parse", "--short", "HEAD"]);
    let out = o.out.clone().unwrap_or_else(|| repo.join("target/smoke").join(format!("{}-{commit}", utc_stamp())));
    // The agent runs in each task's project folder, so every path it is given is absolute.
    std::fs::create_dir_all(&out).unwrap();
    let out = out.canonicalize().unwrap();
    let depot = o.depot.clone().unwrap_or_else(|| format!("{}:", repo.join("target/smoke/depot").display()));
    let agent = claude_version();
    let mut tasks: Vec<PathBuf> = std::fs::read_dir(repo.join("smoke/tasks")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.join("prompt.md").is_file()).collect();
    tasks.sort();
    if let Some(only) = &o.only {
        tasks.retain(|t| only.iter().any(|id| t.file_name().is_some_and(|n| n == id.as_str())));
    }
    if tasks.is_empty() {
        eprintln!("endeavor-smoke: no tasks to run");
        return 2;
    }
    eprintln!("endeavor-smoke: {} task(s), results in {}", tasks.len(), out.display());
    let scratch = std::env::temp_dir().join(format!("endeavor-smoke-{}-{}", utc_stamp(), std::process::id()));
    let mut rows = Vec::new();
    for task in &tasks {
        let id = task.file_name().unwrap().to_string_lossy().into_owned();
        let mut attempts = Vec::new();
        loop {
            let n = attempts.len() + 1;
            eprintln!("── {id}, attempt {n}");
            let work = scratch.join(&id).join(format!("attempt-{n}"));
            let result = attempt(&id, task, &work, &endeavor, &exe, &repo, &o.julia, &depot);
            eprintln!("   {} in {:.0}s", if result["passed"] == true { "passed" } else { "FAILED" }, result["metrics"]["seconds"].as_f64().unwrap_or(0.0));
            let _ = std::fs::write(work.join("result.json"), serde_json::to_string_pretty(&result).unwrap());
            // Keep what the attempt left, not the runtime's own folders.
            let kept = out.join(&id).join(format!("attempt-{n}"));
            let _ = std::fs::remove_dir_all(&kept);
            std::fs::create_dir_all(&kept).unwrap();
            copy_dir(&work, &kept, &["state", "home"]);
            let _ = std::fs::remove_dir_all(&work);
            let passed = result["passed"] == true;
            attempts.push(result);
            // A first run that passes is enough; one that fails runs `retries` more times.
            if (n == 1 && passed) || n as u32 > o.retries {
                break;
            }
        }
        rows.push(row(&id, &attempts));
    }
    let _ = std::fs::remove_dir_all(&scratch);
    let summary = json!({ "commit": commit, "agent": agent, "tasks": rows });
    let _ = std::fs::write(out.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap());
    let md = summary_md(&summary);
    let _ = std::fs::write(out.join("summary.md"), &md);
    println!("{md}");
    if rows.iter().all(|r| r["status"] == "pass") { 0 } else { 1 }
}

#[allow(clippy::too_many_arguments)]
fn attempt(id: &str, task: &Path, work: &Path, endeavor: &Path, exe: &Path, repo: &Path, julia: &str, depot: &str) -> Value {
    let _ = std::fs::remove_dir_all(work);
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    if task.join("project").is_dir() {
        copy_dir(&task.join("project"), &project, &[]);
    }
    let project = project.canonicalize().unwrap();
    std::fs::write(work.join("settings.json"), json!({ "julia": julia, "depot": depot }).to_string()).unwrap();
    let prompt = std::fs::read_to_string(task.join("prompt.md")).unwrap().trim().to_owned();
    let specs: Value = std::fs::read_to_string(task.join("checks.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({ "checks": [] }));

    let began = Instant::now();
    let agent = run_claude(&prompt, work, &project, endeavor, exe, repo);
    let seconds = began.elapsed().as_secs_f64();
    let calls = crate::log::calls(&work.join("mcp.jsonl"));
    let read = crate::mcp::Session::start(endeavor, work, &project).and_then(|mut s| crate::mcp::notebooks(&mut s));
    stop_runtime(endeavor, work);

    let (notebooks, read_error) = match read {
        Ok(n) => (n, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let ev = Evidence { calls: &calls, notebooks: &notebooks, agent_tools: &agent.tools, final_message: &agent.final_message };
    let outcomes: Vec<checks::Outcome> = specs["checks"].as_array().into_iter().flatten().map(|spec| checks::run(spec, &ev)).collect();
    let mut problems: Vec<String> = Vec::new();
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
        },
        "final_message": agent.final_message,
    })
}

struct AgentRun {
    final_message: String,
    tools: Vec<String>,
    turns: Option<u64>,
    cost_usd: Option<f64>,
    problem: Option<String>,
}

/// Claude Code, headless, with the plugin from this checkout and this binary as its server.
fn run_claude(prompt: &str, work: &Path, project: &Path, endeavor: &Path, exe: &Path, repo: &Path) -> AgentRun {
    let transcript = std::fs::File::create(work.join("transcript.jsonl")).unwrap();
    let stderr = std::fs::File::create(work.join("agent-stderr.txt")).unwrap();
    let spawned = Command::new("claude")
        .args(["-p", prompt, "--output-format", "stream-json", "--verbose", "--max-turns", "80"])
        .arg("--plugin-dir")
        .arg(repo.join("claude-plugin"))
        // The only built-in tools there are: reading files, and the two that load skills and deferred tools.
        .args(["--tools", &BUILT_IN_TOOLS.join(",")])
        // Approved without asking: those, and the plugin's server.
        .args(["--allowedTools", &format!("{OUR_SERVER},{}", BUILT_IN_TOOLS.join(","))])
        // Only the project's settings: the user's own plugins, servers and hooks stay out of the run.
        .args(["--setting-sources", "project,local"])
        .args(MODEL.get().map(|m| ["--model", m.as_str()]).into_iter().flatten())
        .current_dir(project)
        .env_clear()
        .envs(agent_env())
        .env("ENDEAVOR_BIN", exe)
        .env("SMOKE_ENDEAVOR", endeavor)
        .env("SMOKE_WORK", work)
        .stdin(Stdio::null())
        .stdout(transcript)
        .stderr(stderr)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return AgentRun { final_message: String::new(), tools: Vec::new(), turns: None, cost_usd: None, problem: Some(format!("can't start claude: {e}")) },
    };
    let began = Instant::now();
    let mut problem = None;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if began.elapsed() > AGENT_LIMIT => {
                let _ = child.kill();
                let _ = child.wait();
                problem = Some(format!("the agent ran over {} minutes and was ended", AGENT_LIMIT.as_secs() / 60));
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => {
                problem = Some(e.to_string());
                break;
            }
        }
    }
    let mut run = AgentRun { final_message: String::new(), tools: Vec::new(), turns: None, cost_usd: None, problem };
    let text = std::fs::read_to_string(work.join("transcript.jsonl")).unwrap_or_default();
    for event in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match event["type"].as_str() {
            Some("system") if event["subtype"] == "init" => {
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
                for part in event["message"]["content"].as_array().into_iter().flatten().filter(|p| p["type"] == "tool_use") {
                    run.tools.push(part["name"].as_str().unwrap_or_default().to_owned());
                }
            }
            Some("result") => {
                run.final_message = event["result"].as_str().unwrap_or_default().to_owned();
                run.turns = event["num_turns"].as_u64();
                run.cost_usd = event["total_cost_usd"].as_f64();
                if event["is_error"] == true && run.problem.is_none() {
                    run.problem = Some(format!("the agent's run ended in an error: {}", event["subtype"]));
                }
            }
            _ => {}
        }
    }
    run
}

fn stop_runtime(endeavor: &Path, work: &Path) {
    let stop = |force: bool| {
        let mut c = Command::new(endeavor);
        c.arg("stop").args(["--state-dir".to_owned(), work.join("state").display().to_string()]).envs(runtime_env(work)).stdout(Stdio::null()).stderr(Stdio::null());
        if force {
            c.arg("--force");
        }
        c.status().is_ok_and(|s| s.success())
    };
    if !stop(false) {
        stop(true);
    }
}

/// A task's line in the summary: pass on the first try, flaky if some attempts failed, failing if all did.
fn row(id: &str, attempts: &[Value]) -> Value {
    let failed = attempts.iter().filter(|a| a["passed"] != true).count();
    let status = if failed == 0 { "pass" } else if failed == attempts.len() { "failing" } else { "flaky" };
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
    json!({ "task": id, "status": status, "attempts": attempts.len(), "failed": failed, "failures": failures, "first": first })
}

fn summary_md(summary: &Value) -> String {
    let mut md = format!("# Smoke run at {}\n\nAgent: {}\n\n| Task | Status | Failed attempts | Tool calls | Tool errors | Time | Cost |\n|---|---|---|---|---|---|---|\n", summary["commit"].as_str().unwrap_or("?"), summary["agent"].as_str().unwrap_or("?"));
    for r in summary["tasks"].as_array().into_iter().flatten() {
        let m = &r["first"];
        md += &format!(
            "| {} | {} | {}/{} | {} | {} | {:.0} s | {} |\n",
            r["task"].as_str().unwrap_or_default(),
            r["status"].as_str().unwrap_or_default(),
            r["failed"],
            r["attempts"],
            m["tool_calls"],
            m["tool_errors"],
            m["seconds"].as_f64().unwrap_or(0.0),
            m["cost_usd"].as_f64().map_or("?".into(), |c| format!("${c:.2}")),
        );
    }
    md += "\nTime, tool calls and cost are from each task's first attempt.\n";
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
        + &MODEL.get().map(|m| format!(", model {m}")).unwrap_or_default()
}

fn git(repo: &Path, args: &[&str]) -> String {
    Command::new("git").args(args).current_dir(repo).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into())
}

/// Copy a folder's contents, leaving out the top-level entries named in `skip`.
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

/// The time now as `2026-10-10T0215Z`, for a results folder's name.
fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
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

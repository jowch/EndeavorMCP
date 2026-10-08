//! Slurm, as the app and the helper both see it: the resources a session asks
//! for and the `sbatch` arguments they become, a pasted `salloc` line, the
//! partitions `sinfo` lists, a job as `squeue` shows it, and Slurm's durations.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// What a session's job asks for. `mem_gb` 0 leaves memory to the cluster's
/// default (a pasted `--mem-per-cpu`, kept in `extra`, sets it instead).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Resources {
    /// None is the cluster's default partition.
    pub partition: Option<String>,
    pub cpus: u32,
    pub mem_gb: u32,
    pub minutes: u32,
    pub gres: Option<String>,
    /// Flags from a pasted line that Endeavor has no control for, passed to
    /// `sbatch` as they are.
    pub extra: Vec<String>,
}

impl Default for Resources {
    fn default() -> Self {
        Resources::preset(1)
    }
}

/// Small, Medium, Large: (name, CPUs, GB, minutes).
pub const PRESETS: [(&str, u32, u32, u32); 3] = [("Small", 2, 8, 120), ("Medium", 8, 32, 480), ("Large", 32, 128, 1440)];

impl Resources {
    pub fn preset(i: usize) -> Resources {
        let (_, cpus, mem_gb, minutes) = PRESETS[i];
        Resources { partition: None, cpus, mem_gb, minutes, gres: None, extra: Vec::new() }
    }

    /// This, with a preset's size (partition and the rest kept).
    pub fn sized_as(&self, i: usize) -> Resources {
        let (_, cpus, mem_gb, minutes) = PRESETS[i];
        Resources { cpus, mem_gb, minutes, ..self.clone() }
    }

    /// Keep within what `partition` offers (its nodes' CPUs and memory, its time limit).
    pub fn clip(&mut self, partition: Option<&Partition>) {
        self.cpus = self.cpus.max(1);
        self.minutes = self.minutes.max(1);
        let Some(p) = partition else { return };
        if let Some(max) = p.max_minutes {
            self.minutes = self.minutes.min(max);
        }
        if p.cpus > 0 {
            self.cpus = self.cpus.min(p.cpus);
        }
        if p.mem_gb() > 0 {
            self.mem_gb = self.mem_gb.min(p.mem_gb());
        }
    }

    /// "8 CPUs · 32 GB · 8 h"
    pub fn summary(&self) -> String {
        let cpus = if self.cpus == 1 { "1 CPU".to_owned() } else { format!("{} CPUs", self.cpus) };
        let mut parts = vec![cpus];
        if self.mem_gb > 0 {
            parts.push(format!("{} GB", self.mem_gb));
        }
        parts.push(duration_text(self.minutes));
        if let Some(gres) = &self.gres {
            parts.push(gres.clone());
        }
        parts.join(" · ")
    }

    /// The `sbatch` flags for these resources.
    pub fn sbatch_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(p) = &self.partition {
            args.push(format!("--partition={p}"));
        }
        args.push(format!("--cpus-per-task={}", self.cpus.max(1)));
        if self.mem_gb > 0 {
            args.push(format!("--mem={}G", self.mem_gb));
        }
        args.push(format!("--time={}", self.minutes.max(1)));
        if let Some(gres) = &self.gres {
            args.push(format!("--gres={gres}"));
        }
        args.extend(self.extra.iter().cloned());
        args
    }
}

/// Why `flag` can't be an extra flag for `sbatch`, if it can't: it must start with `-` (a bare
/// word would be taken as the script to run, so a flag and its value are one entry, as
/// `--constraint=a100` or `-N2`), hold no line break or NUL, and not be `--wrap`, which gives
/// `sbatch` a command to run in place of the script. `sbatch` takes an unambiguous start of a
/// long option for the option, so `--wr` and `--wra` count as `--wrap`.
pub fn check_extra_flag(flag: &str) -> Result<(), String> {
    let shown = flag.escape_debug();
    if !flag.starts_with('-') {
        return Err(format!("\"{shown}\" doesn't start with \"-\": a bare word would be taken as the script to run. Write a flag and its value as one entry, such as \"--constraint=a100\"."));
    }
    if flag.contains(['\n', '\r', '\0']) {
        return Err(format!("\"{shown}\" holds a line break or NUL character, which a flag can't have."));
    }
    let name = flag.split_once('=').map_or(flag, |(name, _)| name);
    if name.len() >= 4 && "--wrap".starts_with(name) {
        return Err(format!("\"{shown}\" is --wrap, which gives sbatch a command to run in place of Endeavor's script, so it isn't allowed."));
    }
    Ok(())
}

/// "45 min", "8 h", "1 h 30 min"
pub fn duration_text(minutes: u32) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// What the app asks the helper to submit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobRequest {
    pub resources: Resources,
    pub account: Option<String>,
    /// Where Julia keeps packages; None is `$SCRATCH/endeavor/depot` when the
    /// cluster sets `$SCRATCH`, else `~/.cache/endeavor/depot`.
    pub depot: Option<String>,
}

impl JobRequest {
    /// `sbatch_args`, or why the extra flags can't be passed on (`check_extra_flag`).
    pub fn checked_sbatch_args(&self) -> Result<Vec<String>, String> {
        self.resources.extra.iter().try_for_each(|flag| check_extra_flag(flag))?;
        Ok(self.sbatch_args())
    }

    pub fn sbatch_args(&self) -> Vec<String> {
        let mut args = self.resources.sbatch_args();
        if let Some(account) = &self.account {
            args.insert(0, format!("--account={account}"));
        }
        args
    }
}

/// The runtime's job, once it runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub node: String,
    /// When Slurm will end it (Unix seconds); None without a time limit.
    pub ends_at: Option<u64>,
    /// How the login node reaches the compute node: "srun" or "ssh".
    pub route: String,
}

/// A partition as `sinfo` lists it; a partition with several kinds of node
/// shows its largest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Partition {
    pub name: String,
    /// Jobs go here unless they say otherwise.
    pub default: bool,
    /// None: no time limit.
    pub max_minutes: Option<u32>,
    /// CPUs and memory (MB) per node.
    pub cpus: u32,
    pub mem_mb: u64,
}

impl Partition {
    pub fn mem_gb(&self) -> u32 {
        (self.mem_mb / 1024) as u32
    }
}

/// The `sinfo` format [`parse_sinfo`] reads.
pub const SINFO_FORMAT: &str = "%P|%l|%c|%m";

/// `sinfo -h -o SINFO_FORMAT`, one line per partition and kind of node.
pub fn parse_sinfo(text: &str) -> Vec<Partition> {
    let mut partitions: Vec<Partition> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.trim().split('|').collect();
        let [name, limit, cpus, mem] = fields[..] else { continue };
        let default = name.ends_with('*');
        let name = name.trim_end_matches('*').to_owned();
        if name.is_empty() {
            continue;
        }
        let cpus = cpus.trim_end_matches('+').parse().unwrap_or(0);
        let mem_mb = mem.trim_end_matches('+').parse().unwrap_or(0);
        let max_minutes = parse_duration(limit).map(|s| (s / 60) as u32);
        match partitions.iter_mut().find(|p| p.name == name) {
            Some(p) => {
                p.cpus = p.cpus.max(cpus);
                p.mem_mb = p.mem_mb.max(mem_mb);
            }
            None => partitions.push(Partition { name, default, max_minutes, cpus, mem_mb }),
        }
    }
    partitions
}

/// A Slurm duration in seconds: "minutes", "minutes:seconds",
/// "hours:minutes:seconds", "days-hours", "days-hours:minutes",
/// "days-hours:minutes:seconds". None for "UNLIMITED", "INVALID" and the like.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, rest) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, Some(rest)),
        None => (0, None),
    };
    let numbers = |s: &str| s.split(':').map(|n| n.parse::<u64>().ok()).collect::<Option<Vec<u64>>>();
    let seconds = match rest {
        Some(rest) => match numbers(rest)?[..] {
            [h] => h * 3600,
            [h, m] => h * 3600 + m * 60,
            [h, m, s] => h * 3600 + m * 60 + s,
            _ => return None,
        },
        None => match numbers(text)?[..] {
            [m] => m * 60,
            [m, s] => m * 60 + s,
            [h, m, s] => h * 3600 + m * 60 + s,
            _ => return None,
        },
    };
    Some(days * 86400 + seconds)
}

/// The `squeue` format [`parse_squeue`] reads.
pub const SQUEUE_FORMAT: &str = "%T|%r|%N|%L";

/// A job as `squeue` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queued {
    /// PENDING, CONFIGURING, RUNNING, COMPLETING, …
    pub state: String,
    /// Why it's pending ("Priority", "Resources"); "None" while it runs.
    pub reason: String,
    pub node: String,
    /// Seconds until its time limit, if it has one.
    pub left: Option<u64>,
}

impl Queued {
    pub fn pending(&self) -> bool {
        matches!(self.state.as_str(), "PENDING" | "CONFIGURING" | "REQUEUED" | "RESIZING" | "SUSPENDED")
    }

    pub fn running(&self) -> bool {
        self.state == "RUNNING"
    }
}

/// `squeue -h -j JOB -o SQUEUE_FORMAT`; None when the job isn't listed.
pub fn parse_squeue(text: &str) -> Option<Queued> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let fields: Vec<&str> = line.split('|').collect();
    let [state, reason, node, left] = fields[..] else { return None };
    Some(Queued { state: state.to_owned(), reason: reason.to_owned(), node: node.to_owned(), left: parse_duration(left) })
}

/// A pending job's reason in plain words.
pub fn reason_text(reason: &str) -> Option<String> {
    let text = match reason {
        "" | "None" => return None,
        "Priority" => "other jobs are ahead in the queue",
        "Resources" => "waiting for a node with enough free CPUs and memory",
        "JobHeldUser" => "held (scontrol hold); release it to go on",
        "JobHeldAdmin" => "held by an administrator",
        "QOSMaxJobsPerUserLimit" | "AssocMaxJobsLimit" | "QOSMaxSubmitJobPerUserLimit" => "you're at your limit of running jobs",
        "ReqNodeNotAvail" => "the nodes it needs are unavailable (maintenance?)",
        "PartitionTimeLimit" => "the time limit is longer than the partition allows",
        "BeginTime" => "waiting for its start time",
        "Dependency" => "waiting for another job",
        other => return Some(other.to_owned()),
    };
    Some(text.to_owned())
}

/// Why a job ended, in plain words, from its final state (`sacct`'s State,
/// e.g. "TIMEOUT" or "CANCELLED by 1000").
pub fn ended_text(state: &str) -> Option<&'static str> {
    let state = state.split_whitespace().next().unwrap_or_default().trim_end_matches('+');
    Some(match state {
        "TIMEOUT" => "Its Slurm job reached its time limit.",
        "PREEMPTED" => "The cluster preempted its Slurm job for other work.",
        "CANCELLED" => "Its Slurm job was cancelled.",
        "NODE_FAIL" => "The node its Slurm job ran on failed.",
        "OUT_OF_MEMORY" => "Its Slurm job ran out of memory.",
        "FAILED" => "Its Slurm job failed.",
        "BOOT_FAIL" => "The node for its Slurm job didn't boot.",
        "DEADLINE" => "Its Slurm job reached its deadline.",
        _ => return None,
    })
}

/// A pasted `salloc` (or `srun`, `sbatch`) line applied over `base`: the
/// resources and the account it sets. Flags Endeavor has no control for are
/// kept in `extra`; a command at the end (`bash`) is ignored.
pub fn parse_salloc(line: &str, base: &Resources) -> Result<(Resources, Option<String>), String> {
    let words = shell_words(line)?;
    let mut words = words.into_iter().peekable();
    if words.peek().is_some_and(|w| ["salloc", "srun", "sbatch"].contains(&w.as_str())) {
        words.next();
    }
    let mut r = Resources { extra: Vec::new(), ..base.clone() };
    let mut account = None;
    let mut any = false;
    while let Some(word) = words.next() {
        if !word.starts_with('-') || word == "-" {
            break;
        }
        // --name=value, --name value, -xvalue, -x value
        let (flag, inline) = match word.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ if !word.starts_with("--") && word.len() > 2 => (word[..2].to_owned(), Some(word[2..].to_owned())),
            _ => (word.clone(), None),
        };
        let known = ["-p", "--partition", "-c", "--cpus-per-task", "--mem", "-t", "--time", "-A", "--account", "--gres"];
        if known.contains(&flag.as_str()) {
            let value = match inline {
                Some(v) => v,
                None => words.next().ok_or_else(|| format!("{flag} needs a value."))?,
            };
            match flag.as_str() {
                "-p" | "--partition" => r.partition = Some(value),
                "-c" | "--cpus-per-task" => r.cpus = value.parse().map_err(|_| format!("\"{value}\" isn't a number of CPUs."))?,
                "--mem" => r.mem_gb = parse_mem_gb(&value).ok_or_else(|| format!("\"{value}\" isn't an amount of memory."))?,
                "-t" | "--time" => {
                    let seconds = parse_duration(&value).ok_or_else(|| format!("\"{value}\" isn't a time limit."))?;
                    r.minutes = seconds.div_ceil(60) as u32;
                }
                "-A" | "--account" => account = Some(value),
                _ => r.gres = Some(value),
            }
            any = true;
            continue;
        }
        if ["--mem-per-cpu", "--mem-per-gpu"].contains(&flag.as_str()) {
            r.mem_gb = 0;
        }
        // Interactive-only flags mean nothing to a batch job.
        if ["--pty", "--x11", "-I", "--immediate", "--no-shell"].contains(&flag.as_str()) {
            continue;
        }
        let boolean = BOOLEAN_FLAGS.contains(&flag.as_str());
        if inline.is_none() && !boolean && words.peek().is_some_and(|next| !next.starts_with('-')) && !looks_like_command(words.peek().unwrap()) {
            // One entry for a flag and its value: a bare word in `extra` would be taken for the script.
            let value = words.next().unwrap();
            r.extra.push(if flag.starts_with("--") { format!("{flag}={value}") } else { format!("{flag}{value}") });
        } else {
            r.extra.push(word.clone());
        }
        any = true;
    }
    if !any {
        return Err("That line has no salloc options (like -p, -c, --mem or -t).".into());
    }
    Ok((r, account))
}

const BOOLEAN_FLAGS: [&str; 18] = [
    "--exclusive", "--overcommit", "-O", "--contiguous", "--quiet", "-Q", "--verbose", "-v", "--spread-job", "--use-min-nodes",
    "--no-kill", "-k", "--hold", "-H", "--requeue", "--no-requeue", "--oversubscribe", "-s",
];

fn looks_like_command(word: &str) -> bool {
    ["bash", "sh", "zsh", "srun", "julia", "$SHELL"].contains(&word)
}

/// "32G" -> 32, "32000" (MB, Slurm's default unit) -> 31, "1T" -> 1024.
fn parse_mem_gb(text: &str) -> Option<u32> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(text.len()));
    let n: f64 = number.parse().ok()?;
    let gb = match unit.to_ascii_uppercase().trim_end_matches('B') {
        "" | "M" => n / 1024.,
        "K" => n / 1024. / 1024.,
        "G" => n,
        "T" => n * 1024.,
        _ => return None,
    };
    Some(gb.round().max(if n > 0. { 1. } else { 0. }) as u32)
}

/// Split a command line the way `sh` would, for quotes and backslashes only.
fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("The line has an unclosed quote.".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => word.extend(chars.next()),
                        Some(c) => word.push(c),
                        None => return Err("The line has an unclosed quote.".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// What a machine says about Slurm: its partitions, and where scratch space is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scheduler {
    pub partitions: Vec<Partition>,
    /// `$SCRATCH`, if the cluster sets it.
    pub scratch: Option<String>,
}

/// Where `has` looks for Slurm's commands besides the PATH. The bootstrap script repeats this list
/// (endeavor-mcp's `PICK_LAUNCHER_SH`), and a test there checks it does.
pub const FOLDERS: [&str; 3] = ["/usr/bin", "/usr/local/bin", "/opt/slurm/bin"];

/// Whether `program` is on the PATH or in one of `FOLDERS`.
pub fn has(program: &str) -> bool {
    let on_path = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()));
    on_path || FOLDERS.iter().any(|d| std::path::Path::new(d).join(program).is_file())
}

/// Ask this machine's Slurm about itself (for Test connection).
pub fn probe() -> Result<Scheduler, String> {
    if !has("sinfo") {
        return Err("Slurm wasn't found here (no sinfo command).".into());
    }
    if !has("sbatch") {
        return Err("sinfo is here but sbatch isn't, so Endeavor can't submit jobs.".into());
    }
    let output = Command::new("sinfo")
        .args(["-h", "-o", SINFO_FORMAT])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run sinfo: {e}"))?;
    if !output.status.success() {
        return Err(format!("sinfo failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let partitions = parse_sinfo(&String::from_utf8_lossy(&output.stdout));
    if partitions.is_empty() {
        return Err("sinfo listed no partitions.".into());
    }
    Ok(Scheduler { partitions, scratch: scratch() })
}

/// `$SCRATCH`, from the environment or a login shell's.
pub fn scratch() -> Option<String> {
    if let Some(s) = std::env::var("SCRATCH").ok().filter(|s| s.starts_with('/')) {
        return Some(s);
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let output = Command::new(shell).args(["-lc", "echo \"$SCRATCH\""]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().rev().map(str::trim).find(|l| l.starts_with('/')).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_in_every_slurm_form() {
        assert_eq!(parse_duration("30"), Some(1800));
        assert_eq!(parse_duration("30:15"), Some(1815));
        assert_eq!(parse_duration("8:00:00"), Some(8 * 3600));
        assert_eq!(parse_duration("1-00:00:00"), Some(86400));
        assert_eq!(parse_duration("2-12"), Some(2 * 86400 + 12 * 3600));
        assert_eq!(parse_duration("1-02:30"), Some(86400 + 2 * 3600 + 1800));
        assert_eq!(parse_duration("UNLIMITED"), None);
        assert_eq!(parse_duration("INVALID"), None);
        assert_eq!(duration_text(480), "8 h");
        assert_eq!(duration_text(45), "45 min");
        assert_eq!(duration_text(90), "1 h 30 min");
    }

    #[test]
    fn sinfo_partitions_merge_node_kinds() {
        let text = "shared*|8:00:00|4|7900\nshort|1:00:00|4|7900\ngpu|2-00:00:00|32|128000\ngpu|2-00:00:00|64|256000\nlong|infinite|4|7900\n";
        let parts = parse_sinfo(text);
        assert_eq!(parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["shared", "short", "gpu", "long"]);
        assert!(parts[0].default && !parts[1].default);
        assert_eq!(parts[0].max_minutes, Some(480));
        assert_eq!((parts[2].cpus, parts[2].mem_gb(), parts[2].max_minutes), (64, 250, Some(2880)));
        assert_eq!(parts[3].max_minutes, None);
    }

    #[test]
    fn resources_clip_to_the_partition_and_become_sbatch_flags() {
        let short = Partition { name: "short".into(), default: false, max_minutes: Some(60), cpus: 4, mem_mb: 7900 };
        let mut r = Resources { partition: Some("short".into()), ..Resources::preset(2) };
        r.clip(Some(&short));
        assert_eq!((r.cpus, r.mem_gb, r.minutes), (4, 7, 60));
        assert_eq!(r.summary(), "4 CPUs · 7 GB · 1 h");
        let request = JobRequest { resources: r, account: Some("lab".into()), depot: None };
        assert_eq!(request.sbatch_args(), ["--account=lab", "--partition=short", "--cpus-per-task=4", "--mem=7G", "--time=60"]);
        assert_eq!(Resources::default().summary(), "8 CPUs · 32 GB · 8 h");
    }

    #[test]
    fn a_pasted_salloc_line_sets_what_it_says_and_keeps_the_rest() {
        let base = Resources::default();
        let (r, account) =
            parse_salloc("salloc -p gpu -c 16 --mem=64G -t 2-00:00:00 -A mylab --gres=gpu:a100:1 --qos normal --exclusive -N1 srun --pty bash", &base).unwrap();
        assert_eq!(r.partition.as_deref(), Some("gpu"));
        assert_eq!((r.cpus, r.mem_gb, r.minutes), (16, 64, 2880));
        assert_eq!(r.gres.as_deref(), Some("gpu:a100:1"));
        assert_eq!(account.as_deref(), Some("mylab"));
        assert_eq!(r.extra, ["--qos=normal", "--exclusive", "-N1"]);
        let (r, _) = parse_salloc("salloc -c2 -C a100 -w node1 --reservation=r1", &base).unwrap();
        assert_eq!(r.extra, ["-Ca100", "-wnode1", "--reservation=r1"]);

        let (r, _) = parse_salloc("--partition=short --cpus-per-task=2 --mem 16000 --time=30", &base).unwrap();
        assert_eq!((r.partition.as_deref(), r.cpus, r.mem_gb, r.minutes), (Some("short"), 2, 16, 30));
        let (r, _) = parse_salloc("salloc -pshort -c4 --mem-per-cpu=4G", &base).unwrap();
        assert_eq!((r.partition.as_deref(), r.cpus, r.mem_gb), (Some("short"), 4, 0));
        assert_eq!(r.extra, ["--mem-per-cpu=4G"]);
        assert_eq!(r.sbatch_args(), ["--partition=short", "--cpus-per-task=4", "--time=480", "--mem-per-cpu=4G"]);
        assert!(parse_salloc("salloc -c lots", &base).is_err());
        assert!(parse_salloc("hello there", &base).is_err());
        assert!(parse_salloc("salloc -p 'unclosed", &base).is_err());
    }

    #[test]
    fn extra_flags_must_be_flags_and_cannot_run_a_command() {
        for flag in ["--constraint=a100", "-N2", "--exclusive", "--", "--w=x", "--wait", "--wckey=k", "--wrapper"] {
            assert!(check_extra_flag(flag).is_ok(), "{flag}");
        }
        for flag in ["normal", "", "--wrap=sleep 1", "--wrap", "--wra=x", "--wr", "--wrap=", "--a\nb", "-x\0", "--qos=a\rb"] {
            assert!(check_extra_flag(flag).is_err(), "{flag:?}");
        }
        assert!(check_extra_flag("normal").unwrap_err().contains("bare word"));
        let mut request = JobRequest::default();
        request.resources.extra = vec!["--qos".into(), "normal".into()];
        assert!(request.checked_sbatch_args().unwrap_err().contains("\"normal\" doesn't start with"));
        request.resources.extra = vec!["--qos=normal".into()];
        assert_eq!(request.checked_sbatch_args().unwrap().last().map(String::as_str), Some("--qos=normal"));
    }

    #[test]
    fn squeue_lines() {
        let q = parse_squeue("PENDING|Priority|(null)|8:00:00\n").unwrap();
        assert!(q.pending() && !q.running());
        assert_eq!(q.left, Some(28800));
        let q = parse_squeue("RUNNING|None|n2cn0216|7:58:10").unwrap();
        assert!(q.running() && q.node == "n2cn0216");
        assert_eq!(parse_squeue(""), None);
        assert_eq!(reason_text("None"), None);
        assert_eq!(reason_text("Priority").unwrap(), "other jobs are ahead in the queue");
        assert_eq!(ended_text("CANCELLED by 1000"), Some("Its Slurm job was cancelled."));
        assert_eq!(ended_text("COMPLETED"), None);
    }
}

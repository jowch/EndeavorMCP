//! Tools that act on the machine the runtime runs on. Only sessions on a server
//! get them (their MCP connection carries `X-Endeavor-Host`): there, Claude's
//! own file and shell tools would see the user's computer instead of the server.
//!
//! Results and errors are what the Julia runtime gave before, to the byte:
//! paths normalized as Julia's `normpath` does, invalid UTF-8 replaced one
//! Julia `Char` at a time, and Julia's own wording for system errors.

use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::mcp::julia_string;

pub const NAMES: [&str; 3] = ["list_folder", "read_file", "run_shell"];

const LIST_FOLDER_MAX_ENTRIES: usize = 1000;
const READ_FILE_MAX_BYTES: usize = 256 * 1024;
const READ_FILE_MAX_LINE: usize = 2000;
#[cfg(any(unix, test))]
const SHELL_KEEP_HALF: usize = 15_000;
#[cfg(unix)]
const SHELL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The tools' schemas, for `tools/list`.
pub fn schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "list_folder",
            "description": "List a folder on the server this session works on. Only for a session on a server; otherwise use your own file tools, which see the user's computer. Returns up to 1000 entries (name, kind, size, modified), folders first; truncated is true when there are more.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder on the server. ~ and relative paths start at the home folder. Default: the home folder." },
                },
                "required": [],
            },
        }),
        json!({
            "name": "read_file",
            "description": "Read a text file on the server this session works on (only for a session on a server). text has numbered lines, like cat -n; the numbers are not part of the file. When truncated is true, read on with offset set to end_line + 1. Refuses binary files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File on the server. ~ and relative paths start at the home folder." },
                    "offset": { "type": "integer", "description": "First line to return, from 1. Default: 1." },
                    "limit": { "type": "integer", "description": "Most lines to return. Default: 2000." },
                },
                "required": ["path"],
            },
        }),
        json!({
            "name": "run_shell",
            "description": "Run a shell command on the server this session works on (only for a session on a server), in the user's login shell, with no input. On a Slurm cluster it runs inside the session's job, on the job's node, so only once the job has started. Otherwise it runs on the machine itself, which other people may share: ask the user before a long or heavy command. Don't submit or cancel Slurm jobs with it unless the user asks. The user may be asked to approve each run, so put related steps in one command and don't use it to wait. Returns exit_code, stdout, stderr (long output keeps its start and end) and timed_out. Don't write notebook files with it: the runtime rewrites them; change notebooks with the notebook tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string", "description": "Folder to run in; ~ and relative paths start at the home folder. Default: the session's folder." },
                    "timeout_seconds": { "type": "integer", "description": "Kill the command, and what it started, after this many seconds. Default: 120; at most 600." },
                },
                "required": ["command"],
            },
        }),
    ]
}

/// Where and how `run_shell` runs commands.
#[cfg_attr(windows, allow(dead_code))]
pub struct Shell<'a> {
    /// The session's working folder: where commands run unless told otherwise.
    pub folder: Option<&'a str>,
    /// Set in, or (None) removed from, the command's environment.
    pub env: &'a [(&'a str, Option<&'a str>)],
}

/// Run a host tool. An error is the text Julia showed for its exception, which
/// the caller turns into the agent's `{error, message}`.
pub fn call(name: &str, args: &Value, shell: &Shell) -> Result<Value, String> {
    match name {
        "list_folder" => list_folder(args),
        "read_file" => read_file(args),
        "run_shell" => run_shell(args, shell),
        _ => Err(argument_error(&format!("unknown_tool::Unknown host tool: '{name}'"))),
    }
}

fn argument_error(message: &str) -> String {
    format!("ArgumentError: {message}")
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name).and_then(Value::as_str).ok_or_else(|| argument_error(&format!("invalid_argument::{name} must be a string")))
}

/// A whole number, as Julia took it: a JSON integer, an integral float, or a boolean.
fn int_arg(args: &Value, name: &str, default: i64) -> Result<i64, String> {
    let whole = || argument_error(&format!("invalid_argument::{name} must be a whole number"));
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(Value::Number(n)) => {
            if let Some(n) = n.as_i64() {
                return Ok(n);
            }
            let x = n.as_f64().unwrap_or(f64::NAN);
            if x.fract() != 0.0 || !x.is_finite() {
                return Err(whole());
            }
            if x < i64::MIN as f64 || x >= i64::MAX as f64 {
                return Err(format!("InexactError: Int64({n})"));
            }
            Ok(x as i64)
        }
        Some(_) => Err(whole()),
    }
}

#[cfg(unix)]
pub fn home() -> String {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return home.to_string_lossy().into_owned();
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; we copy out at once.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return "/".into();
        }
        std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned()
    }
}

#[cfg(windows)]
pub fn home() -> String {
    std::env::home_dir().map(|home| home.to_string_lossy().into_owned()).unwrap_or_default()
}

/// When the file last changed, in Unix seconds as Julia's `mtime` gives them.
#[cfg(unix)]
pub(crate) fn mtime(meta: &Metadata) -> f64 {
    meta.mtime() as f64 + meta.mtime_nsec() as f64 * 1e-9
}

#[cfg(windows)]
pub(crate) fn mtime(meta: &Metadata) -> f64 {
    match meta.modified().map(|t| t.duration_since(std::time::UNIX_EPOCH)) {
        Ok(Ok(since)) => since.as_secs_f64(),
        Ok(Err(before)) => -before.duration().as_secs_f64(),
        Err(_) => 0.0,
    }
}

/// A path the agent gave: `~` is the home folder, a relative path is taken
/// from it, nothing is the home folder itself.
fn host_path(path: Option<&str>) -> Result<String, String> {
    let path = path.unwrap_or_default().trim();
    let home = home();
    if path.is_empty() {
        return Ok(home);
    }
    let path = match path.strip_prefix('~') {
        None => path.to_owned(),
        Some("") => home.clone(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        Some(_) => return Err(argument_error("~user tilde expansion not yet implemented")),
    };
    Ok(normpath(&if path.starts_with('/') { path } else { format!("{home}/{path}") }))
}

/// Julia's `normpath`: `.` and `x/..` gone, repeated slashes collapsed, and a
/// path naming a folder (ending in `/`, `/.` or `/..`) still ends in `/`.
pub fn normpath(path: &str) -> String {
    let dir_path = |p: &str| p.is_empty() || p == "." || p == ".." || p.ends_with('/') || p.ends_with("/.") || p.ends_with("/..");
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    while let Some(j) = (0..parts.len().saturating_sub(1)).find(|&j| parts[j] != ".." && parts[j + 1] == "..") {
        parts.drain(j..j + 2);
    }
    if absolute {
        let ups = parts.iter().take_while(|p| **p == "..").count();
        parts.drain(..ups);
    }
    let mut out = if absolute { format!("/{}", parts.join("/")) } else { parts.join("/") };
    if dir_path(path) && !dir_path(&out) {
        out.push('/');
    }
    out
}

/// Text with each invalid UTF-8 sequence replaced by one U+FFFD, grouping the
/// bytes of a sequence the way Julia splits a `String` into `Char`s.
fn valid_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        let wanted = match lead {
            0x00..=0x7f => 0,
            0x80..=0xbf => 0,
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            _ => 3,
        };
        let mut end = i + 1;
        while end - i <= wanted && end < bytes.len() && bytes[end] & 0xc0 == 0x80 {
            end += 1;
        }
        match std::str::from_utf8(&bytes[i..end]) {
            Ok(text) => out.push_str(text),
            Err(_) => out.push('\u{fffd}'),
        }
        i = end;
    }
    out
}

/// A string as Julia's `repr` shows it, for its error messages.
pub(crate) fn julia_repr(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' | '\\' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Julia's `IOError` for a failed libuv call, e.g.
/// `IOError: readdir("/x"): permission denied (EACCES)`.
fn uv_error(what: &str, error: &io::Error) -> String {
    let (name, message) = match error.raw_os_error() {
        Some(libc::EACCES) => ("EACCES", "permission denied"),
        Some(libc::EPERM) => ("EPERM", "operation not permitted"),
        Some(libc::ENOENT) => ("ENOENT", "no such file or directory"),
        Some(libc::ENOTDIR) => ("ENOTDIR", "not a directory"),
        Some(libc::ELOOP) => ("ELOOP", "too many symbolic links encountered"),
        Some(libc::EMFILE) => ("EMFILE", "too many open files"),
        Some(libc::ENFILE) => ("ENFILE", "file table overflow"),
        Some(libc::ENAMETOOLONG) => ("ENAMETOOLONG", "name too long"),
        Some(libc::EIO) => ("EIO", "i/o error"),
        Some(libc::ENOMEM) => ("ENOMEM", "not enough memory"),
        _ => return format!("IOError: {what}: {error}"),
    };
    format!("IOError: {what}: {message} ({name})")
}

/// Julia's `SystemError`, e.g. `SystemError: opening file "/x": Permission denied`.
fn system_error(what: &str, error: &io::Error) -> String {
    let message = match error.raw_os_error() {
        // SAFETY: strerror returns a valid C string; we copy it out at once.
        Some(errno) => unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)).to_string_lossy().into_owned() },
        None => error.to_string(),
    };
    format!("SystemError: {what}: {message}")
}

fn list_folder(args: &Value) -> Result<Value, String> {
    let path = host_path(args.get("path").filter(|p| !p.is_null()).map(julia_string).as_deref())?;
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(argument_error(&format!("not_a_folder::{path} is a file, not a folder"))),
        Err(_) => return Err(argument_error(&format!("not_found::No folder at {path}"))),
    }
    let mut names: Vec<String> = std::fs::read_dir(&path)
        .map_err(|e| uv_error(&format!("readdir({})", julia_repr(&path)), &e))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut entries: Vec<(bool, String, Value)> = Vec::new();
    for name in names {
        let Ok(meta) = std::fs::symlink_metadata(format!("{path}/{name}")) else { continue };
        let kind = if meta.file_type().is_symlink() {
            "link"
        } else if meta.is_dir() {
            "dir"
        } else {
            "file"
        };
        let modified = mtime(&meta).round_ties_even() as i64;
        let mut entry = json!({ "name": name, "kind": kind, "modified": modified });
        if kind == "file" {
            entry["size"] = meta.len().into();
        }
        entries.push((kind != "dir", name, entry));
    }
    entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = entries.len();
    let listed: Vec<Value> = entries.into_iter().take(LIST_FOLDER_MAX_ENTRIES).map(|(_, _, entry)| entry).collect();
    Ok(json!({ "path": path, "entries": listed, "total": total, "truncated": total > LIST_FOLDER_MAX_ENTRIES }))
}

fn read_file(args: &Value) -> Result<Value, String> {
    let path = host_path(Some(string_arg(args, "path")?))?;
    let offset = int_arg(args, "offset", 1)?;
    let limit = int_arg(args, "limit", 2000)?;
    if offset < 1 {
        return Err(argument_error("invalid_argument::offset is the first line to read, 1 or more"));
    }
    if limit < 1 {
        return Err(argument_error("invalid_argument::limit must be 1 or more"));
    }
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) if meta.is_dir() => return Err(argument_error(&format!("not_a_file::{path} is a folder; use list_folder"))),
        _ => return Err(argument_error(&format!("not_found::No file at {path}"))),
    }
    let open = || File::open(&path).map_err(|e| system_error(&format!("opening file {}", julia_repr(&path)), &e));
    let mut start = Vec::new();
    open()?.take(8192).read_to_end(&mut start).map_err(|e| system_error("read", &e))?;
    if start.contains(&0) {
        return Err(argument_error(&format!("binary_file::{path} is a binary file (it has NUL bytes); read_file only reads text")));
    }

    let mut reader = BufReader::new(open()?);
    let mut text = String::new();
    let (mut total, mut last_line, mut truncated) = (0i64, offset - 1, false);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(|e| system_error("read", &e))? == 0 {
            break;
        }
        total += 1;
        if total < offset || total >= offset.saturating_add(limit) {
            continue;
        }
        if text.len() >= READ_FILE_MAX_BYTES {
            truncated = true;
            continue;
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        let mut shown = valid_text(&line);
        if let Some((cut, _)) = shown.char_indices().nth(READ_FILE_MAX_LINE) {
            shown.truncate(cut);
            shown.push_str(&format!(" [line cut at {READ_FILE_MAX_LINE} characters]"));
            truncated = true;
        }
        text.push_str(&format!("{total:>6}\t{shown}\n"));
        last_line = total;
    }
    truncated |= last_line < total;
    Ok(json!({
        "path": path,
        "text": text,
        "start_line": offset,
        "end_line": last_line,
        "total_lines": total,
        "truncated": truncated,
    }))
}

/// The first and last SHELL_KEEP_HALF bytes of a stream.
#[cfg(any(unix, test))]
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

#[cfg(any(unix, test))]
impl Captured {
    fn add(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        let n = (SHELL_KEEP_HALF - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..n]);
        self.tail.extend_from_slice(&bytes[n..]);
        if self.tail.len() > 4 * SHELL_KEEP_HALF {
            self.tail.drain(..self.tail.len() - SHELL_KEEP_HALF);
        }
    }

    fn text(&self) -> String {
        if self.total <= 2 * SHELL_KEEP_HALF {
            return valid_text(&[&self.head[..], &self.tail[..]].concat());
        }
        let tail = &self.tail[self.tail.len() - SHELL_KEEP_HALF..];
        let omitted = self.total - self.head.len() - tail.len();
        format!("{}\n[… {omitted} bytes left out …]\n{}", valid_text(&self.head), valid_text(tail))
    }
}

/// The user's login shell running `command`, as Julia's runtime ran it.
#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let executable = !shell.is_empty() && std::fs::metadata(&shell).is_ok_and(|m| m.is_file()) && {
        let path = std::ffi::CString::new(shell.as_str()).unwrap_or_default();
        // SAFETY: `path` is a valid C string.
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    };
    if !executable {
        let mut sh = Command::new("/bin/sh");
        sh.args(["-c", command]);
        return sh;
    }
    let mut sh = Command::new(&shell);
    // csh and tcsh refuse -l alongside other flags; they read .cshrc without it.
    if !matches!(shell.rsplit('/').next(), Some("csh" | "tcsh")) {
        sh.arg("-l");
    }
    sh.args(["-c", command]);
    sh
}

/// Only sessions on a server get host tools, and a Windows server is out of scope.
#[cfg(windows)]
fn run_shell(_args: &Value, _shell: &Shell) -> Result<Value, String> {
    Err(argument_error("unsupported::run_shell runs only on Linux and macOS"))
}

#[cfg(unix)]
fn run_shell(args: &Value, shell: &Shell) -> Result<Value, String> {
    let command = string_arg(args, "command")?;
    if command.trim().is_empty() {
        return Err(argument_error("invalid_argument::command is empty"));
    }
    let cwd = match args.get("cwd").filter(|c| !c.is_null()) {
        Some(cwd) => Some(julia_string(cwd)),
        None => shell.folder.map(str::to_owned),
    };
    let cwd = host_path(cwd.as_deref())?;
    if !std::fs::metadata(&cwd).is_ok_and(|m| m.is_dir()) {
        return Err(argument_error(&format!("not_found::No folder at {cwd}")));
    }
    let timeout = Duration::from_secs(int_arg(args, "timeout_seconds", 120)?.clamp(1, 600) as u64);

    let mut command = shell_command(command);
    command.current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (name, value) in shell.env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    // SAFETY: setsid is async-signal-safe. Its own session and process group,
    // so a timeout can kill everything the command started.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| format!("IOError: could not spawn {}: {e}", julia_repr(&cwd)))?;
    let pid = child.id() as i32;
    let mut pipes = [child.stdout.take().map(|p| Box::new(p) as Box<dyn ReadFd>), child.stderr.take().map(|p| Box::new(p) as Box<dyn ReadFd>)];
    let mut captured = [Captured::default(), Captured::default()];
    let deadline = Instant::now() + timeout;
    let (mut status, mut drained_by, mut timed_out) = (None, None, false);
    loop {
        if status.is_none() {
            status = child.try_wait().map_err(|e| format!("IOError: {e}"))?;
            if status.is_some() {
                // A background job the command started can hold the pipes open after it exits.
                drained_by = Some(Instant::now() + SHELL_DRAIN_GRACE);
            } else if !timed_out && Instant::now() >= deadline {
                timed_out = true;
                // SAFETY: plain syscall, to the command's process group.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
        let open = pipes.iter().any(Option::is_some);
        if status.is_some() && (!open || drained_by.is_some_and(|by| Instant::now() >= by)) {
            break;
        }
        read_ready(&mut pipes, &mut captured, Duration::from_millis(50));
    }
    let status = status.expect("the loop ends once the command has exited");
    Ok(json!({
        "exit_code": status.code(),
        "stdout": captured[0].text(),
        "stderr": captured[1].text(),
        "timed_out": timed_out,
        "cwd": cwd,
    }))
}

#[cfg(unix)]
trait ReadFd: Read + AsRawFd {}
#[cfg(unix)]
impl<T: Read + AsRawFd> ReadFd for T {}

/// Wait up to `wait` for output on the open pipes and take what has come;
/// a pipe at its end is dropped.
#[cfg(unix)]
fn read_ready(pipes: &mut [Option<Box<dyn ReadFd>>; 2], captured: &mut [Captured; 2], wait: Duration) {
    let mut fds: Vec<libc::pollfd> = pipes.iter().flatten().map(|p| libc::pollfd { fd: p.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: `fds` holds valid pollfds for pipes we own; with none, poll only waits.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, wait.as_millis() as i32) };
    if ready <= 0 {
        return;
    }
    let mut fds = fds.into_iter();
    for (pipe, captured) in pipes.iter_mut().zip(captured.iter_mut()) {
        let Some(reader) = pipe else { continue };
        if fds.next().is_none_or(|fd| fd.revents == 0) {
            continue;
        }
        let mut buffer = [0u8; 65536];
        match reader.read(&mut buffer) {
            Ok(0) => *pipe = None,
            Ok(n) => captured.add(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => *pipe = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_as_julia_does() {
        for (path, normal) in [
            ("/a/./b/../c", "/a/c"),
            ("/a/b/", "/a/b/"),
            ("//a//b", "/a/b"),
            ("/..", "/"),
            ("/a/b/..", "/a/"),
            ("/a/b/.", "/a/b/"),
            ("/a/..b/", "/a/..b/"),
            ("/a/b/../../..", "/"),
            ("/x/~/y", "/x/~/y"),
        ] {
            assert_eq!(normpath(path), normal, "{path}");
        }
    }

    #[test]
    fn replaces_invalid_utf8_one_julia_char_at_a_time() {
        let bytes = [0x61, 0xe2, 0x82, 0x62, 0x0a, 0xff, 0xc0, 0x80, 0xed, 0xa0, 0x80, 0x7f, 0x01, 0xf0, 0x9f, 0x98, 0x80];
        assert_eq!(valid_text(&bytes), "a\u{fffd}b\n\u{fffd}\u{fffd}\u{fffd}\u{7f}\u{1}😀");
    }

    #[test]
    fn keeps_the_start_and_end_of_long_output() {
        let mut captured = Captured::default();
        for _ in 0..100 {
            captured.add(&[b'y'; 1000]);
        }
        let text = captured.text();
        assert!(text.starts_with(&"y".repeat(SHELL_KEEP_HALF)));
        assert!(text.contains("\n[… 70000 bytes left out …]\n"));
        assert_eq!(text.len(), 2 * SHELL_KEEP_HALF + "\n[… 70000 bytes left out …]\n".len());
    }

    #[test]
    fn shows_strings_and_errors_as_julia_does() {
        assert_eq!(julia_repr("a$b\"c\\d\ne\u{e9}\u{1}"), r#""a\$b\"c\\d\neé\x01""#);
        let denied = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(uv_error("readdir(\"/x\")", &denied), "IOError: readdir(\"/x\"): permission denied (EACCES)");
        assert_eq!(system_error("opening file \"/x\"", &denied), "SystemError: opening file \"/x\": Permission denied");
    }
}

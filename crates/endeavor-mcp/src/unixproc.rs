//! When a process started, on Unix: with its pid, what tells a process from a later one given the same
//! pid (after a reboot, or when pids come round). `runtime.json` and `starting.lock` record it, and a
//! recorded pid counts only while the process now has that start time. `winproc` does this on Windows.

/// What asking for a process's start time came to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Start {
    /// There is no such process.
    Gone,
    /// The platform gives no start time, or it could not be read now (out of descriptors or memory, no
    /// permission): nothing is known about the process.
    Unknown,
    /// In a unit that is the platform's own and means nothing across platforms or reboots. On Linux it
    /// is clock ticks after boot, so `boot_id` goes with it.
    At(u64),
}

impl Start {
    /// The time, when there is one.
    pub fn at(self) -> Option<u64> {
        match self {
            Start::At(time) => Some(time),
            Start::Gone | Start::Unknown => None,
        }
    }
}

/// When the process `pid` started.
#[cfg(target_os = "linux")]
pub fn start_time(pid: i32) -> Start {
    if pid <= 0 {
        return Start::Gone;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat(&stat).map_or(Start::Unknown, Start::At),
        // The file goes with the process; ESRCH is what a process that is exiting gives.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => Start::Gone,
        Err(_) => Start::Unknown,
    }
}

/// Field 22 of `/proc/PID/stat`, the start in clock ticks after boot. The name in field 2 is in
/// parentheses and may hold spaces and parentheses, so the fields are counted from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<u64> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    // The first field after the name is field 3.
    after_name.split_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(target_os = "macos")]
pub fn start_time(pid: i32) -> Start {
    if pid <= 0 {
        return Start::Gone;
    }
    // SAFETY: `info` is plain data that the call fills, and the size given is its own.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size);
        if got == size {
            Start::At(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
        } else if got <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Start::Gone
        } else {
            Start::Unknown
        }
    }
}

/// No start time is recorded on other Unix systems, and a record without one is trusted as before.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn start_time(_pid: i32) -> Start {
    Start::Unknown
}

/// Which boot of this computer this is, where start times count from boot (Linux). macOS gives an absolute
/// time, so none.
#[cfg(target_os = "linux")]
pub fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(id.trim().to_owned()).filter(|id| !id.is_empty())
}

#[cfg(not(target_os = "linux"))]
pub fn boot_id() -> Option<String> {
    None
}

/// Whether a start time recorded on boot `recorded` is comparable with one read now: when either side
/// has no boot id, as before.
pub fn same_boot(recorded: Option<&str>) -> bool {
    match (recorded, boot_id()) {
        (Some(recorded), Some(now)) => recorded == now,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_is_field_22_counted_after_the_last_parenthesis() {
        let stat = "123 (a) (b) c) S 1 123 123 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10 18446744073709551615";
        assert_eq!(parse_stat(stat), Some(987654));
        assert_eq!(parse_stat("1 (init) S 0 1"), None, "too few fields");
        assert_eq!(parse_stat("no name"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_running_process_has_a_start_time_that_does_not_change_and_a_gone_one_is_gone() {
        let me = std::process::id() as i32;
        let started = start_time(me).at().expect("this process's start time");
        assert_eq!(start_time(me), Start::At(started));
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let theirs = start_time(child.id() as i32).at().expect("the child's start time");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(theirs >= started, "a later process started no earlier");
        assert_eq!(start_time(child.id() as i32), Start::Gone, "reaped");
        assert_eq!(start_time(0), Start::Gone);
        assert_eq!(start_time(-1), Start::Gone);
        // A pid that is in no process: the largest a pid can be is far below this.
        assert_eq!(start_time(i32::MAX), Start::Gone);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_boot_is_told_by_its_id_when_both_sides_have_one() {
        let now = boot_id().expect("a boot id on Linux");
        assert!(same_boot(Some(&now)) && same_boot(None));
        assert!(!same_boot(Some("not-this-boot")));
    }
}

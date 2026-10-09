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
        // A zombie has ended and only waits to be reaped, which a container whose PID 1 doesn't reap
        // never does, so it is gone even though `kill(pid, 0)` still finds it.
        Ok(stat) if zombie(&stat) => Start::Gone,
        Ok(stat) => parse_stat(&stat).map_or(Start::Unknown, Start::At),
        // The file goes with the process; ESRCH is what a process that is exiting gives.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => Start::Gone,
        Err(_) => Start::Unknown,
    }
}

/// Whether field 3 of `/proc/PID/stat`, the state, is Z (zombie) or X (dead).
#[cfg(any(target_os = "linux", test))]
fn zombie(stat: &str) -> bool {
    stat.rfind(')').and_then(|end| stat[end + 1..].split_whitespace().next()).is_some_and(|state| state == "Z" || state == "X")
}

/// Whether a process of the group `pgid` is still running. Zombies don't count, as in `start_time`:
/// a container whose PID 1 reaps slowly or not at all may keep the ended processes of a group for a
/// while, or for good.
#[cfg(target_os = "linux")]
pub fn group_running(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks; a group of another user's processes (EPERM) is not counted.
    if pgid <= 0 || unsafe { libc::kill(-pgid, 0) } != 0 {
        return false;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else { return true };
    procs.flatten().filter(|entry| entry.file_name().to_str().is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))).any(|entry| {
        std::fs::read_to_string(entry.path().join("stat")).is_ok_and(|stat| group_of(&stat) == Some(pgid) && !zombie(&stat))
    })
}

/// Whether a process of the group `pgid` is still running. A zombie here doesn't last: the core's parent
/// waits on it, and launchd reaps orphans.
#[cfg(not(target_os = "linux"))]
pub fn group_running(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks; a group of another user's processes (EPERM) is not counted.
    pgid > 0 && unsafe { libc::kill(-pgid, 0) } == 0
}

/// Field 5 of `/proc/PID/stat`, the process group.
#[cfg(any(target_os = "linux", test))]
fn group_of(stat: &str) -> Option<i32> {
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(2)?.parse().ok()
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

/// Whether a process recorded as started at `started` (`start_time`) on boot `boot` (`boot_id`) is from
/// this boot of the computer, so that a process group named after it may still be its. Linux tells by the
/// boot id, macOS by the time it booted; where neither is known, it is taken to be.
pub fn this_boot(started: Option<u64>, boot: Option<&str>) -> bool {
    same_boot(boot) && started.zip(boot_time()).is_none_or(|(started, booted)| started >= booted)
}

/// When this computer booted, in `start_time`'s unit, where that unit is an absolute time (macOS).
#[cfg(target_os = "macos")]
fn boot_time() -> Option<u64> {
    let mut booted: libc::timeval = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut size = std::mem::size_of::<libc::timeval>();
    let mut name = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    // SAFETY: the name has the length given, and `booted` is plain data of the size given.
    let got = unsafe { libc::sysctl(name.as_mut_ptr(), 2, (&raw mut booted).cast(), &mut size, std::ptr::null_mut(), 0) };
    (got == 0 && booted.tv_sec > 0).then(|| booted.tv_sec as u64 * 1_000_000 + booted.tv_usec as u64)
}

#[cfg(not(target_os = "macos"))]
fn boot_time() -> Option<u64> {
    None
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
        assert!(zombie("123 (a) (b) c) Z 1 123") && zombie("9 (x) X 1"));
        assert!(!zombie(stat) && !zombie("no name"));
        assert_eq!(group_of("123 (a) (b) c) S 1 456 456"), Some(456));
        assert_eq!(group_of("no name"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_ended_child_not_yet_reaped_is_gone() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| zombie(&stat)) {
            assert!(std::time::Instant::now() < deadline, "the child never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(start_time(pid), Start::Gone, "a zombie");
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_group_left_with_only_a_zombie_has_nothing_running() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let pid = child.id() as i32;
        assert!(group_running(pid), "it runs");
        // SAFETY: plain syscall, on the child this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| zombie(&stat)) {
            assert!(std::time::Instant::now() < deadline, "the child never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // SAFETY: signal 0 only checks.
        assert_eq!(unsafe { libc::kill(-pid, 0) }, 0, "the zombie still answers a signal");
        assert!(!group_running(pid), "but nothing in the group runs");
        child.wait().unwrap();
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_process_of_this_boot_is_of_this_boot_and_one_recorded_before_it_is_not() {
        let me = start_time(std::process::id() as i32).at();
        assert!(this_boot(me, boot_id().as_deref()));
        assert!(this_boot(None, None), "nothing recorded: as before");
        if cfg!(target_os = "macos") {
            assert!(!this_boot(Some(1), None), "started a microsecond after 1970");
        } else {
            assert!(!this_boot(me, Some("not-this-boot")));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_boot_is_told_by_its_id_when_both_sides_have_one() {
        let now = boot_id().expect("a boot id on Linux");
        assert!(same_boot(Some(&now)) && same_boot(None));
        assert!(!same_boot(Some("not-this-boot")));
    }
}

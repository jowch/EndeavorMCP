//! When a process started, on Unix: with its pid, what tells a process from a later one given the same
//! pid (after a reboot, or when pids come round). `runtime.json` and `starting.lock` record it, and a
//! recorded pid counts only while the process now has that start time. `winproc` does this on Windows.

/// When the process `pid` started, in a unit that is the platform's own and means nothing across
/// platforms or reboots. None when the process is gone, the platform gives no such time, or it can't be
/// read.
#[cfg(target_os = "linux")]
pub fn start_time(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
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
pub fn start_time(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: `info` is plain data that the call fills, and the size given is its own.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size);
        (got == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }
}

/// No start time is recorded on other Unix systems, and a record without one is trusted as before.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn start_time(_pid: i32) -> Option<u64> {
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
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_running_process_has_a_start_time_that_does_not_change_and_a_gone_one_has_none() {
        let me = std::process::id() as i32;
        let started = start_time(me).expect("this process's start time");
        assert_eq!(start_time(me), Some(started));
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let theirs = start_time(child.id() as i32).expect("the child's start time");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(theirs >= started, "a later process started no earlier");
        assert_eq!(start_time(child.id() as i32), None, "reaped");
        assert_eq!(start_time(0), None);
        assert_eq!(start_time(-1), None);
    }
}

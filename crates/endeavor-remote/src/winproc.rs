//! Windows process control for the runtime (docs/windows.md, process control).
//! Unix keeps the runtime together as a process group; here the core puts
//! itself in a Job Object that ends every process in it when the core ends,
//! and a recorded pid is trusted only while the process's start time matches,
//! since Windows hands pids out again quickly.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

/// A process this one opened, known to be the one recorded.
pub struct Process(OwnedHandle);

impl Process {
    /// The process `pid`, if it is still the one that started at `started`
    /// (`start_time`). None when it's gone, its pid now names another
    /// process, or nothing says when it started.
    pub fn open(pid: i32, started: Option<u64>) -> Option<Process> {
        let started = started?;
        let process = Process::open_pid(u32::try_from(pid).ok().filter(|&pid| pid != 0)?)?;
        (start_time(process.0.as_raw_handle()) == Some(started)).then_some(process)
    }

    fn open_pid(pid: u32) -> Option<Process> {
        // SAFETY: plain call; a null handle means it failed.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        // SAFETY: a handle OpenProcess just gave us, owned from here on.
        (!raw.is_null()).then(|| Process(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) }))
    }

    /// An open handle keeps an exited process's record, so ask whether it
    /// has exited rather than whether it can be opened.
    pub fn alive(&self) -> bool {
        // SAFETY: a process handle we own; a zero timeout only polls.
        unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, 0) == WAIT_TIMEOUT }
    }

    /// End it at once, as SIGKILL would.
    pub fn terminate(&self) {
        // SAFETY: a process handle we own, opened with PROCESS_TERMINATE.
        unsafe { TerminateProcess(self.0.as_raw_handle() as HANDLE, 1) };
    }

    /// Wait up to `millis` for it to exit; whether it did.
    pub fn wait(&self, millis: u32) -> bool {
        // SAFETY: a process handle we own, opened with PROCESS_SYNCHRONIZE.
        unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, millis) != WAIT_TIMEOUT }
    }
}

/// When the process behind `handle` started, in FILETIME units: with its pid,
/// what identifies it.
pub fn start_time(handle: RawHandle) -> Option<u64> {
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: four FILETIMEs to write into; a handle with at least limited query access.
    let ok = unsafe { GetProcessTimes(handle as HANDLE, created, exited, kernel, user) } != 0;
    ok.then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
}

/// This process's `start_time`.
pub fn own_start_time() -> Option<u64> {
    // SAFETY: a pseudo-handle that needs no closing.
    start_time(unsafe { GetCurrentProcess() } as RawHandle)
}

/// Put this process in a new Job Object that ends every process in it once
/// the last handle to it closes: the one returned, which nothing inherits, so
/// that happens when this process ends, however it ends. Processes started
/// from here on join the job unless they're allowed to break away, which it
/// doesn't allow. Keep the handle for the life of the process.
pub fn job_ending_with_this_process() -> io::Result<OwnedHandle> {
    // SAFETY: no security attributes and no name: a private job.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle CreateJobObjectW just gave us, owned from here on.
    let job = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size = std::mem::size_of_val(&limits) as u32;
    // SAFETY: the job we own, and a limit structure of the size given.
    if unsafe { SetInformationJobObject(job.as_raw_handle() as HANDLE, JobObjectExtendedLimitInformation, (&raw const limits).cast(), size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Windows 8 and later nest jobs, so this works when this process is in a
    // job already (one the app runs in, or a terminal's).
    // SAFETY: the job we own, and the pseudo-handle for this process.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, GetCurrentProcess()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    const ROLE: &str = "ENDEAVOR_JOB_TEST_ROLE";
    const PID_LINE: &str = "endeavor-test-pid ";

    /// This test binary again, running only `test` as `role`.
    fn rerun(test: &str, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--nocapture", "--test-threads=1"]).env(ROLE, role).stdin(Stdio::null());
        command
    }

    #[test]
    fn ending_the_jobs_first_process_ends_everything_it_started() {
        const TEST: &str = "winproc::tests::ending_the_jobs_first_process_ends_everything_it_started";
        match std::env::var(ROLE).as_deref() {
            // Stands for the core: in the job, with a child that has a child of its own.
            Ok("core") => {
                let _job = job_ending_with_this_process().unwrap();
                let _ = rerun(TEST, "worker").spawn().unwrap().wait();
                return;
            }
            // About a minute, unless the job ends it.
            Ok("worker") => {
                let mut ping = Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
                println!("{PID_LINE}{}", std::process::id());
                println!("{PID_LINE}{}", ping.id());
                let _ = ping.wait();
                return;
            }
            _ => {}
        }
        let mut core = rerun(TEST, "core").stdout(Stdio::piped()).spawn().unwrap();
        let mut pids = Vec::new();
        for line in std::io::BufReader::new(core.stdout.take().unwrap()).lines() {
            if let Some(pid) = line.unwrap().strip_prefix(PID_LINE) {
                pids.push(pid.trim().parse::<u32>().unwrap());
                if pids.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(pids.len(), 2, "the worker said its pid and its child's");
        let started: Vec<Process> = pids.iter().map(|&pid| Process::open_pid(pid).expect("the worker and its child run")).collect();
        assert!(started.iter().all(Process::alive));
        core.kill().unwrap();
        core.wait().unwrap();
        for (process, pid) in started.iter().zip(&pids) {
            assert!(process.wait(10_000), "process {pid} outlived the job's first process");
        }
    }

    #[test]
    fn a_recorded_pid_counts_only_while_its_start_time_matches() {
        let started = own_start_time().expect("this process's start time");
        let me = std::process::id() as i32;
        let this = Process::open(me, Some(started)).expect("this process, as recorded");
        assert!(this.alive());
        assert!(Process::open(me, Some(started + 1)).is_none(), "a pid reused by a later process");
        assert!(Process::open(me, None).is_none(), "no start time recorded");
        assert!(Process::open(0, Some(started)).is_none());

        let mut child = Command::new("cmd").args(["/c", "exit 0"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        let (pid, child_started) = (child.id() as i32, start_time(child.as_raw_handle()).unwrap());
        let open = Process::open(pid, Some(child_started)).expect("the child, while its handle is open");
        child.wait().unwrap();
        assert!(!open.alive(), "an exited process isn't alive, though a handle keeps its record");
        drop((child, open));
        assert!(Process::open(pid, Some(child_started)).is_none_or(|p| !p.alive()), "gone, or its pid is another process's");
    }
}

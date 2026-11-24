use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

pub struct Process(OwnedHandle);

impl Process {
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

    pub fn alive(&self) -> bool {
        // SAFETY: a process handle we own; a zero timeout only polls.
        unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, 0) == WAIT_TIMEOUT }
    }

    pub fn terminate(&self) {
        // SAFETY: a process handle we own, opened with PROCESS_TERMINATE.
        unsafe { TerminateProcess(self.0.as_raw_handle() as HANDLE, 1) };
    }

    pub fn wait(&self, millis: u32) -> bool {
        // SAFETY: a process handle we own, opened with PROCESS_SYNCHRONIZE.
        unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, millis) != WAIT_TIMEOUT }
    }
}

pub fn start_time(handle: RawHandle) -> Option<u64> {
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: four FILETIMEs to write into; a handle with at least limited query access.
    let ok = unsafe { GetProcessTimes(handle as HANDLE, created, exited, kernel, user) } != 0;
    ok.then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
}

pub fn own_start_time() -> Option<u64> {
    // SAFETY: a pseudo-handle that needs no closing.
    start_time(unsafe { GetCurrentProcess() } as RawHandle)
}

fn job_ending_with_its_last_handle() -> io::Result<OwnedHandle> {
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
    Ok(job)
}

fn assign(job: &OwnedHandle, process: HANDLE) -> io::Result<()> {
    // SAFETY: the job we own, and a process handle with PROCESS_SET_QUOTA and PROCESS_TERMINATE access.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn job_ending_with_this_process() -> io::Result<OwnedHandle> {
    let job = job_ending_with_its_last_handle()?;
    // SAFETY: the pseudo-handle for this process.
    assign(&job, unsafe { GetCurrentProcess() })?;
    Ok(job)
}

pub struct Tree(OwnedHandle);

impl Tree {
    pub fn of(child: &std::process::Child) -> io::Result<Tree> {
        let job = job_ending_with_its_last_handle()?;
        assign(&job, child.as_raw_handle() as HANDLE)?;
        Ok(Tree(job))
    }

    pub fn end(&self) {
        // SAFETY: the job we own.
        unsafe { TerminateJobObject(self.0.as_raw_handle() as HANDLE, 1) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const ROLE: &str = "ENDEAVOR_JOB_TEST_ROLE";
    const PIDS: &str = "ENDEAVOR_JOB_TEST_PIDS";

    fn rerun(test: &str, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test, "--nocapture", "--test-threads=1"]).env(ROLE, role).stdin(Stdio::null()).stdout(Stdio::null());
        command
    }

    #[test]
    fn ending_the_jobs_first_process_ends_everything_it_started() {
        const TEST: &str = "winproc::tests::ending_the_jobs_first_process_ends_everything_it_started";
        match std::env::var(ROLE).as_deref() {
            Ok("core") => {
                let _job = job_ending_with_this_process().unwrap();
                let _ = rerun(TEST, "worker").spawn().unwrap().wait();
                return;
            }
            Ok("worker") => {
                let mut ping = Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
                let path = std::env::var(PIDS).unwrap();
                std::fs::write(format!("{path}.part"), format!("{} {}", std::process::id(), ping.id())).unwrap();
                std::fs::rename(format!("{path}.part"), &path).unwrap();
                let _ = ping.wait();
                return;
            }
            _ => {}
        }
        let file = std::env::temp_dir().join(format!("endeavor-job-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let mut core = rerun(TEST, "core").env(PIDS, &file).spawn().unwrap();
        let mut pids = Vec::new();
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(&file) {
                pids = text.split_whitespace().map(|p| p.parse::<u32>().unwrap()).collect();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&file);
        assert_eq!(pids.len(), 2, "the worker wrote its pid and its child's");
        let started: Vec<Process> = pids.iter().map(|&pid| Process::open_pid(pid).expect("the worker and its child run")).collect();
        assert!(started.iter().all(Process::alive));
        core.kill().unwrap();
        core.wait().unwrap();
        for (process, pid) in started.iter().zip(&pids) {
            assert!(process.wait(10_000), "process {pid} outlived the job's first process");
        }
    }

    #[test]
    fn ending_a_tree_ends_the_child_and_what_it_started() {
        const TEST: &str = "winproc::tests::ending_a_tree_ends_the_child_and_what_it_started";
        if std::env::var(ROLE).as_deref() == Ok("worker") {
            let mut ping = Command::new("ping").args(["-n", "60", "127.0.0.1"]).stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
            let path = std::env::var(PIDS).unwrap();
            std::fs::write(format!("{path}.part"), ping.id().to_string()).unwrap();
            std::fs::rename(format!("{path}.part"), &path).unwrap();
            let _ = ping.wait();
            return;
        }
        let file = std::env::temp_dir().join(format!("endeavor-tree-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let mut worker = rerun(TEST, "worker").env(PIDS, &file).spawn().unwrap();
        let tree = Tree::of(&worker).unwrap();
        let mut grandchild = None;
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(&file) {
                grandchild = text.trim().parse::<u32>().ok();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&file);
        let grandchild = Process::open_pid(grandchild.expect("the worker wrote its child's pid")).expect("the worker's child runs");
        tree.end();
        worker.wait().unwrap();
        assert!(grandchild.wait(10_000), "the worker's child outlived the tree");
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

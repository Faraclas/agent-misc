//! Owns a stdio MCP server's whole process tree.
//!
//! Launchers such as `npx`, `uvx`, `pnpm dlx` or `docker run` start the real server as a child
//! of their own, so killing only the process utcp started would leave the server running.
//!
//! On Unix the server gets its own process group. Stopping it sends SIGTERM to the group, so
//! servers can clean up and `docker run` / `podman run` forward the stop to their container
//! (a container is not in our group, so this forwarding is the only way to reach it). Whatever
//! is still running after [`STOP_GRACE`] gets SIGKILL. This runs on a background thread; call
//! [`wait_for_stopping_servers`] before the process exits so the SIGKILL is not skipped.
//!
//! On Windows the server is put in a Job Object that kills every process in it when the job
//! handle closes, which also happens if utcp itself dies.

use std::time::Duration;
use tokio::process::{Child, Command};

/// How long a server gets to exit after SIGTERM before what is left of its group is killed.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) const STOP_GRACE: Duration = Duration::from_secs(2);

/// Set up `cmd` so the tree it starts can be stopped as one unit.
pub(super) fn prepare(cmd: &mut Command) {
    cmd.kill_on_drop(true);
    crate::transports::hide_console_window(cmd);
    #[cfg(unix)]
    cmd.process_group(0);
}

/// A started server. Dropping it stops the server and everything it started.
pub(super) struct ServerProcess {
    // Windows only keeps it for `kill_on_drop`.
    #[cfg_attr(windows, allow(dead_code))]
    child: Option<Child>,
    #[cfg(windows)]
    _job: Option<windows_job::Job>,
}

impl ServerProcess {
    pub(super) fn new(child: Child) -> Self {
        #[cfg(windows)]
        let _job = windows_job::Job::kill_on_close_for(&child);
        Self {
            child: Some(child),
            #[cfg(windows)]
            _job,
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(child) = self.child.take() {
            unix_stop::stop(child);
        }
        // Windows: dropping `_job` closes it and the system kills the tree; `kill_on_drop` on
        // the child covers the case where the job could not be created.
    }
}

/// Wait, at most `max`, until every server whose stop has begun is fully stopped.
pub fn wait_for_stopping_servers(max: Duration) {
    #[cfg(unix)]
    unix_stop::wait_all(max);
    #[cfg(not(unix))]
    let _ = max;
}

#[cfg(unix)]
mod unix_stop {
    use super::STOP_GRACE;
    use std::sync::{Condvar, Mutex};
    use std::time::{Duration, Instant};
    use tokio::process::Child;

    static STOPPING: Mutex<usize> = Mutex::new(0);
    static ALL_STOPPED: Condvar = Condvar::new();

    pub(super) fn stop(mut child: Child) {
        // `None` once the child has been reaped, which only happens below.
        let Some(pid) = child.id() else { return };
        let group = -(pid as libc::pid_t);
        // SAFETY: plain syscall. The leader is not reaped yet, so its pid (the group id, from
        // `process_group(0)`) is still reserved and the group cannot belong to anyone else.
        unsafe { libc::kill(group, libc::SIGTERM) };

        *STOPPING.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        let spawned = std::thread::Builder::new()
            .name("utcp-mcp-stop".into())
            .spawn(move || {
                finish(&mut child, group);
                drop(child);
                stopped();
            });
        if spawned.is_err() {
            // SAFETY: as above; no time to wait, so stop the group now.
            unsafe { libc::kill(group, libc::SIGKILL) };
            stopped();
        }
    }

    /// Wait for the group to exit on its own, kill what is left, and reap the leader.
    fn finish(child: &mut Child, group: libc::pid_t) {
        let deadline = Instant::now() + STOP_GRACE;
        let mut reaped = false;
        while Instant::now() < deadline {
            if !reaped {
                reaped = !matches!(child.try_wait(), Ok(None));
            }
            // Once the leader is reaped, the group id stays reserved while any member is
            // alive, so signal 0 (a pure existence check) tells whether the group is gone.
            // SAFETY: plain syscall.
            if reaped && unsafe { libc::kill(group, 0) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: the group still had members a moment ago, so its id is still theirs.
        unsafe { libc::kill(group, libc::SIGKILL) };
        for _ in 0..100 {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stopped() {
        let mut count = STOPPING.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        if *count == 0 {
            ALL_STOPPED.notify_all();
        }
    }

    pub(super) fn wait_all(max: Duration) {
        let count = STOPPING.lock().unwrap_or_else(|e| e.into_inner());
        let _ = ALL_STOPPED.wait_timeout_while(count, max, |n| *n > 0);
    }
}

#[cfg(windows)]
mod windows_job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub(super) struct Job(HANDLE);

    // SAFETY: a job handle refers to a kernel object and may be used and closed from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A job that kills `child` and all its descendants when the handle is closed. `None`
        /// when the job cannot be set up (the caller then falls back to killing the child only).
        pub(super) fn kill_on_close_for(child: &tokio::process::Child) -> Option<Self> {
            let process = child.raw_handle()? as HANDLE;
            // SAFETY: documented Win32 calls on handles we own; `info` is a plain C struct.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let job = Job(handle);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION
                        as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(job.0, process) == 0 {
                    return None;
                }
                Some(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

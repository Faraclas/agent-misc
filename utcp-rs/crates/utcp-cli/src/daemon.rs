use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
use std::time::{Duration, Instant};
use utcp_core::daemon_log;
use utcp_core::ipc::{
    query_daemon_status, query_daemon_status_checked, run_daemon_foreground, try_send_ipc_sync,
    DaemonStatusInfo, IpcCommand, IpcPayload, IpcRequest, IpcResponse,
};
use utcp_core::paths::{UtcpPaths, UtcpSettings};

/// Returns true on Linux when running against the default user session (no test/custom path overrides)
/// AND `utcp-daemon.service` is registered in `systemd --user`, so `utcp daemon start|stop|restart`
/// and `utcp ui` delegate cleanly to `systemctl --user` instead of spawning an unmanaged process.
fn should_delegate_to_systemd_user(
    explicit_config: Option<&Path>,
    idle_timeout_mins: Option<u64>,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        if explicit_config.is_some()
            || idle_timeout_mins.is_some()
            || std::env::var_os("UTCP_SOCKET_PATH").is_some()
            || std::env::var_os("UTCP_CONFIG_DIR").is_some()
            || std::env::var_os("UTCP_CONFIG").is_some()
        {
            return false;
        }
        Command::new("systemctl")
            .args(["--user", "cat", "utcp-daemon.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (explicit_config, idle_timeout_mins);
        false
    }
}

/// The detached daemon just started, watched so `daemon start` stops waiting as soon as it exits.
#[cfg(windows)]
struct SpawnedDaemon(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl SpawnedDaemon {
    fn has_exited(&mut self) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: the handle is the live process handle from CreateProcessW, owned by `self`.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

#[cfg(windows)]
impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        // SAFETY: closes the handle `self` owns, once; the daemon keeps running.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(not(windows))]
struct SpawnedDaemon(std::process::Child);

#[cfg(not(windows))]
impl SpawnedDaemon {
    fn has_exited(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(Some(_)))
    }
}

/// Starts the detached Windows daemon with `CreateProcessW` and `bInheritHandles = FALSE`.
///
/// `std::process::Command` always lets the child inherit every inheritable handle of this process, not
/// only the std handles: a pipe that a caller's shell passed down to `utcp daemon start` would then stay
/// open in the long-lived daemon, and that caller (an SSH session, an agent's shell tool capturing
/// output) would wait for end-of-file until the daemon exits. The daemon needs no inherited handle: its
/// std handles are null (writes are discarded) and it opens its own pipes for MCP servers.
///
/// It also leaves the caller's job object (`CREATE_BREAKAWAY_FROM_JOB`): Windows OpenSSH and other
/// hosts kill their job, and every process still in it, when the session ends. A job that forbids
/// breakaway refuses with `ERROR_ACCESS_DENIED`; the daemon then starts inside it.
#[cfg(windows)]
fn spawn_detached_daemon(
    exe: &Path,
    args: &[std::ffi::OsString],
) -> std::io::Result<SpawnedDaemon> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
        DETACHED_PROCESS, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    let wide = |s: &std::ffi::OsStr| s.encode_wide().collect::<Vec<u16>>();
    let mut application: Vec<u16> = wide(exe.as_os_str());
    application.push(0);
    let mut command_line = Vec::new();
    push_windows_arg(&mut command_line, &application[..application.len() - 1]);
    for arg in args {
        command_line.push(u16::from(b' '));
        push_windows_arg(&mut command_line, &wide(arg));
    }
    command_line.push(0);

    let base_flags = CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_UNICODE_ENVIRONMENT;
    for flags in [base_flags | CREATE_BREAKAWAY_FROM_JOB, base_flags] {
        // SAFETY: all pointers are valid for the call; CreateProcessW may modify `command_line`, which
        // we own mutably. Null std handles with STARTF_USESTDHANDLES give the child no std handles.
        unsafe {
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.dwFlags = STARTF_USESTDHANDLES;
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let mut line = command_line.clone();
            let ok = CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut info,
            );
            if ok != 0 {
                CloseHandle(info.hThread);
                return Ok(SpawnedDaemon(info.hProcess));
            }
        }
        let err = std::io::Error::last_os_error();
        if flags & CREATE_BREAKAWAY_FROM_JOB == 0
            || err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32)
        {
            return Err(err);
        }
    }
    unreachable!("the second attempt always returns")
}

/// Appends one argument to a Windows command line, quoted the way `CommandLineToArgvW` and the C
/// runtime split it back (backslashes are literal unless they precede a quote).
#[cfg(any(windows, test))]
fn push_windows_arg(line: &mut Vec<u16>, arg: &[u16]) {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;
    let needs_quotes = arg.is_empty()
        || arg
            .iter()
            .any(|&c| matches!(c, 0x20 | 0x09 | 0x0a | 0x0b | QUOTE));
    if !needs_quotes {
        line.extend_from_slice(arg);
        return;
    }
    line.push(QUOTE);
    let mut backslashes = 0usize;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
            continue;
        }
        let escaped = if c == QUOTE {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        line.extend(std::iter::repeat_n(BACKSLASH, escaped));
        line.push(c);
        backslashes = 0;
    }
    line.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    line.push(QUOTE);
}

/// Start `utcp daemon` (via `systemctl --user start utcp-daemon.service` when installed, or as a
/// detached background process) and wait for socket readiness: 3 seconds, plus the `[secrets]`
/// command's timeout when one is set; less if the detached
/// daemon exits first. A failure includes what the daemon wrote to `daemon.log` during this attempt,
/// since its stderr is discarded.
pub fn start_daemon_background(
    paths: &UtcpPaths,
    explicit_config: Option<&Path>,
    idle_timeout_mins: Option<u64>,
) -> Result<DaemonStatusInfo> {
    if let Some(info) = query_daemon_status_checked(paths)? {
        return Ok(info);
    }
    let log_offset = daemon_log::current_len(paths);

    // None under systemd: the unit's process is not ours to watch.
    let mut spawned: Option<SpawnedDaemon> = None;
    if should_delegate_to_systemd_user(explicit_config, idle_timeout_mins) {
        let status = Command::new("systemctl")
            .args(["--user", "start", "utcp-daemon.service"])
            .status()
            .context("Failed to execute `systemctl --user start utcp-daemon.service`")?;
        if !status.success() {
            return Err(anyhow!(
                "`systemctl --user start utcp-daemon.service` exited with {}",
                status
            ));
        }
    } else {
        let exe = std::env::current_exe().context("Failed to locate current utcp executable")?;
        let mut args: Vec<std::ffi::OsString> = Vec::new();
        if let Some(cfg) = explicit_config {
            args.push("--config".into());
            args.push(cfg.into());
        }
        args.push("daemon".into());
        args.push("run".into());
        if let Some(mins) = idle_timeout_mins {
            args.push("--idle-timeout".into());
            args.push(mins.to_string().into());
        }

        #[cfg(windows)]
        let child = spawn_detached_daemon(&exe, &args)
            .context("Failed to spawn background utcp daemon process")?;
        #[cfg(not(windows))]
        let child = SpawnedDaemon(
            Command::new(exe)
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to spawn background utcp daemon process")?,
        );
        spawned = Some(child);
    }

    // The daemon runs the secrets command before it listens, so allow for its timeout too.
    let wait = Duration::from_secs(
        3 + UtcpSettings::load(paths)
            .secrets
            .map_or(0, |s| s.timeout_secs.max(1)),
    );
    let deadline = Instant::now() + wait;
    loop {
        // Read before probing: a daemon that exits after losing a start race to another one leaves
        // that one answering, which the probe below still finds.
        let exited = spawned.as_mut().is_some_and(SpawnedDaemon::has_exited);
        if query_daemon_status(paths).is_some() {
            let _ = utcp_core::ipc::sync_caller_env_to_daemon(paths);
            if let Some(updated) = query_daemon_status(paths) {
                return Ok(updated);
            }
        }
        if exited || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let outcome = if spawned.as_mut().is_some_and(SpawnedDaemon::has_exited) {
        "exited before listening".to_string()
    } else {
        format!("did not start listening within {} seconds", wait.as_secs())
    };
    match daemon_log::read_since(paths, log_offset) {
        Some(logged) => Err(anyhow!(
            "utcp daemon {} on {}. It logged ({}):\n{}",
            outcome,
            paths.socket_endpoint,
            daemon_log::daemon_log_file(paths).display(),
            logged
        )),
        None => Err(anyhow!(
            "utcp daemon {} on {}",
            outcome,
            paths.socket_endpoint
        )),
    }
}

/// Gracefully stop a running `utcp daemon` (via `systemctl --user stop` when unit-managed, else over IPC).
pub fn stop_daemon(paths: &UtcpPaths) -> Result<bool> {
    if query_daemon_status_checked(paths)?.is_none() {
        return Ok(false);
    }

    if should_delegate_to_systemd_user(None, None) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "utcp-daemon.service"])
            .status();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if query_daemon_status(paths).is_none() {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    let req = IpcRequest {
        caller: "cli".into(),
        force_log: false,
        command: IpcCommand::Shutdown,
    };
    if let Some(IpcResponse::Ok {
        data: IpcPayload::ShuttingDown,
    }) = try_send_ipc_sync(paths, &req)?
    {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if query_daemon_status(paths).is_none() {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        return Ok(true);
    }
    Ok(false)
}

/// Run the foreground Tokio loop for `utcp daemon run`. Panics and the error it exits with also go to
/// `daemon.log`: stderr is discarded when the daemon runs detached or from the Windows logon task.
pub fn execute_daemon_run(paths: UtcpPaths, idle_timeout_mins: Option<u64>) -> Result<()> {
    daemon_log::install_panic_hook(&paths);
    let log_paths = paths.clone();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to initialize Tokio runtime for utcp daemon")
        .and_then(|rt| rt.block_on(run_daemon_foreground(paths, idle_timeout_mins)));
    if let Err(err) = &result {
        let _ = daemon_log::append(&log_paths, &format!("daemon exited with an error: {err:#}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::push_windows_arg;

    fn quoted(arg: &str) -> String {
        let mut line = Vec::new();
        push_windows_arg(&mut line, &arg.encode_utf16().collect::<Vec<u16>>());
        String::from_utf16(&line).unwrap()
    }

    #[test]
    fn windows_args_are_quoted_the_way_the_c_runtime_splits_them() {
        assert_eq!(quoted("run"), "run");
        assert_eq!(quoted(r"C:\tools\utcp.exe"), r"C:\tools\utcp.exe");
        assert_eq!(
            quoted(r"C:\Users\Jane Doe\utcp.exe"),
            r#""C:\Users\Jane Doe\utcp.exe""#
        );
        assert_eq!(quoted(r"C:\My Dir\"), r#""C:\My Dir\\""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(quoted(""), r#""""#);
    }
}

/// Fast caller detection (< 0.05 ms on Linux; on Windows one process snapshot to find the parent).
/// Checks specific agent markers and ancestor process names BEFORE generic terminal host variables
/// (so running `agy` or `hermes` inside a Zed/VS Code terminal tab still reports `agy` / `hermes`).
pub fn detect_caller() -> String {
    if let Ok(explicit) = std::env::var("UTCP_CALLER") {
        if !explicit.trim().is_empty() {
            return explicit;
        }
    }
    if std::env::var_os("ANTIGRAVITY_CLI").is_some() || std::env::var_os("GEMINI_CLI").is_some() {
        return "agy".to_string();
    }
    if std::env::var_os("HERMES_SESSION_ID").is_some() || std::env::var_os("HERMES_AGENT").is_some()
    {
        return "hermes".to_string();
    }

    // Linux only: walk up to 6 parent PIDs via `/proc/<pid>/stat` (~0.03 ms total) to detect `agy` and `hermes`
    if let Some(agent_ancestor) = detect_agent_in_ancestry() {
        return agent_ancestor;
    }

    if std::env::var_os("ZED_TERM").is_some()
        || std::env::var("TERM_PROGRAM")
            .map(|v| v.eq_ignore_ascii_case("zed"))
            .unwrap_or(false)
    {
        return "zed".to_string();
    }
    if std::env::var("TERM_PROGRAM")
        .map(|v| v.eq_ignore_ascii_case("vscode"))
        .unwrap_or(false)
        || std::env::var_os("VSCODE_PID").is_some()
    {
        return "vscode".to_string();
    }

    detect_parent_process_name().unwrap_or_else(|| "terminal".to_string())
}

#[cfg(target_os = "linux")]
fn read_ppid_and_comm(pid: u32) -> Option<(u32, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let open_paren = stat.find('(')?;
    let close_paren = stat.rfind(')')?;
    let comm = stat[open_paren + 1..close_paren].to_string();
    let fields: Vec<&str> = stat[close_paren + 2..].split_whitespace().collect();
    let ppid: u32 = fields.get(1)?.parse().ok()?;
    Some((ppid, comm))
}

#[cfg(target_os = "linux")]
fn detect_agent_in_ancestry() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..6 {
        let (ppid, _) = read_ppid_and_comm(pid)?;
        if ppid <= 1 {
            break;
        }
        let (_, parent_comm) = read_ppid_and_comm(ppid)?;
        let lower = parent_comm.to_ascii_lowercase();
        if lower == "agy" || lower.contains("antigravity") {
            return Some("agy".to_string());
        }
        if lower.contains("hermes") {
            return Some("hermes".to_string());
        }
        // Also inspect `/proc/<ppid>/cmdline` for node/python-wrapped `agy` or `hermes` CLIs
        if matches!(lower.as_str(), "node" | "python" | "python3") {
            if let Ok(cmdline) = std::fs::read(format!("/proc/{}/cmdline", ppid)) {
                let cmd_str = String::from_utf8_lossy(&cmdline).to_ascii_lowercase();
                if cmd_str.contains("antigravity") || cmd_str.contains("/agy") {
                    return Some("agy".to_string());
                }
                if cmd_str.contains("hermes") {
                    return Some("hermes".to_string());
                }
            }
        }
        pid = ppid;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detect_agent_in_ancestry() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn detect_parent_process_name() -> Option<String> {
    let (ppid, _) = read_ppid_and_comm(std::process::id())?;
    if ppid == 0 {
        return None;
    }
    let (_, comm) = read_ppid_and_comm(ppid)?;
    let trimmed = comm.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(normalize_proc_name(trimmed))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn detect_parent_process_name() -> Option<String> {
    None
}

#[cfg(windows)]
fn detect_parent_process_name() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let current_pid = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ppid = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == current_pid {
                    ppid = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);

        let parent_pid = ppid?;
        let proc_handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid);
        if proc_handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(proc_handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(proc_handle);
        if ok == 0 || len == 0 {
            return None;
        }
        let full_path = String::from_utf16_lossy(&buf[..len as usize]);
        let stem = std::path::Path::new(&full_path)
            .file_stem()
            .and_then(|s| s.to_str())?;
        Some(normalize_proc_name(stem))
    }
}

/// Maps known editor and agent process names to their caller name and keeps any other name as is.
/// Matches whole names (or a known prefix), so `codex` or `opencode` are not reported as `vscode`.
fn normalize_proc_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let is = |names: &[&str]| names.contains(&lower.as_str());
    if is(&["zed", "zeditor", "zed-editor", "zed-preview"]) {
        "zed".to_string()
    } else if is(&["code", "code-insiders", "code-oss", "codium", "vscodium"]) {
        "vscode".to_string()
    } else if lower == "agy" || lower.contains("antigravity") {
        "agy".to_string()
    } else if lower.contains("hermes") {
        "hermes".to_string()
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_proc_name;

    #[test]
    fn normalize_proc_name_matches_whole_names() {
        for (raw, want) in [
            ("Code", "vscode"),
            ("code-insiders", "vscode"),
            ("zeditor", "zed"),
            ("zed", "zed"),
            ("agy", "agy"),
            ("Antigravity", "agy"),
            ("hermes", "hermes"),
            ("codex", "codex"),
            ("opencode", "opencode"),
            ("pwsh", "pwsh"),
        ] {
            assert_eq!(normalize_proc_name(raw), want, "{raw}");
        }
    }
}

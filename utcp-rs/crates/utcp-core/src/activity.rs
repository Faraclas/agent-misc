use crate::paths::UtcpPaths;
use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Default maximum size of `activity.jsonl` before rotating to `activity.jsonl.bak` (1 MiB, ~5,000 entries).
pub const MAX_ACTIVITY_LOG_BYTES: u64 = 1024 * 1024;

/// Longest `detail` text kept per activity entry (characters); longer text is cut with a marker.
pub const MAX_DETAIL_CHARS: usize = 4000;

/// Environment values shorter than this are never redacted (too likely to match ordinary text).
const MIN_REDACT_LEN: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub timestamp: String,
    pub mode: String,
    pub caller: String,
    pub command: String,
    pub target: String,
    pub ok: bool,
    pub status: String,
    pub latency_ms: f64,
    /// Full error text for failed calls (or the failed items of a batch); shown by TUI Tab 5 `Enter`.
    /// Secret-looking environment values are redacted and the text is capped at `MAX_DETAIL_CHARS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn is_secret_env_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
        "_PAT",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// Prepare error text for an activity entry: replace the values of secret-looking environment
/// variables (`*TOKEN*`, `*KEY*`, `*SECRET*`, ...) with `***REDACTED:<NAME>***`, then cap the length.
pub fn activity_detail(message: &str, env: &HashMap<String, String>) -> String {
    let mut secrets: Vec<(&String, &String)> = env
        .iter()
        .filter(|(k, v)| v.len() >= MIN_REDACT_LEN && is_secret_env_key(k))
        .collect();
    // Longest first, so a value that contains another secret is replaced whole.
    secrets.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let mut text = message.to_string();
    for (name, value) in secrets {
        if text.contains(value.as_str()) {
            text = text.replace(value.as_str(), &format!("***REDACTED:{}***", name));
        }
    }
    if text.chars().count() > MAX_DETAIL_CHARS {
        let cut: String = text.chars().take(MAX_DETAIL_CHARS).collect();
        text = format!("{}\n... (cut at {} characters)", cut, MAX_DETAIL_CHARS);
    }
    text
}

/// `(ok, status, detail)` for a batch activity entry: status is `OK` when every item succeeded,
/// otherwise `<ok>/<total> OK`, and `detail` lists each failed item as `<id> <tool>: <error>`.
pub fn batch_activity_summary(
    items: &[crate::catalog::BatchResultItem],
    env: &HashMap<String, String>,
) -> (bool, String, Option<String>) {
    let ok_count = items.iter().filter(|i| i.ok).count();
    if ok_count == items.len() {
        return (true, "OK".to_string(), None);
    }
    let failures: Vec<String> = items
        .iter()
        .filter(|i| !i.ok)
        .map(|i| {
            format!(
                "{} {}: {}",
                i.id,
                i.tool,
                i.error.as_deref().unwrap_or(&i.status)
            )
        })
        .collect();
    (
        false,
        format!("{}/{} OK", ok_count, items.len()),
        Some(activity_detail(&failures.join("\n\n"), env)),
    )
}

/// Fast sentinel check (< 1 microsecond kernel dentry `stat()`):
/// Disk logging is strictly opt-in and OFF by default unless `logging.enabled` exists or `force_log` is true.
#[inline]
pub fn is_logging_enabled(paths: &UtcpPaths) -> bool {
    if std::env::var("UTCP_LOG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    paths.logging_sentinel.exists()
}

/// Explicitly turn persistent disk activity logging on or off.
pub fn set_logging_enabled(paths: &UtcpPaths, enabled: bool) -> Result<()> {
    paths.ensure_dirs()?;
    if enabled {
        fs::write(&paths.logging_sentinel, b"1\n")?;
    } else if paths.logging_sentinel.exists() {
        let _ = fs::remove_file(&paths.logging_sentinel);
    }
    Ok(())
}

/// Clear `activity.jsonl` and `activity.jsonl.bak`.
pub fn clear_activity_logs(paths: &UtcpPaths) -> Result<()> {
    if paths.activity_log_file.exists() {
        fs::remove_file(&paths.activity_log_file)?;
    }
    if paths.activity_log_bak.exists() {
        fs::remove_file(&paths.activity_log_bak)?;
    }
    Ok(())
}

/// Append a single `ActivityEntry` with 1-backup rotation (`activity.jsonl` -> `activity.jsonl.bak`)
/// at `max_bytes` (default 1 MiB). Keeps at most 1 backup file.
pub fn append_activity_entry(paths: &UtcpPaths, entry: &ActivityEntry) -> Result<()> {
    append_activity_entry_with_cap(paths, entry, MAX_ACTIVITY_LOG_BYTES)
}

pub fn append_activity_entry_with_cap(
    paths: &UtcpPaths,
    entry: &ActivityEntry,
    max_bytes: u64,
) -> Result<()> {
    if let Some(parent) = paths.activity_log_file.parent() {
        fs::create_dir_all(parent)?;
    }

    // Serialize line in memory first
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');

    // Acquire short advisory lock on state dir lock file so direct-mode concurrent writers rotate safely
    let lock_path = paths.state_dir.join(".activity.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock_file.lock_exclusive()?;

    let res = (|| -> Result<()> {
        if let Ok(meta) = fs::metadata(&paths.activity_log_file) {
            if meta.len() + (line.len() as u64) > max_bytes && meta.len() > 0 {
                if paths.activity_log_bak.exists() {
                    let _ = fs::remove_file(&paths.activity_log_bak);
                }
                fs::rename(&paths.activity_log_file, &paths.activity_log_bak)?;
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.activity_log_file)?;
        file.write_all(&line)?;
        Ok(())
    })();

    let _ = FileExt::unlock(&lock_file);
    res
}

/// Read the most recent `limit` activity entries across `activity.jsonl.bak` and `activity.jsonl`
/// (returned newest-first for TUI / CLI display).
pub fn read_recent_activity(paths: &UtcpPaths, limit: usize) -> Vec<ActivityEntry> {
    let mut entries = Vec::new();
    read_jsonl_file(&paths.activity_log_bak, &mut entries);
    read_jsonl_file(&paths.activity_log_file, &mut entries);
    entries.reverse();
    if entries.len() > limit {
        entries.truncate(limit);
    }
    entries
}

fn read_jsonl_file(path: &Path, out: &mut Vec<ActivityEntry>) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<ActivityEntry>(trimmed) {
            out.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(tmp: &TempDir) -> UtcpPaths {
        std::env::remove_var("UTCP_LOG");
        let base = tmp.path();
        UtcpPaths {
            config_dir: base.join("config"),
            cache_dir: base.join("cache"),
            state_dir: base.join("state"),
            media_dir: base.join("media"),
            config_file: base.join("config/.utcp_config.json"),
            settings_file: base.join("config/settings.toml"),
            env_file: base.join("config/.env"),
            lock_file: base.join("config/.utcp.lock"),
            manuals_dir: base.join("cache/manuals"),
            search_index_file: base.join("cache/search_index.bin"),
            logging_sentinel: base.join("state/logging.enabled"),
            activity_log_file: base.join("state/activity.jsonl"),
            activity_log_bak: base.join("state/activity.jsonl.bak"),
            socket_endpoint: base.join("cache/utcp.sock").to_string_lossy().into_owned(),
            daemon_pid_file: base.join("state/daemon.pid"),
        }
    }

    #[test]
    fn logging_is_off_by_default_and_rotates_with_single_backup() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        assert!(!is_logging_enabled(&paths));

        set_logging_enabled(&paths, true).unwrap();
        assert!(is_logging_enabled(&paths));

        for i in 0..20 {
            let entry = ActivityEntry {
                timestamp: format!("2026-11-13T00:00:{:02}Z", i),
                mode: "direct".into(),
                caller: "zed".into(),
                command: "call".into(),
                target: format!("demo.tool_{}", i),
                ok: true,
                status: "200 OK".into(),
                latency_ms: 1.2,
                detail: None,
            };
            append_activity_entry_with_cap(&paths, &entry, 600).unwrap();
        }

        assert!(paths.activity_log_file.exists());
        assert!(paths.activity_log_bak.exists());
        let recent = read_recent_activity(&paths, 50);
        assert!(!recent.is_empty());
        assert_eq!(recent[0].target, "demo.tool_19");

        set_logging_enabled(&paths, false).unwrap();
        assert!(!is_logging_enabled(&paths));
    }

    #[test]
    fn activity_detail_redacts_secret_env_values_and_caps_length() {
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "ghp_abcdefgh12345".to_string());
        env.insert("MY_API_KEY".to_string(), "sk-live-999999".to_string());
        env.insert("HOME".to_string(), "/home/someone".to_string());
        env.insert("SHORT_TOKEN".to_string(), "abc".to_string());

        let msg = "401 for Bearer ghp_abcdefgh12345 (key sk-live-999999) in /home/someone, abc";
        let out = activity_detail(msg, &env);
        assert!(!out.contains("ghp_abcdefgh12345"), "{out}");
        assert!(!out.contains("sk-live-999999"), "{out}");
        assert!(out.contains("***REDACTED:GITHUB_TOKEN***"), "{out}");
        assert!(out.contains("***REDACTED:MY_API_KEY***"), "{out}");
        assert!(
            out.contains("/home/someone"),
            "non-secret vars stay readable: {out}"
        );
        assert!(
            out.ends_with(", abc"),
            "short values are not redacted: {out}"
        );

        let long = "x".repeat(MAX_DETAIL_CHARS + 50);
        let cut = activity_detail(&long, &HashMap::new());
        assert!(cut.starts_with(&"x".repeat(MAX_DETAIL_CHARS)));
        assert!(cut.ends_with(&format!("(cut at {} characters)", MAX_DETAIL_CHARS)));
    }

    #[test]
    fn old_log_lines_without_detail_still_parse() {
        let line = r#"{"timestamp":"t","mode":"daemon","caller":"zed","command":"call","target":"a.b","ok":false,"status":"ERR","latency_ms":1.0}"#;
        let entry: ActivityEntry = serde_json::from_str(line).unwrap();
        assert!(entry.detail.is_none());
        assert!(!serde_json::to_string(&entry).unwrap().contains("detail"));
    }
}

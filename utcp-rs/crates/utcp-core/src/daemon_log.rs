//! `daemon.log`: what the daemon process would write to stderr, namely the error it exits with, any panic
//! (also one inside a single task, which the daemon survives), the problems it finds when it starts (an
//! invalid `settings.toml`, a failed secrets command, required variables still unset), and a secrets command
//! that starts or stops failing later.
//!
//! The daemon's stderr is discarded when it runs detached (`utcp daemon start` without systemd) or from
//! the Windows logon task, so these lines are also written here. They are written only on those events,
//! never per call and never with a secret value, so the file costs nothing on the call path. Like
//! `activity.jsonl`, the file is capped: past [`DAEMON_LOG_CAP_BYTES`] it becomes `daemon.log.bak` and a
//! fresh file starts.

use crate::paths::UtcpPaths;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

pub const DAEMON_LOG_CAP_BYTES: u64 = 256 * 1024;

pub fn daemon_log_file(paths: &UtcpPaths) -> PathBuf {
    paths.state_dir.join("daemon.log")
}

pub fn daemon_log_bak(paths: &UtcpPaths) -> PathBuf {
    paths.state_dir.join("daemon.log.bak")
}

/// Appends one entry: a header line with the time and process id, then `message`.
pub fn append(paths: &UtcpPaths, message: &str) -> std::io::Result<()> {
    let file = daemon_log_file(paths);
    if fs::metadata(&file).is_ok_and(|m| m.len() > DAEMON_LOG_CAP_BYTES) {
        let bak = daemon_log_bak(paths);
        let _ = fs::remove_file(&bak);
        let _ = fs::rename(&file, &bak);
    }
    fs::create_dir_all(&paths.state_dir)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&file)?;
    let entry = format!(
        "{} pid {}\n{}\n",
        chrono::Utc::now().to_rfc3339(),
        std::process::id(),
        message.trim_end()
    );
    out.write_all(entry.as_bytes())
}

/// Logs every panic in this process before the previous hook (the default one prints to stderr).
/// Install it once, in the daemon process only.
pub fn install_panic_hook(paths: &UtcpPaths) {
    let paths = paths.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let _ = append(&paths, &format!("panic in thread '{name}': {info}"));
        previous(info);
    }));
}

/// Current size of `daemon.log` (0 when missing). Pass it to [`read_since`] after an attempt.
pub fn current_len(paths: &UtcpPaths) -> u64 {
    fs::metadata(daemon_log_file(paths)).map_or(0, |m| m.len())
}

/// What was appended after `offset` (from the start if the file rotated since), trimmed;
/// `None` when nothing was.
pub fn read_since(paths: &UtcpPaths, offset: u64) -> Option<String> {
    let mut file = fs::File::open(daemon_log_file(paths)).ok()?;
    let len = file.metadata().ok()?.len();
    let start = if len < offset { 0 } else { offset };
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(base: &std::path::Path) -> UtcpPaths {
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
            separate_setup: true,
        }
    }

    #[test]
    fn appends_entries_and_reads_back_only_the_new_ones() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(tmp.path());
        assert_eq!(current_len(&paths), 0);
        assert_eq!(read_since(&paths, 0), None);

        append(&paths, "first failure").unwrap();
        let offset = current_len(&paths);
        assert!(read_since(&paths, 0).unwrap().ends_with("first failure"));
        assert_eq!(read_since(&paths, offset), None);

        append(&paths, "second failure\n").unwrap();
        let new = read_since(&paths, offset).unwrap();
        assert!(!new.contains("first failure"));
        assert!(new.contains(&format!("pid {}", std::process::id())));
        assert!(new.ends_with("second failure"));
    }

    #[test]
    fn rotates_to_one_backup_past_the_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(tmp.path());
        fs::create_dir_all(&paths.state_dir).unwrap();
        fs::write(daemon_log_bak(&paths), "oldest").unwrap();
        fs::write(
            daemon_log_file(&paths),
            "x".repeat(DAEMON_LOG_CAP_BYTES as usize + 1),
        )
        .unwrap();
        let offset = current_len(&paths);

        append(&paths, "after rotation").unwrap();
        assert!(fs::read_to_string(daemon_log_bak(&paths))
            .unwrap()
            .starts_with("xxx"));
        assert!(current_len(&paths) < 200);
        // The file shrank below the caller's offset, so everything in it is new.
        assert!(read_since(&paths, offset)
            .unwrap()
            .ends_with("after rotation"));
    }

    #[test]
    fn panic_hook_logs_the_message_and_thread() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(tmp.path());
        install_panic_hook(&paths);
        let joined = std::thread::Builder::new()
            .name("hooked".into())
            .spawn(|| panic!("boom for the daemon log"))
            .unwrap()
            .join();
        // Restore the default hook so other tests' panics are not logged here.
        let _ = std::panic::take_hook();
        assert!(joined.is_err());
        let logged = read_since(&paths, 0).unwrap();
        assert!(logged.contains("panic in thread 'hooked'"), "{logged}");
        assert!(logged.contains("boom for the daemon log"), "{logged}");
    }
}

use anyhow::{Context, Result};
use directories::ProjectDirs;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Resolved cross-platform directories and file paths for `utcp`.
#[derive(Debug, Clone)]
pub struct UtcpPaths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub media_dir: PathBuf,
    pub config_file: PathBuf,
    pub settings_file: PathBuf,
    pub env_file: PathBuf,
    pub lock_file: PathBuf,
    pub manuals_dir: PathBuf,
    pub search_index_file: PathBuf,
    pub logging_sentinel: PathBuf,
    pub activity_log_file: PathBuf,
    pub activity_log_bak: PathBuf,
    pub socket_endpoint: String,
    pub daemon_pid_file: PathBuf,
}

impl UtcpPaths {
    /// Resolve paths using `directories::ProjectDirs::from("", "", "utcp")` and environment overrides:
    /// - `UTCP_CONFIG` overrides `.utcp_config.json` path directly
    /// - `UTCP_CONFIG_DIR`, `UTCP_CACHE_DIR`, `UTCP_STATE_DIR`, `UTCP_MEDIA_DIR` override base directories
    /// - `UTCP_SOCKET_PATH` overrides the local IPC socket / pipe endpoint
    ///
    /// The platform folders are looked up only for a directory without an override. On Windows that lookup
    /// goes through the Known Folders API, which fails when `%USERPROFILE%` points at a missing folder.
    pub fn resolve(explicit_config: Option<&Path>) -> Result<Self> {
        let config_override = std::env::var_os("UTCP_CONFIG_DIR").map(PathBuf::from);
        let cache_override = std::env::var_os("UTCP_CACHE_DIR").map(PathBuf::from);
        let state_override = std::env::var_os("UTCP_STATE_DIR").map(PathBuf::from);
        let proj =
            if config_override.is_some() && cache_override.is_some() && state_override.is_some() {
                None
            } else {
                Some(ProjectDirs::from("", "", "utcp").context(
                    "Failed to determine this user's config/cache folders for utcp \
                 (set UTCP_CONFIG_DIR, UTCP_CACHE_DIR and UTCP_STATE_DIR to choose them)",
                )?)
            };
        let proj = || {
            proj.as_ref()
                .expect("looked up when a directory has no override")
        };

        let config_dir = config_override.unwrap_or_else(|| proj().config_dir().to_path_buf());
        let cache_dir = cache_override.unwrap_or_else(|| proj().cache_dir().to_path_buf());
        let state_dir = state_override.unwrap_or_else(|| {
            proj()
                .state_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| proj().data_local_dir().to_path_buf())
        });

        let config_file = if let Some(cfg) = explicit_config {
            cfg.to_path_buf()
        } else if let Some(env_cfg) = std::env::var_os("UTCP_CONFIG") {
            PathBuf::from(env_cfg)
        } else {
            config_dir.join(".utcp_config.json")
        };

        let settings_file = config_dir.join("settings.toml");
        let settings = UtcpSettings::load_from_file(&settings_file);

        let media_dir = resolve_media_dir(settings.media_dir.as_deref());
        let env_file = config_dir.join(".env");
        let lock_file = config_dir.join(".utcp.lock");
        let manuals_dir = cache_dir.join("manuals");
        let search_index_file = cache_dir.join("search_index.bin");
        let logging_sentinel = state_dir.join("logging.enabled");
        let activity_log_file = state_dir.join("activity.jsonl");
        let activity_log_bak = state_dir.join("activity.jsonl.bak");
        let daemon_pid_file = state_dir.join("daemon.pid");

        let socket_endpoint = resolve_socket_endpoint(&cache_dir);

        Ok(Self {
            config_dir,
            cache_dir,
            state_dir,
            media_dir,
            config_file,
            settings_file,
            env_file,
            lock_file,
            manuals_dir,
            search_index_file,
            logging_sentinel,
            activity_log_file,
            activity_log_bak,
            socket_endpoint,
            daemon_pid_file,
        })
    }

    /// Ensure all required directories exist with secure permissions on Unix.
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(&self.cache_dir)?;
        fs::create_dir_all(&self.manuals_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        if let Some(parent) = self.config_file.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.config_dir, fs::Permissions::from_mode(0o700));
            let _ = fs::set_permissions(&self.state_dir, fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }

    /// Ensure the temporary `tmpfs` media spool directory exists with `0700` permissions on Unix.
    /// On Unix it must be a real directory owned by this user: the `/tmp/utcp-media-$USER` fallback has a
    /// predictable name, so another local user could create it first. Such a folder is refused, and the
    /// caller then leaves the media inline.
    pub fn ensure_media_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.media_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = fs::symlink_metadata(&self.media_dir)?;
            // SAFETY: geteuid has no preconditions and cannot fail.
            let uid = unsafe { libc::geteuid() };
            if !meta.is_dir() || meta.uid() != uid {
                anyhow::bail!(
                    "media folder {} is not a directory owned by this user; set UTCP_MEDIA_DIR or `media_dir`",
                    self.media_dir.display()
                );
            }
            if meta.mode() & 0o077 != 0 {
                fs::set_permissions(&self.media_dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    /// Path to a cached manual `.utcp.json` file for `manual_name`.
    pub fn cached_manual_path(&self, manual_name: &str) -> PathBuf {
        let safe_name: String = manual_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.manuals_dir.join(format!("{}.utcp.json", safe_name))
    }

    /// Acquire an exclusive advisory lock on `.utcp.lock` for cold-path mutations.
    pub fn acquire_write_lock(&self) -> Result<AdvisoryLockGuard> {
        self.ensure_dirs()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_file)
            .with_context(|| format!("Failed to open lock file {}", self.lock_file.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("Failed to lock {}", self.lock_file.display()))?;
        Ok(AdvisoryLockGuard { file })
    }
}

fn resolve_media_dir(configured: Option<&str>) -> PathBuf {
    if let Some(env_dir) = std::env::var_os("UTCP_MEDIA_DIR") {
        if !env_dir.is_empty() {
            return PathBuf::from(env_dir);
        }
    }
    if let Some(cfg) = configured {
        if !cfg.trim().is_empty() {
            return PathBuf::from(cfg.trim());
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("utcp-media");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        std::env::temp_dir().join(format!("utcp-media-{}", user))
    }
    #[cfg(windows)]
    {
        std::env::temp_dir().join("utcp-media")
    }
}

fn resolve_socket_endpoint(cache_dir: &Path) -> String {
    if let Ok(explicit) = std::env::var("UTCP_SOCKET_PATH") {
        if !explicit.trim().is_empty() {
            return explicit;
        }
    }
    #[cfg(windows)]
    {
        let _ = cache_dir;
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "default".to_string());
        let safe_user: String = user
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!(r"\\.\pipe\utcp-{}", safe_user)
    }
    #[cfg(not(windows))]
    {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(runtime_dir).join("utcp.sock");
            return p.to_string_lossy().into_owned();
        }
        cache_dir.join("utcp.sock").to_string_lossy().into_owned()
    }
}

pub struct AdvisoryLockGuard {
    file: File,
}

impl Drop for AdvisoryLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Write `bytes` atomically via `.<file>.tmp.<pid>.<nanos>` + `fs::rename` so concurrent lock-free readers
/// never observe partial writes.
pub fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("utcp.tmp");
    let tmp_path = target.with_file_name(format!(".{}.tmp.{}.{}", file_name, pid, nanos));

    {
        let mut f = File::create(&tmp_path)
            .with_context(|| format!("Failed to create temp file {}", tmp_path.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, target).with_context(|| {
        format!(
            "Failed to atomically rename {} -> {}",
            tmp_path.display(),
            target.display()
        )
    })?;
    Ok(())
}

/// User-level management settings persisted in `settings.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtcpSettings {
    #[serde(default = "default_true")]
    pub cache_on_register: bool,
    #[serde(default)]
    pub daemon_idle_timeout_mins: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_dir: Option<String>,
    #[serde(default = "default_max_media_files")]
    pub max_media_files: usize,
    #[serde(default = "default_media_ttl_mins")]
    pub media_ttl_mins: u64,
    /// `[secrets]`: the command that prints the secrets registered manuals need.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsSettings>,
}

/// `[secrets]` in `settings.toml`: a program utcp runs to get the secrets registered manuals need,
/// so the secrets can stay wherever the user keeps them (see [`crate::secrets`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsSettings {
    /// The program and its arguments, one list item each (no shell). A leading `~` is the home
    /// folder. utcp appends the variable names it wants as further arguments.
    pub command: Vec<String>,
    /// Seconds the command may run before utcp stops it.
    #[serde(default = "default_secrets_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_secrets_timeout_secs() -> u64 {
    10
}

fn default_true() -> bool {
    true
}

fn default_max_media_files() -> usize {
    64
}

fn default_media_ttl_mins() -> u64 {
    60
}

impl Default for UtcpSettings {
    fn default() -> Self {
        Self {
            cache_on_register: true,
            daemon_idle_timeout_mins: 0,
            media_dir: None,
            max_media_files: default_max_media_files(),
            media_ttl_mins: default_media_ttl_mins(),
            secrets: None,
        }
    }
}

impl UtcpSettings {
    /// The settings in `path`; the defaults when the file is missing or invalid (see
    /// [`Self::load_checked`] to tell an invalid file apart).
    pub fn load_from_file(path: &Path) -> Self {
        Self::load_checked(path).unwrap_or_default()
    }

    /// Like [`Self::load_from_file`], but an unreadable or invalid file is an error. A missing file
    /// gives the defaults.
    pub fn load_checked(path: &Path) -> Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
        };
        toml::from_str::<Self>(&raw).with_context(|| format!("Invalid {}", path.display()))
    }

    pub fn load(paths: &UtcpPaths) -> Self {
        Self::load_from_file(&paths.settings_file)
    }

    pub fn save(&self, paths: &UtcpPaths) -> Result<()> {
        paths.ensure_dirs()?;
        let serialized = toml::to_string_pretty(self)?;
        write_atomic(&paths.settings_file, serialized.as_bytes())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::UtcpPaths;
    use std::path::Path;

    /// Paths with every folder under `base` (`config/`, `cache/`, `state/`, `media/`).
    pub(crate) fn paths_in(base: &Path) -> UtcpPaths {
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
}

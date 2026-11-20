use crate::paths::{write_atomic, UtcpPaths, UtcpSettings};
use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static MEDIA_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaSpoolStatus {
    pub dir: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub max_files: usize,
    pub ttl_mins: u64,
}

/// Inspect current usage of the temporary media spool directory (`$XDG_RUNTIME_DIR/utcp-media` on Linux).
pub fn inspect_media_spool(paths: &UtcpPaths) -> MediaSpoolStatus {
    let settings = UtcpSettings::load(paths);
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;

    if let Ok(entries) = fs::read_dir(&paths.media_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    file_count += 1;
                    total_bytes += meta.len();
                }
            }
        }
    }

    MediaSpoolStatus {
        dir: paths.media_dir.to_string_lossy().into_owned(),
        file_count,
        total_bytes,
        max_files: settings.max_media_files,
        ttl_mins: settings.media_ttl_mins,
    }
}

/// Delete all spooled media files in `media_dir`.
pub fn clear_media_spool(paths: &UtcpPaths) -> Result<usize> {
    let mut removed = 0usize;
    if let Ok(entries) = fs::read_dir(&paths.media_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Enforce TTL (`media_ttl_mins`) and ring-count cap (`max_media_files`) so `utcp` never litters
/// the filesystem or exhausts `tmpfs`.
pub fn prune_media_spool(paths: &UtcpPaths) {
    let settings = UtcpSettings::load(paths);
    let Ok(entries) = fs::read_dir(&paths.media_dir) else {
        return;
    };

    let now = SystemTime::now();
    let ttl = Duration::from_secs(settings.media_ttl_mins.saturating_mul(60));
    let mut survivors: Vec<(SystemTime, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // Skip in-flight atomic temp files
        if p.file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            continue;
        }

        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
        if settings.media_ttl_mins > 0 {
            if let Ok(age) = now.duration_since(mtime) {
                if age > ttl {
                    let _ = fs::remove_file(&p);
                    continue;
                }
            }
        }
        survivors.push((mtime, p));
    }

    let max_files = settings.max_media_files.max(1);
    if survivors.len() > max_files {
        // Sort oldest first and remove excess
        survivors.sort_by_key(|(mtime, _)| *mtime);
        let to_remove = survivors.len() - max_files;
        for (_, old_path) in survivors.into_iter().take(to_remove) {
            let _ = fs::remove_file(old_path);
        }
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

fn save_media_bytes(paths: &UtcpPaths, bytes: &[u8], mime: &str) -> Result<PathBuf> {
    paths.ensure_media_dir()?;
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = MEDIA_SEQ.fetch_add(1, Ordering::Relaxed);
    let ext = extension_for_mime(mime);
    let file_path = paths.media_dir.join(format!(
        "utcp-media-{}-{}-{}.{}",
        ts_ms,
        std::process::id(),
        seq,
        ext
    ));
    write_atomic(&file_path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&file_path, fs::Permissions::from_mode(0o600));
    }
    prune_media_spool(paths);
    Ok(file_path)
}

/// Recursively inspect a tool response JSON value and intercept MCP-style inline base64 media blocks:
/// - `{ "type": "image" | "audio", "data": "<base64>", "mimeType": "image/png" }`
/// - `{ "blob": "<base64>", "mimeType": "..." }`
///
/// Writes decoded bytes to `paths.media_dir` (`tmpfs` on Linux) and replaces the inline base64 string
/// with a compact `media_file` object so LLM context windows are never flooded with megabytes of base64.
pub fn intercept_inline_media(value: &mut Value, paths: &UtcpPaths) {
    match value {
        Value::Array(items) => {
            for item in items {
                intercept_inline_media(item, paths);
            }
        }
        Value::Object(map) => {
            let block_type = map
                .get("type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_ascii_lowercase());

            if matches!(block_type.as_deref(), Some("image" | "audio")) {
                if let Some(b64) = map.get("data").and_then(|v| v.as_str()) {
                    let mime = map
                        .get("mimeType")
                        .or_else(|| map.get("mime_type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(if block_type.as_deref() == Some("audio") {
                            "audio/wav"
                        } else {
                            "image/png"
                        })
                        .to_string();

                    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&cleaned)
                    {
                        if let Ok(saved_path) = save_media_bytes(paths, &decoded, &mime) {
                            *value = json!({
                                "type": "media_file",
                                "original_type": block_type.unwrap_or_else(|| "image".into()),
                                "mime_type": mime,
                                "bytes": decoded.len(),
                                "path": saved_path.to_string_lossy(),
                                "hint": "Pass this file path to your multimodal file-reading/vision tool if inspection is needed."
                            });
                            return;
                        }
                    }
                }
            }

            // Also intercept MCP embedded binary resource blobs (`{ "blob": "<base64>", "mimeType": "..." }`)
            if let (Some(b64), Some(mime)) = (
                map.get("blob").and_then(|v| v.as_str()),
                map.get("mimeType")
                    .or_else(|| map.get("mime_type"))
                    .and_then(|v| v.as_str()),
            ) {
                let mime_owned = mime.to_string();
                let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if cleaned.len() >= 64 {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&cleaned)
                    {
                        if let Ok(saved_path) = save_media_bytes(paths, &decoded, &mime_owned) {
                            map.remove("blob");
                            map.insert(
                                "spooled_path".to_string(),
                                Value::String(saved_path.to_string_lossy().into_owned()),
                            );
                            map.insert("bytes".to_string(), json!(decoded.len()));
                            map.insert(
                                "hint".to_string(),
                                Value::String(
                                    "Pass spooled_path to your multimodal file-reading/vision tool if inspection is needed."
                                        .to_string(),
                                ),
                            );
                            return;
                        }
                    }
                }
            }

            for child in map.values_mut() {
                intercept_inline_media(child, paths);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(tmp: &TempDir) -> UtcpPaths {
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

    #[cfg(unix)]
    #[test]
    fn media_dir_must_be_a_private_directory_owned_by_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);

        // A symlink planted at the media path (e.g. by another user in /tmp) is refused, so the media
        // stays inline instead of being written where the link points.
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &paths.media_dir).unwrap();
        assert!(paths.ensure_media_dir().is_err());
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"payload");
        let mut resp = json!({ "type": "image", "mimeType": "image/png", "data": b64 });
        intercept_inline_media(&mut resp, &paths);
        assert_eq!(resp["type"], "image");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);

        // An existing folder of ours that others can read is made private.
        fs::remove_file(&paths.media_dir).unwrap();
        fs::create_dir(&paths.media_dir).unwrap();
        fs::set_permissions(&paths.media_dir, fs::Permissions::from_mode(0o755)).unwrap();
        paths.ensure_media_dir().unwrap();
        let mode = fs::metadata(&paths.media_dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // The same block is spooled once the folder is valid.
        intercept_inline_media(&mut resp, &paths);
        assert_eq!(resp["type"], "media_file");
    }

    #[test]
    fn intercepts_mcp_base64_image_and_enforces_max_files_cap() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        let settings = UtcpSettings {
            cache_on_register: true,
            daemon_idle_timeout_mins: 0,
            media_dir: None,
            max_media_files: 3,
            media_ttl_mins: 60,
        };
        settings.save(&paths).unwrap();

        let png_bytes = b"\x89PNG\r\n\x1a\nfake-image-payload";
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);

        for _ in 0..6 {
            let mut mcp_resp = json!({
                "content": [
                    { "type": "text", "text": "Window state captured" },
                    { "type": "image", "mimeType": "image/png", "data": b64 }
                ]
            });
            intercept_inline_media(&mut mcp_resp, &paths);
            assert_eq!(mcp_resp["content"][1]["type"], "media_file");
            assert_eq!(mcp_resp["content"][1]["bytes"], png_bytes.len());
            let spooled = mcp_resp["content"][1]["path"].as_str().unwrap();
            assert!(std::path::Path::new(spooled).exists());
        }

        // Pruning must enforce max_media_files = 3!
        let status = inspect_media_spool(&paths);
        assert_eq!(status.file_count, 3);

        let cleared = clear_media_spool(&paths).unwrap();
        assert_eq!(cleared, 3);
        assert_eq!(inspect_media_spool(&paths).file_count, 0);
    }
}

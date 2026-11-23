//! `settings.toml`: the list of settings utcp knows, the template a new file starts from, and the
//! sync that adds settings a newer utcp knows to an existing file.
//!
//! The file is the user's. utcp creates it from [`template`] when it is missing, and each run adds a
//! setting that is missing entirely (neither an active nor a commented line), in its template state.
//! It never changes or removes a line that is there: a commented line means "off", and an active line
//! keeps its value even when a newer utcp changes the default. A commented section header
//! (`# [secrets]`) turns the whole section off, so keys added to it are commented too.

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use regex::Regex;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::paths::{write_atomic, UtcpPaths};

/// One setting, as the template writes it.
#[derive(Debug, Clone)]
pub struct SettingSpec {
    pub key: &'static str,
    /// The `[table]` it belongs to; `None` for a top-level setting.
    pub section: Option<&'static str>,
    /// The value as TOML text: the default for an active line, an example for a commented one.
    pub value: String,
    /// Whether the template has the line commented out (off).
    pub commented: bool,
    /// The comment above the line, one string per line, without `# `.
    pub comment: &'static [&'static str],
}

/// One `[table]`, as the template writes it.
#[derive(Debug, Clone)]
pub struct SectionSpec {
    pub name: &'static str,
    /// Whether the template has the header commented out, which turns the whole section off.
    pub commented: bool,
    pub comment: &'static [&'static str],
}

const HEADER: &[&str] = &[
    "utcp settings. utcp created this file; change it freely.",
    "A line starting with # is off, and utcp uses its built-in behavior for it.",
    "To change a setting, remove the # and set your value. To undo, put the # back.",
    "Don't delete settings: utcp adds a missing setting back the next time it runs.",
];

/// The sections utcp knows, in template order.
pub fn sections() -> Vec<SectionSpec> {
    vec![SectionSpec {
        name: "secrets",
        commented: true,
        comment: &[
            "A program that prints the secrets manuals need. See \"The Secrets Command\" in operating.md.",
        ],
    }]
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Every setting utcp knows, in template order. This is the one list behind the template, the sync,
/// `utcp doctor`'s unknown-setting check and the settings table in `operating.md`.
pub fn settings() -> Vec<SettingSpec> {
    let secrets_example = if cfg!(windows) {
        r#"["powershell.exe", "-NoProfile", "-File", "~/scripts/secret-source.ps1"]"#
    } else {
        r#"["/bin/bash", "~/scripts/utcp-secret-source.sh"]"#
    };
    vec![
        SettingSpec {
            key: "cache_on_register",
            section: None,
            value: "true".into(),
            commented: false,
            comment: &["Keep a copy of a manual's tools when it is registered."],
        },
        SettingSpec {
            key: "daemon_idle_timeout_mins",
            section: None,
            value: "0".into(),
            commented: false,
            comment: &["Stop the daemon after this many idle minutes. 0 keeps it running."],
        },
        SettingSpec {
            key: "max_upload_mb",
            section: None,
            value: "100".into(),
            commented: false,
            comment: &[
                "Largest file a tool may upload, in MB. 0 turns uploads off. Commented out: no limit.",
            ],
        },
        SettingSpec {
            key: "max_download_mb",
            section: None,
            value: "500".into(),
            commented: true,
            comment: &[
                "Largest file a tool may download, in MB. 0 turns downloads off. Commented out: no limit.",
            ],
        },
        SettingSpec {
            key: "download_dir",
            section: None,
            value: toml_string("~/Downloads"),
            commented: true,
            comment: &["Where downloaded files go. Commented out: your Downloads folder."],
        },
        SettingSpec {
            key: "media_dir",
            section: None,
            value: toml_string(&crate::paths::default_media_dir().to_string_lossy()),
            commented: true,
            comment: &[
                "Where images and audio from MCP servers go. Commented out: the folder shown here.",
                "The UTCP_MEDIA_DIR environment variable overrides this.",
            ],
        },
        SettingSpec {
            key: "max_media_files",
            section: None,
            value: "64".into(),
            commented: false,
            comment: &["How many of those media files to keep."],
        },
        SettingSpec {
            key: "media_ttl_mins",
            section: None,
            value: "60".into(),
            commented: false,
            comment: &["Minutes to keep each media file."],
        },
        SettingSpec {
            key: "command",
            section: Some("secrets"),
            value: secrets_example.into(),
            commented: true,
            comment: &[],
        },
        SettingSpec {
            key: "timeout_secs",
            section: Some("secrets"),
            value: "10".into(),
            commented: true,
            comment: &[],
        },
    ]
}

fn comment_lines(comment: &[&str]) -> Vec<String> {
    comment.iter().map(|c| format!("# {c}")).collect()
}

fn key_line(spec: &SettingSpec, commented: bool) -> String {
    let hash = if commented { "# " } else { "" };
    format!("{hash}{} = {}", spec.key, spec.value)
}

fn section_lines(section: &SectionSpec, specs: &[SettingSpec]) -> Vec<String> {
    let mut lines = comment_lines(section.comment);
    let hash = if section.commented { "# " } else { "" };
    lines.push(format!("{hash}[{}]", section.name));
    for spec in specs.iter().filter(|s| s.section == Some(section.name)) {
        lines.extend(comment_lines(spec.comment));
        lines.push(key_line(spec, section.commented || spec.commented));
    }
    lines
}

/// The file utcp writes when there is no `settings.toml`.
pub fn template() -> String {
    let specs = settings();
    let mut lines = comment_lines(HEADER);
    for spec in specs.iter().filter(|s| s.section.is_none()) {
        lines.push(String::new());
        lines.extend(comment_lines(spec.comment));
        lines.push(key_line(spec, spec.commented));
    }
    for section in sections() {
        lines.push(String::new());
        lines.extend(section_lines(&section, &specs));
    }
    lines.join("\n") + "\n"
}

/// What [`sync_settings_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing was missing.
    Unchanged,
    /// There was no file; utcp wrote the template.
    Created,
    /// utcp added these lines (as written, so a commented one starts with `#`) and saved the old
    /// file as `settings.toml.bak`.
    Added(Vec<String>),
    /// Another utcp held the lock; the next run adds what is missing.
    Busy,
}

fn header_re() -> Regex {
    Regex::new(r"^\s*(#\s*)?\[\s*([A-Za-z0-9_.-]+)\s*\]\s*(#.*)?$").expect("valid regex")
}

fn key_re(key: &str) -> Regex {
    Regex::new(&format!(r"^\s*#?\s*{}\s*=", regex::escape(key))).expect("valid regex")
}

/// A `[name]` or `# [name]` line.
struct HeaderLine {
    idx: usize,
    name: String,
    commented: bool,
}

/// `text` with every setting that is missing entirely added in its template state, and the lines
/// added. `None` when nothing is missing.
pub fn add_missing_settings(text: &str) -> Option<(String, Vec<String>)> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header_re = header_re();
    let headers: Vec<HeaderLine> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            header_re.captures(line).map(|c| HeaderLine {
                idx,
                name: c[2].to_string(),
                commented: c.get(1).is_some(),
            })
        })
        .collect();
    let region_end = |start: usize| {
        headers
            .iter()
            .map(|h| h.idx)
            .find(|&i| i > start)
            .unwrap_or(lines.len())
    };
    let present = |range: std::ops::Range<usize>, key: &str| {
        let re = key_re(key);
        lines[range].iter().any(|l| re.is_match(l))
    };

    let specs = settings();
    // (line index to insert before, lines), applied from the end so earlier indexes stay valid.
    let mut inserts: Vec<(usize, Vec<String>)> = Vec::new();
    let mut added = Vec::new();

    // Top-level settings go above the first header and the comment block right above it.
    let top_end = headers.first().map_or(lines.len(), |h| h.idx);
    let mut top_block = Vec::new();
    for spec in specs.iter().filter(|s| s.section.is_none()) {
        if !present(0..top_end, spec.key) {
            top_block.extend(comment_lines(spec.comment));
            let line = key_line(spec, spec.commented);
            added.push(line.clone());
            top_block.push(line);
            top_block.push(String::new());
        }
    }
    if !top_block.is_empty() {
        let mut at = top_end;
        if headers.is_empty() {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                top_block.insert(0, String::new());
            }
            top_block.pop();
        } else {
            while at > 0 && lines[at - 1].trim_start().starts_with('#') {
                at -= 1;
            }
            if at > 0 && !lines[at - 1].trim().is_empty() {
                top_block.insert(0, String::new());
            }
        }
        inserts.push((at, top_block));
    }

    for section in sections() {
        let section_specs: Vec<&SettingSpec> = specs
            .iter()
            .filter(|s| s.section == Some(section.name))
            .collect();
        let found = headers
            .iter()
            .filter(|h| h.name == section.name)
            .min_by_key(|h| (h.commented, h.idx));
        match found {
            Some(h) => {
                let end = region_end(h.idx);
                let mut block = Vec::new();
                for spec in &section_specs {
                    if !present(h.idx + 1..end, spec.key) {
                        block.extend(comment_lines(spec.comment));
                        let line = key_line(spec, h.commented || spec.commented);
                        added.push(line.clone());
                        block.push(line);
                    }
                }
                if !block.is_empty() {
                    let mut at = end;
                    while at > h.idx + 1 && lines[at - 1].trim().is_empty() {
                        at -= 1;
                    }
                    inserts.push((at, block));
                }
            }
            None => {
                let owned: Vec<SettingSpec> = section_specs.into_iter().cloned().collect();
                let mut block = vec![String::new()];
                block.extend(section_lines(&section, &owned));
                added.push(format!(
                    "{}[{}]",
                    if section.commented { "# " } else { "" },
                    section.name
                ));
                for spec in &owned {
                    added.push(key_line(spec, section.commented || spec.commented));
                }
                inserts.push((lines.len(), block));
            }
        }
    }

    if added.is_empty() {
        return None;
    }
    inserts.sort_by_key(|(at, _)| *at);
    for (at, block) in inserts.into_iter().rev() {
        lines.splice(at..at, block);
    }
    Some((lines.join(newline) + newline, added))
}

/// Create `settings.toml` from the template when it is missing, or add the settings it lacks.
/// An invalid file is left alone (an error); so is a file another utcp is changing (`Busy`).
pub fn sync_settings_file(paths: &UtcpPaths) -> Result<SyncOutcome> {
    let file = &paths.settings_file;
    match fs::read_to_string(file) {
        Ok(text) => {
            check_toml(file, &text)?;
            if add_missing_settings(&text).is_none() {
                return Ok(SyncOutcome::Unchanged);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", file.display())),
    }

    paths.ensure_dirs()?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock_file)
        .with_context(|| format!("Failed to open lock file {}", paths.lock_file.display()))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(SyncOutcome::Busy);
    }
    let outcome = sync_locked(file);
    let _ = FileExt::unlock(&lock);
    outcome
}

fn sync_locked(file: &Path) -> Result<SyncOutcome> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_atomic(file, template().as_bytes())?;
            return Ok(SyncOutcome::Created);
        }
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", file.display())),
    };
    check_toml(file, &text)?;
    let Some((new_text, added)) = add_missing_settings(&text) else {
        return Ok(SyncOutcome::Unchanged);
    };
    // Never write a file utcp itself would then refuse.
    check_toml(file, &new_text).context("utcp could not add the new settings safely")?;
    fs::copy(file, backup_path(file))
        .with_context(|| format!("Failed to back up {}", file.display()))?;
    write_atomic(&link_target(file), new_text.as_bytes())?;
    Ok(SyncOutcome::Added(added))
}

/// The file a symlinked `settings.toml` (for example one kept in a dotfiles repository) points to, so writing it
/// keeps the link; `file` itself otherwise.
fn link_target(file: &Path) -> PathBuf {
    match fs::symlink_metadata(file) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf())
        }
        _ => file.to_path_buf(),
    }
}

/// `settings.toml.bak`, next to the file.
pub fn backup_path(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    file.with_file_name(name)
}

fn check_toml(file: &Path, text: &str) -> Result<()> {
    text.parse::<toml::Table>()
        .map(|_| ())
        .map_err(|e| anyhow!("{} is not valid TOML: {}", file.display(), e.message()))
}

/// Set one top-level setting and leave every other line, comments included, as it is. A commented
/// line for the key is uncommented; without any line, the key is added above the first section.
pub fn set_top_level_setting(paths: &UtcpPaths, key: &str, value: toml_edit::Value) -> Result<()> {
    let _lock = paths.acquire_write_lock()?;
    let file = &paths.settings_file;
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => template(),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", file.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow!("{} is not valid TOML: {}", file.display(), e))?;
    if !doc.contains_key(key) {
        // Turn `# key = ...` above the first section into `key = <value>` in place.
        let header_re = header_re();
        let key_re = key_re(key);
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let top_end = lines
            .iter()
            .position(|l| header_re.is_match(l))
            .unwrap_or(lines.len());
        if let Some(i) = lines[..top_end].iter().position(|l| key_re.is_match(l)) {
            lines[i] = format!("{key} = {value}");
            let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
            doc = (lines.join(newline) + newline)
                .parse()
                .map_err(|e| anyhow!("{} is not valid TOML: {}", file.display(), e))?;
        }
    }
    doc[key] = toml_edit::Item::Value(value);
    write_atomic(&link_target(file), doc.to_string().as_bytes())
}

/// A setting in `settings.toml` that utcp does not know, so it has no effect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnknownSetting {
    /// `key`, or `section.key` for a key in a table.
    pub key: String,
    /// What probably went wrong, when utcp can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The settings in `text` that utcp does not know. Invalid TOML gives none (that is reported
/// separately).
pub fn unknown_settings(text: &str) -> Vec<UnknownSetting> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let specs = settings();
    let section_names: Vec<&str> = sections().iter().map(|s| s.name).collect();
    let section_of = |key: &str| {
        specs
            .iter()
            .find(|s| s.section.is_some() && s.key == key)
            .and_then(|s| s.section)
    };
    let is_top_level = |key: &str| specs.iter().any(|s| s.section.is_none() && s.key == key);
    let mut unknown = Vec::new();
    for (key, value) in &table {
        if is_top_level(key) {
            continue;
        }
        if section_names.contains(&key.as_str()) {
            if let toml::Value::Table(inner) = value {
                for inner_key in inner.keys() {
                    if specs
                        .iter()
                        .any(|s| s.section == Some(key.as_str()) && s.key == inner_key)
                    {
                        continue;
                    }
                    let hint = is_top_level(inner_key).then(|| {
                        format!(
                            "{inner_key} is a top-level setting, so it has to come before the first [section] line"
                        )
                    });
                    unknown.push(UnknownSetting {
                        key: format!("{key}.{inner_key}"),
                        hint,
                    });
                }
                continue;
            }
        }
        let hint = section_of(key).map(|section| {
            format!("{key} belongs in [{section}]; was its section header commented out?")
        });
        unknown.push(UnknownSetting {
            key: key.clone(),
            hint,
        });
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{test_support::paths_in, UtcpSettings};

    #[test]
    fn the_template_gives_the_built_in_settings_plus_the_upload_limit() {
        let parsed: UtcpSettings = toml::from_str(&template()).unwrap();
        let defaults = UtcpSettings::default();
        assert_eq!(parsed.cache_on_register, defaults.cache_on_register);
        assert_eq!(
            parsed.daemon_idle_timeout_mins,
            defaults.daemon_idle_timeout_mins
        );
        assert_eq!(parsed.max_media_files, defaults.max_media_files);
        assert_eq!(parsed.media_ttl_mins, defaults.media_ttl_mins);
        assert_eq!(parsed.max_upload_mb, Some(100));
        assert_eq!(parsed.max_download_mb, None);
        assert_eq!(parsed.download_dir, None);
        assert_eq!(parsed.media_dir, None);
        assert!(parsed.secrets.is_none());
        assert!(unknown_settings(&template()).is_empty());
        assert!(add_missing_settings(&template()).is_none());
    }

    /// Every setting in the list is a field of `UtcpSettings`, and every field is in the list.
    #[test]
    fn the_list_matches_the_settings_struct() {
        let specs = settings();
        let mut all_on = String::new();
        for spec in specs.iter().filter(|s| s.section.is_none()) {
            all_on += &format!("{} = {}\n", spec.key, spec.value);
        }
        for section in sections() {
            all_on += &format!("[{}]\n", section.name);
            for spec in specs.iter().filter(|s| s.section == Some(section.name)) {
                all_on += &format!("{} = {}\n", spec.key, spec.value);
            }
        }
        let parsed: UtcpSettings = toml::from_str(&all_on).unwrap();
        let round_trip: toml::Table = toml::from_str(&toml::to_string(&parsed).unwrap()).unwrap();
        let written: toml::Table = toml::from_str(&all_on).unwrap();
        let keys = |t: &toml::Table| {
            let mut keys: Vec<String> = Vec::new();
            for (k, v) in t {
                match v {
                    toml::Value::Table(inner) => {
                        keys.extend(inner.keys().map(|i| format!("{k}.{i}")))
                    }
                    _ => keys.push(k.clone()),
                }
            }
            keys.sort();
            keys
        };
        assert_eq!(keys(&round_trip), keys(&written));
        let defaults: toml::Table =
            toml::from_str(&toml::to_string(&UtcpSettings::default()).unwrap()).unwrap();
        for key in keys(&defaults) {
            assert!(keys(&written).contains(&key), "{key} is not in settings()");
        }
    }

    #[test]
    fn operating_md_lists_every_setting() {
        let doc = include_str!("../../../docs/operating.md");
        for spec in settings() {
            assert!(
                doc.contains(&format!("`{}`", spec.key)),
                "operating.md does not mention `{}`",
                spec.key
            );
        }
    }

    #[test]
    fn missing_settings_are_added_above_the_first_section_and_lines_stay() {
        // An older file: a changed value, a commented line, and an active [secrets] at the end.
        let old = "# my notes\ncache_on_register = false\n# daemon_idle_timeout_mins = 5\n\n\
                   max_media_files = 64\nmedia_ttl_mins = 60\n\n[secrets]\ncommand = [\"x\"]\n";
        let (new, added) = add_missing_settings(old).unwrap();
        assert_eq!(
            added,
            vec![
                "max_upload_mb = 100",
                "# max_download_mb = 500",
                &format!("# download_dir = {}", toml_string("~/Downloads")),
                &format!(
                    "# media_dir = {}",
                    toml_string(&crate::paths::default_media_dir().to_string_lossy())
                ),
                "# timeout_secs = 10",
            ]
        );
        let parsed: UtcpSettings = toml::from_str(&new).unwrap();
        assert!(!parsed.cache_on_register, "an active value is kept");
        assert_eq!(
            parsed.daemon_idle_timeout_mins, 0,
            "a commented line stays off"
        );
        assert_eq!(parsed.max_upload_mb, Some(100));
        assert_eq!(parsed.secrets.unwrap().command, vec!["x".to_string()]);
        assert!(new.starts_with(
            "# my notes\ncache_on_register = false\n# daemon_idle_timeout_mins = 5\n"
        ));
        let upload = new.find("max_upload_mb").unwrap();
        assert!(upload < new.find("[secrets]").unwrap());
        assert!(new.ends_with("command = [\"x\"]\n# timeout_secs = 10\n"));
        assert!(
            add_missing_settings(&new).is_none(),
            "a second run adds nothing"
        );
    }

    #[test]
    fn a_commented_section_header_gets_commented_keys_and_a_missing_section_is_appended() {
        let off = "cache_on_register = true\n\n# [secrets]\n# command = [\"x\"]\n";
        let (new, added) = add_missing_settings(off).unwrap();
        assert!(added.contains(&"# timeout_secs = 10".to_string()));
        assert!(new.contains("# [secrets]\n# command = [\"x\"]\n# timeout_secs = 10\n"));
        assert!(toml::from_str::<UtcpSettings>(&new)
            .unwrap()
            .secrets
            .is_none());

        let none = "cache_on_register = true\n";
        let (new, added) = add_missing_settings(none).unwrap();
        assert!(added.contains(&"# [secrets]".to_string()));
        assert!(new.trim_end().ends_with("# timeout_secs = 10"));
        let parsed: UtcpSettings = toml::from_str(&new).unwrap();
        assert!(parsed.secrets.is_none());
        assert_eq!(parsed.max_upload_mb, Some(100));
    }

    #[test]
    fn sync_creates_adds_with_a_backup_and_leaves_an_invalid_file_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        assert_eq!(sync_settings_file(&paths).unwrap(), SyncOutcome::Created);
        assert_eq!(
            fs::read_to_string(&paths.settings_file).unwrap(),
            template()
        );
        assert_eq!(sync_settings_file(&paths).unwrap(), SyncOutcome::Unchanged);

        fs::write(&paths.settings_file, "cache_on_register = false\n").unwrap();
        assert!(matches!(
            sync_settings_file(&paths).unwrap(),
            SyncOutcome::Added(_)
        ));
        assert_eq!(
            fs::read_to_string(backup_path(&paths.settings_file)).unwrap(),
            "cache_on_register = false\n"
        );
        assert!(!UtcpSettings::load(&paths).cache_on_register);

        fs::write(&paths.settings_file, "cache_on_register = \n").unwrap();
        assert!(sync_settings_file(&paths).is_err());
        assert_eq!(
            fs::read_to_string(&paths.settings_file).unwrap(),
            "cache_on_register = \n"
        );
    }

    /// A settings file kept elsewhere (dotfiles) and linked in stays a link; the change lands in the real file.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_stays_a_link() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        fs::create_dir_all(&paths.config_dir).unwrap();
        let real = tmp.path().join("dotfiles-settings.toml");
        fs::write(&real, "cache_on_register = false\n").unwrap();
        std::os::unix::fs::symlink(&real, &paths.settings_file).unwrap();
        assert!(matches!(
            sync_settings_file(&paths).unwrap(),
            SyncOutcome::Added(_)
        ));
        set_top_level_setting(&paths, "cache_on_register", true.into()).unwrap();
        let meta = fs::symlink_metadata(&paths.settings_file).unwrap();
        assert!(meta.file_type().is_symlink());
        let text = fs::read_to_string(&real).unwrap();
        assert!(text.contains("max_upload_mb = 100"));
        assert!(text.starts_with("cache_on_register = true\n"), "{text}");
    }

    #[test]
    fn setting_one_value_keeps_comments_and_uncomments_its_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        sync_settings_file(&paths).unwrap();
        set_top_level_setting(&paths, "cache_on_register", false.into()).unwrap();
        let text = fs::read_to_string(&paths.settings_file).unwrap();
        assert_eq!(
            text,
            template().replace("cache_on_register = true", "cache_on_register = false")
        );

        let text = text.replace(
            "daemon_idle_timeout_mins = 0",
            "# daemon_idle_timeout_mins = 0",
        );
        fs::write(&paths.settings_file, &text).unwrap();
        set_top_level_setting(&paths, "daemon_idle_timeout_mins", 5i64.into()).unwrap();
        let after = fs::read_to_string(&paths.settings_file).unwrap();
        assert!(
            after.contains("\ndaemon_idle_timeout_mins = 5\n"),
            "{after}"
        );
        assert!(!after.contains("# daemon_idle_timeout_mins"));
        assert!(after.contains("# [secrets]"));
    }

    #[test]
    fn unknown_settings_say_what_probably_went_wrong() {
        let text = "cache_on_register = true\nfoo = 1\ncommand = [\"x\"]\n\n[secrets]\ncommand = [\"y\"]\nmax_upload_mb = 5\n";
        let found = unknown_settings(text);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found.iter().find(|u| u.key == "foo").unwrap().hint, None);
        assert!(found
            .iter()
            .find(|u| u.key == "command")
            .unwrap()
            .hint
            .as_deref()
            .unwrap()
            .contains("section header commented out"));
        assert!(found
            .iter()
            .find(|u| u.key == "secrets.max_upload_mb")
            .unwrap()
            .hint
            .as_deref()
            .unwrap()
            .contains("before the first [section]"));
    }
}

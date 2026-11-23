//! Download Microsoft's Azure DevOps specs from the Software Heritage archive, verified by git blob hash.
//!
//! The GitHub repository MicrosoftDocs/vsts-rest-api-specs is gone, so the files come from the archive's copy of it.
//! `pins.json` records the git blob hash of every file the generator uses, so a normal run downloads only those
//! files (one request each) and refuses anything whose hash differs. Downloads are cached by hash and never go stale.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

use crate::http::{Request, Transport};

pub const API: &str = "https://archive.softwareheritage.org/api/1";
/// The archive answers 403 to requests without an identifying User-Agent.
pub const USER_AGENT: &str = concat!(
    "azdo-manuals/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Faraclas/agent-misc)"
);
/// The pins this binary was built with. `pins --pins <file>` writes new ones; rebuild to use them.
pub const PINS: &str = include_str!("../pins.json");

#[derive(Debug)]
pub struct ArchiveError(pub String);

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArchiveError {}

type Result<T> = std::result::Result<T, ArchiveError>;

/// The hash git (and the archive's `sha1_git`) gives a file's content.
pub fn git_blob_sha1(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", data.len()).as_bytes());
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `%LOCALAPPDATA%` on Windows.
pub fn local_app_data() -> PathBuf {
    env_dir("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local"))
}

pub fn default_cache_dir() -> PathBuf {
    if cfg!(windows) {
        return local_app_data().join("azdo-manuals").join("cache");
    }
    env_dir("XDG_CACHE_HOME")
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("azdo-manuals")
}

/// Where `generate` writes an organization's manuals when no `--out` is given. Private to the user.
pub fn default_out_dir(org: &str) -> PathBuf {
    if cfg!(windows) {
        return local_app_data()
            .join("azdo-manuals")
            .join("manuals")
            .join(org);
    }
    let data = env_dir("XDG_DATA_HOME").unwrap_or_else(|| home_dir().join(".local").join("share"));
    data.join("azdo-manuals").join(org)
}

pub fn parse_pins(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| ArchiveError(format!("pins are not valid JSON: {e}")))
}

pub fn load_pins(path: Option<&Path>) -> Result<Value> {
    match path {
        None => parse_pins(PINS),
        Some(p) => {
            let text = fs::read_to_string(p)
                .map_err(|e| ArchiveError(format!("cannot read {}: {e}", p.display())))?;
            parse_pins(&text)
        }
    }
}

/// Minimal client for the archive's public API (no account needed, 120 requests per hour).
pub struct Archive {
    api: String,
    transport: Transport,
    pub requests: usize,
    pub remaining: Option<String>,
}

impl Archive {
    pub fn new(api: &str, transport: Transport) -> Archive {
        Archive {
            api: api.trim_end_matches('/').to_string(),
            transport,
            requests: 0,
            remaining: None,
        }
    }

    fn get(&mut self, path: &str) -> Result<Vec<u8>> {
        let req = Request {
            method: "GET",
            url: format!("{}{path}", self.api),
            headers: vec![("User-Agent".into(), USER_AGENT.into())],
            body: None,
        };
        let resp = (self.transport)(&req)
            .map_err(|e| ArchiveError(format!("cannot reach the archive ({e})")))?;
        self.requests += 1;
        if resp.status == 429 {
            let when = resp
                .header("X-RateLimit-Reset")
                .and_then(|r| r.parse::<i64>().ok())
                .and_then(|r| chrono::DateTime::from_timestamp(r, 0))
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_else(|| "later".into());
            return Err(ArchiveError(format!(
                "the archive's rate limit (120 requests per hour) is used up; try again after {when}"
            )));
        }
        if !(200..300).contains(&resp.status) {
            return Err(ArchiveError(format!(
                "archive request {path} failed: HTTP {}",
                resp.status
            )));
        }
        if let Some(r) = resp.header("X-RateLimit-Remaining") {
            self.remaining = Some(r.to_string());
        }
        Ok(resp.body)
    }

    pub fn get_json(&mut self, path: &str) -> Result<Value> {
        let data = self.get(path)?;
        serde_json::from_slice(&data)
            .map_err(|e| ArchiveError(format!("archive request {path}: bad JSON ({e})")))
    }

    pub fn get_content(&mut self, sha1_git: &str) -> Result<Vec<u8>> {
        self.get(&format!("/content/sha1_git:{sha1_git}/raw/"))
    }

    /// One entry (`type`, `target`, `length`) at `path` below `directory`.
    pub fn lookup(&mut self, directory: &str, path: &str) -> Result<Value> {
        self.get_json(&format!(
            "/directory/{directory}/{}/",
            path.trim_matches('/')
        ))
    }

    pub fn listing(&mut self, directory: &str) -> Result<Vec<Value>> {
        match self.get_json(&format!("/directory/{directory}/"))? {
            Value::Array(entries) => Ok(entries),
            _ => Err(ArchiveError(format!(
                "archive listing of {directory} is not a list"
            ))),
        }
    }
}

/// A pinned file's bytes from the cache or the archive, refusing a hash mismatch.
pub fn fetch_file(archive: &mut Archive, pin: &Value, cache_dir: &Path) -> Result<Vec<u8>> {
    let sha = pin["sha1_git"].as_str().unwrap_or("");
    let file = pin["file"].as_str().unwrap_or("");
    let cached = cache_dir.join(format!("{sha}.json"));
    if cached.is_file() {
        if let Ok(data) = fs::read(&cached) {
            if git_blob_sha1(&data) == sha {
                return Ok(data);
            }
        }
        let _ = fs::remove_file(&cached);
    }
    let data = archive.get_content(sha)?;
    let actual = git_blob_sha1(&data);
    if actual != sha {
        return Err(ArchiveError(format!(
            "{file}: the archive sent content with hash {actual}, pinned {sha}"
        )));
    }
    let io_err = |e: std::io::Error| {
        ArchiveError(format!(
            "cannot write the cache {}: {e}",
            cache_dir.display()
        ))
    };
    fs::create_dir_all(cache_dir).map_err(io_err)?;
    let tmp = cache_dir.join(format!("{sha}.tmp{}", std::process::id()));
    fs::write(&tmp, &data).map_err(io_err)?;
    fs::rename(&tmp, &cached).map_err(io_err)?;
    Ok(data)
}

pub fn load_spec(data: &[u8]) -> Result<Value> {
    // Some files start with a UTF-8 byte order mark.
    let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    serde_json::from_slice(data)
        .map_err(|e| ArchiveError(format!("a spec file is not valid JSON ({e})")))
}

/// Python's `str.splitlines()[0]`.
fn first_line(text: &str) -> &str {
    let breaks = [
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ];
    text.split(|c| breaks.contains(&c)).next().unwrap_or("")
}

/// List `specification/<area>/<api_version>/*.json` at `revision` and return their pins.
///
/// Costs 3 requests plus 2 per area. `existing` (an earlier, partial result for the same revision and version) is
/// reused, so a run cut short by the rate limit can continue where it stopped. Areas without that version are
/// recorded under `missing` with the versions they do have. `save(result)` is called after each area.
pub fn discover_pins(
    archive: &mut Archive,
    revision: &str,
    api_version: &str,
    areas: &[String],
    existing: Option<Value>,
    log: &mut dyn FnMut(&str),
    save: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<Value> {
    let reusable = existing.filter(|e| {
        e.get("revision").and_then(Value::as_str) == Some(revision)
            && e.get("api_version").and_then(Value::as_str) == Some(api_version)
    });
    let mut result = match reusable {
        Some(e) => e,
        None => {
            let rev = archive.get_json(&format!("/revision/{revision}/"))?;
            let message = match rev.get("message").and_then(Value::as_str) {
                Some(m) if !m.is_empty() => json!(first_line(m)),
                _ => Value::Null,
            };
            let root = rev
                .get("directory")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let spec_dir = archive.lookup(&root, "specification")?;
            json!({
                "revision": revision,
                "revision_date": rev.get("date").cloned().unwrap_or(Value::Null),
                "revision_message": message,
                "specification_dir": spec_dir.get("target").cloned().unwrap_or(Value::Null),
                "api_version": api_version,
                "areas": {},
                "missing": {},
            })
        }
    };
    let root = result
        .as_object_mut()
        .ok_or_else(|| ArchiveError("pins are not a JSON object".into()))?;
    root.entry("areas").or_insert_with(|| json!({}));
    root.entry("missing").or_insert_with(|| json!({}));
    let has = |result: &Value, key: &str, area: &str| result[key].get(area).is_some();
    let todo: Vec<&String> = areas
        .iter()
        .filter(|a| !has(&result, "areas", a) && !has(&result, "missing", a))
        .collect();
    if todo.is_empty() {
        return Ok(result);
    }
    let spec_dir = result["specification_dir"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut area_dirs = Map::new();
    for e in archive.listing(&spec_dir)? {
        if e["type"] == "dir" {
            if let Some(name) = e["name"].as_str() {
                area_dirs.insert(name.to_string(), e.clone());
            }
        }
    }
    // Every area of the spec folder, so a test can require each to be in a manual or excluded with a reason.
    result["archive_areas"] = json!(area_dirs.keys().collect::<Vec<_>>());
    for area in todo {
        let Some(dir) = area_dirs.get(area) else {
            return Err(ArchiveError(format!(
                "area '{area}' is not in the archived specification folder"
            )));
        };
        let versions = archive.listing(dir["target"].as_str().unwrap_or(""))?;
        let version_dir = versions
            .iter()
            .find(|e| e["name"] == api_version && e["type"] == "dir");
        let Some(version_dir) = version_dir else {
            let mut names: Vec<String> = versions
                .iter()
                .filter(|e| e["type"] == "dir")
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect();
            names.sort();
            let has = if names.is_empty() {
                "nothing".to_string()
            } else {
                names.join(", ")
            };
            log(&format!("  {area}: no {api_version} folder (has {has})"));
            result["missing"][area] = json!(names);
            save(&result)?;
            continue;
        };
        let files: Vec<Value> = archive
            .listing(version_dir["target"].as_str().unwrap_or(""))?
            .into_iter()
            .filter(|e| {
                e["type"] == "file" && e["name"].as_str().is_some_and(|n| n.ends_with(".json"))
            })
            .map(|e| json!({"file": e["name"], "sha1_git": e["target"], "length": e["length"]}))
            .collect();
        let names: Vec<&str> = files.iter().filter_map(|p| p["file"].as_str()).collect();
        let shown = if names.is_empty() {
            "(no json files)".to_string()
        } else {
            names.join(", ")
        };
        log(&format!("  {area}: {shown}"));
        result["areas"][area] = Value::Array(files);
        save(&result)?;
    }
    Ok(result)
}

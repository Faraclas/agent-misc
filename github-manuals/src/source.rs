//! GitHub's OpenAPI description: the pinned file, its download and cache, and the default folders.
//!
//! The description is downloaded from raw.githubusercontent.com at the pinned commit and checked against the pinned
//! SHA-256 before it is used or cached. It is never stored in this repository.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::{Request, Transport};
use crate::text::write_atomic;

pub const USER_AGENT: &str = concat!(
    "github-manuals/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/Faraclas/agent-misc)"
);
/// The pins this binary was built with. `pins --commit <sha>` writes new ones; rebuild to use them.
pub const PINS: &str = include_str!("../pins.json");
const RAW_BASE: &str = "https://raw.githubusercontent.com";
const API_BASE: &str = "https://api.github.com";

#[derive(Debug)]
pub struct SourceError(pub String);

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, SourceError>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(SourceError(msg.into()))
}

/// The pinned source: repository, commit, file path, API version, SHA-256 and length.
#[derive(Clone, Debug)]
pub struct Pins {
    pub repository: String,
    pub commit: String,
    pub commit_date: String,
    pub file: String,
    pub api_version: String,
    pub sha256: String,
    pub length: u64,
}

impl Pins {
    pub fn parse(text: &str) -> Result<Pins> {
        let v: Value = serde_json::from_str(text)
            .map_err(|e| SourceError(format!("pins file: not valid JSON ({e})")))?;
        let s = |k: &str| -> Result<String> {
            match v[k].as_str() {
                Some(s) if !s.is_empty() => Ok(s.to_string()),
                _ => fail(format!("pins file: '{k}' is missing")),
            }
        };
        Ok(Pins {
            repository: s("repository")?,
            commit: s("commit")?,
            commit_date: s("commit_date")?,
            file: s("file")?,
            api_version: s("api_version")?,
            sha256: s("sha256")?,
            length: v["length"].as_u64().unwrap_or(0),
        })
    }

    pub fn load(path: Option<&Path>) -> Result<Pins> {
        match path {
            None => Pins::parse(PINS),
            Some(p) => {
                let text = fs::read_to_string(p)
                    .map_err(|e| SourceError(format!("{}: {e}", p.display())))?;
                Pins::parse(&text)
            }
        }
    }

    pub fn url(&self) -> String {
        format!(
            "{RAW_BASE}/{}/{}/{}",
            self.repository, self.commit, self.file
        )
    }

    /// What the report and the manuals say about where they came from.
    pub fn source_json(&self) -> Value {
        serde_json::json!({
            "repository": self.repository,
            "commit": self.commit,
            "commit_date": self.commit_date,
            "file": self.file,
            "sha256": self.sha256,
        })
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "repository": self.repository,
            "commit": self.commit,
            "commit_date": self.commit_date,
            "file": self.file,
            "api_version": self.api_version,
            "sha256": self.sha256,
            "length": self.length,
        })
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    env_dir(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%LOCALAPPDATA%` on Windows.
fn local_app_data() -> PathBuf {
    env_dir("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local"))
}

pub fn default_cache_dir() -> PathBuf {
    if cfg!(windows) {
        return local_app_data().join("github-manuals").join("cache");
    }
    env_dir("XDG_CACHE_HOME")
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("github-manuals")
}

pub fn default_out_dir() -> PathBuf {
    if cfg!(windows) {
        return local_app_data().join("github-manuals").join("manuals");
    }
    env_dir("XDG_DATA_HOME")
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
        .join("github-manuals")
}

fn get(transport: &mut Transport, url: &str, accept: &str) -> Result<Vec<u8>> {
    let req = Request {
        method: "GET",
        url: url.to_string(),
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Accept".into(), accept.into()),
        ],
        body: None,
    };
    let resp = transport(&req).map_err(|e| SourceError(format!("cannot download {url} ({e})")))?;
    if resp.status != 200 {
        return fail(format!("{url} answered HTTP {}", resp.status));
    }
    Ok(resp.body)
}

/// The pinned description, from the cache when a verified copy is there, else downloaded, verified and cached.
/// Returns the bytes and whether they were downloaded.
pub fn fetch_spec(
    transport: &mut Transport,
    pins: &Pins,
    cache_dir: &Path,
) -> Result<(Vec<u8>, bool)> {
    let cached = cache_dir.join(format!("{}.json", pins.sha256));
    if let Ok(data) = fs::read(&cached) {
        if sha256_hex(&data) == pins.sha256 {
            return Ok((data, false));
        }
    }
    let url = pins.url();
    let data = get(transport, &url, "application/octet-stream")?;
    let got = sha256_hex(&data);
    if got != pins.sha256 {
        return fail(format!(
            "{url} does not match the pinned file (SHA-256 {got}, expected {}); nothing was used",
            pins.sha256
        ));
    }
    fs::create_dir_all(cache_dir)
        .map_err(|e| SourceError(format!("{}: {e}", cache_dir.display())))?;
    write_atomic(&cached, &data).map_err(|e| SourceError(format!("{}: {e}", cached.display())))?;
    Ok((data, true))
}

pub fn parse_spec(data: &[u8]) -> Result<Value> {
    serde_json::from_slice(data)
        .map_err(|e| SourceError(format!("the description is not valid JSON ({e})")))
}

/// New pins for a commit of the description repository (maintainers): the commit's date, then the file's hash.
pub fn discover_pins(
    transport: &mut Transport,
    current: &Pins,
    commit: &str,
    api_version: &str,
) -> Result<Pins> {
    let url = format!("{API_BASE}/repos/{}/commits/{commit}", current.repository);
    let info: Value = serde_json::from_slice(&get(transport, &url, "application/vnd.github+json")?)
        .map_err(|e| SourceError(format!("{url}: not JSON ({e})")))?;
    let full = info["sha"].as_str().unwrap_or("").to_string();
    if full.len() != 40 {
        return fail(format!("{url}: no commit sha in the answer"));
    }
    let date = info["commit"]["committer"]["date"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let mut pins = Pins {
        commit: full,
        commit_date: date,
        file: format!("descriptions/api.github.com/api.github.com.{api_version}.json"),
        api_version: api_version.to_string(),
        sha256: String::new(),
        length: 0,
        ..current.clone()
    };
    let data = get(transport, &pins.url(), "application/octet-stream")?;
    let spec = parse_spec(&data)?;
    if spec["openapi"]
        .as_str()
        .is_none_or(|v| !v.starts_with("3."))
    {
        return fail(format!("{} is not an OpenAPI 3 description", pins.url()));
    }
    pins.sha256 = sha256_hex(&data);
    pins.length = data.len() as u64;
    Ok(pins)
}

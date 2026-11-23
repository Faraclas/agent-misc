//! Microsoft's Azure DevOps client libraries (Go and Node), the second input of the generator.
//!
//! The client libraries are generated from the service's own route definitions, so they describe endpoints that the
//! REST spec leaves out. Each client method names its endpoint by a location GUID (the `id` of a route in the
//! service's `OPTIONS /_apis` list), its HTTP method, api-version, route and query parameters with types and
//! descriptions, and its request body type. They hold no URL: `sdkops` joins them to the live route list for that.
//!
//! The files are downloaded at run time, never vendored: one tarball per library from GitHub's codeload service at the
//! commit pinned in `pins.json` (`sdk`), every file used verified by its git blob hash and cached by that hash.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::{json, Value};

use crate::archive::{git_blob_sha1, ArchiveError, USER_AGENT};
use crate::http::{Request, Transport};

type Result<T> = std::result::Result<T, ArchiveError>;

fn fail<T>(msg: impl Into<String>) -> Result<T> {
    Err(ArchiveError(msg.into()))
}

/// Which client library a method comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sdk {
    Go,
    Node,
}

impl Sdk {
    pub fn key(self) -> &'static str {
        match self {
            Sdk::Go => "go",
            Sdk::Node => "node",
        }
    }

    pub fn all() -> [Sdk; 2] {
        [Sdk::Go, Sdk::Node]
    }

    /// The files of the library the parsers read, by path inside the repository.
    pub fn wants(self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        match self {
            // azuredevops/v7/<package>/client.go and models.go
            Sdk::Go => {
                parts.len() == 4
                    && parts[0] == "azuredevops"
                    && parts[1] == "v7"
                    && (parts[3] == "client.go" || parts[3] == "models.go")
            }
            // api/<Name>Api.ts, api/<Name>ApiBase.ts, api/interfaces/*.ts, api/interfaces/common/*.ts
            Sdk::Node => match parts.as_slice() {
                ["api", f] => f.ends_with("Api.ts") || f.ends_with("ApiBase.ts"),
                ["api", "interfaces", f] | ["api", "interfaces", "common", f] => f.ends_with(".ts"),
                _ => false,
            },
        }
    }
}

/// Where a parameter goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Path,
    Query,
    Header,
    Body,
}

/// A type as the client library declares it.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    String,
    Integer(&'static str),
    Number,
    Boolean,
    Uuid,
    DateTime,
    Any,
    Array(Box<Ty>),
    /// A JSON object with string keys.
    Map(Box<Ty>),
    /// A type of the library, by its key in `Types` (`go:<package>.<Name>`, `node:<file>.<Name>`).
    Named(String),
    /// Raw bytes: a file upload.
    Stream,
    /// A list sent as one comma-separated value.
    CommaList(Box<Ty>),
}

#[derive(Clone, Debug)]
pub struct Param {
    /// The name on the wire (query name, route value name, header name; `body` for the body).
    pub name: String,
    pub place: Place,
    pub ty: Ty,
    pub required: bool,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Method {
    pub sdk: Sdk,
    /// Go package or Node file the method is in (`git`, `GitApi`).
    pub client: String,
    /// PascalCase (`GetConnectionData`).
    pub name: String,
    pub description: String,
    /// Lower case.
    pub guid: String,
    /// Lower case (`get`, `post`, ...).
    pub http_method: String,
    /// The api-version the library asks for (`7.1-preview.1`, `7.2-preview.3`).
    pub version: String,
    pub params: Vec<Param>,
    /// Route values the method fills with a fixed string (`action: "Branches"`).
    pub route_constants: Vec<(String, String)>,
    /// Content-Type of the body, when it has one.
    pub content_type: Option<String>,
    pub accept: String,
    /// The library returns the answer as a stream (a file or zip).
    pub binary_answer: bool,
}

/// A named type of a library.
#[derive(Clone, Debug)]
pub enum TypeDef {
    Object {
        description: String,
        /// (JSON name, type, description); inherited properties included.
        properties: Vec<(String, Ty, String)>,
    },
    Enum {
        description: String,
        /// The names the service takes (camelCase).
        values: Vec<String>,
    },
}

pub type Types = HashMap<String, TypeDef>;

/// What the parsers return for one library.
#[derive(Debug, Default)]
pub struct Parsed {
    pub methods: Vec<Method>,
    pub types: Types,
}

/// The files of one library at its pinned commit: path -> text.
pub type Files = Vec<(String, String)>;

/// `pins.json` `sdk.<go|node>`.
pub fn pin_of(pins: &Value, sdk: Sdk) -> Option<&Value> {
    pins.get("sdk").and_then(|s| s.get(sdk.key()))
}

fn tarball_url(repo: &str, commit: &str) -> String {
    format!("https://codeload.github.com/{repo}/tar.gz/{commit}")
}

/// The regular files of a gzip-compressed tar archive, as (path, bytes). The first path component (GitHub's
/// `<repo>-<commit>/`) is removed. Handles ustar prefixes, pax `path` records and GNU long names.
pub fn untar_gz(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut raw)
        .map_err(|e| ArchiveError(format!("the tarball is not valid gzip ({e})")))?;
    let field = |block: &[u8], start: usize, len: usize| -> String {
        let bytes = &block[start..start + len];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(len);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let mut out = Vec::new();
    let mut pos = 0;
    let mut long_name: Option<String> = None;
    while pos + 512 <= raw.len() {
        let header = &raw[pos..pos + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size_text = field(header, 124, 12);
        let size = usize::from_str_radix(size_text.trim(), 8)
            .map_err(|_| ArchiveError(format!("bad tar entry size '{size_text}'")))?;
        let kind = header[156];
        let start = pos + 512;
        let end = start + size;
        if end > raw.len() {
            return fail("the tarball is cut short");
        }
        let body = &raw[start..end];
        pos = start + size.div_ceil(512) * 512;
        match kind {
            // pax extended header for the next entry: "<len> path=<value>\n" records.
            b'x' => {
                let text = String::from_utf8_lossy(body);
                for record in text.split('\n') {
                    if let Some((_, kv)) = record.split_once(' ') {
                        if let Some(path) = kv.strip_prefix("path=") {
                            long_name = Some(path.to_string());
                        }
                    }
                }
            }
            b'L' => {
                long_name = Some(
                    String::from_utf8_lossy(body)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'0' | 0 => {
                let name = match long_name.take() {
                    Some(n) => n,
                    None => {
                        let prefix = field(header, 345, 155);
                        let name = field(header, 0, 100);
                        if prefix.is_empty() {
                            name
                        } else {
                            format!("{prefix}/{name}")
                        }
                    }
                };
                let inner = name.split_once('/').map(|(_, rest)| rest).unwrap_or("");
                if !inner.is_empty() {
                    out.push((inner.to_string(), body.to_vec()));
                }
            }
            _ => long_name = None,
        }
    }
    Ok(out)
}

fn download_tarball(transport: &mut Transport, repo: &str, commit: &str) -> Result<Vec<u8>> {
    let url = tarball_url(repo, commit);
    let req = Request {
        method: "GET",
        url: url.clone(),
        headers: vec![("User-Agent".into(), USER_AGENT.into())],
        body: None,
    };
    let resp = transport(&req).map_err(|e| ArchiveError(format!("cannot download {url} ({e})")))?;
    if !(200..300).contains(&resp.status) {
        return fail(format!("downloading {url} failed: HTTP {}", resp.status));
    }
    Ok(resp.body)
}

/// The pinned files of one library, from the cache or else from its tarball (one request), each verified by git
/// blob hash. Returns the files and whether the tarball was downloaded.
pub fn load_files(
    transport: &mut Transport,
    pin: &Value,
    cache_dir: &Path,
) -> Result<(Files, bool)> {
    let repo = pin["repo"].as_str().unwrap_or("");
    let commit = pin["commit"].as_str().unwrap_or("");
    let wanted: Vec<(&str, &str)> = pin["files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            (
                f["file"].as_str().unwrap_or(""),
                f["sha1_git"].as_str().unwrap_or(""),
            )
        })
        .collect();
    if repo.is_empty() || commit.is_empty() || wanted.is_empty() {
        return fail("pins.json has no usable sdk entry");
    }
    let dir = cache_dir.join("sdk");
    let cached = |sha: &str| -> Option<String> {
        let data = fs::read(dir.join(sha)).ok()?;
        (git_blob_sha1(&data) == sha).then(|| String::from_utf8_lossy(&data).into_owned())
    };
    let mut files = Vec::new();
    let mut missing = false;
    for (file, sha) in &wanted {
        match cached(sha) {
            Some(text) => files.push((file.to_string(), text)),
            None => {
                missing = true;
                break;
            }
        }
    }
    if !missing {
        return Ok((files, false));
    }
    let tar = download_tarball(transport, repo, commit)?;
    let mut by_path: HashMap<String, Vec<u8>> = untar_gz(&tar)?.into_iter().collect();
    let io_err =
        |e: std::io::Error| ArchiveError(format!("cannot write the cache {}: {e}", dir.display()));
    fs::create_dir_all(&dir).map_err(io_err)?;
    let mut files = Vec::new();
    for (file, sha) in &wanted {
        let Some(data) = by_path.remove(*file) else {
            return fail(format!("{repo}@{commit}: {file} is not in the tarball"));
        };
        let actual = git_blob_sha1(&data);
        if actual != *sha {
            return fail(format!(
                "{repo}@{commit}: {file} has hash {actual}, pinned {sha}"
            ));
        }
        let tmp = dir.join(format!("{sha}.tmp{}", std::process::id()));
        fs::write(&tmp, &data).map_err(io_err)?;
        fs::rename(&tmp, dir.join(sha)).map_err(io_err)?;
        files.push((
            file.to_string(),
            String::from_utf8_lossy(&data).into_owned(),
        ));
    }
    Ok((files, true))
}

/// The `sdk.<go|node>` pin for a commit: the tarball's files the parsers read, with their git blob hashes.
pub fn discover(transport: &mut Transport, sdk: Sdk, repo: &str, commit: &str) -> Result<Value> {
    let tar = download_tarball(transport, repo, commit)?;
    let mut files: Vec<Value> = untar_gz(&tar)?
        .into_iter()
        .filter(|(path, _)| sdk.wants(path))
        .map(|(path, data)| json!({"file": path, "sha1_git": git_blob_sha1(&data), "length": data.len()}))
        .collect();
    files.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    if files.is_empty() {
        return fail(format!(
            "{repo}@{commit}: no {} client files found",
            sdk.key()
        ));
    }
    Ok(json!({"repo": repo, "commit": commit, "files": files}))
}

/// The default repositories, and the branch `pins-sdk` follows when no commit is given.
pub fn default_repo(sdk: Sdk) -> (&'static str, &'static str) {
    match sdk {
        Sdk::Go => ("microsoft/azure-devops-go-api", "dev"),
        Sdk::Node => ("microsoft/azure-devops-node-api", "master"),
    }
}

/// The commit a branch points at, from GitHub's API (one request, no token needed).
pub fn branch_commit(transport: &mut Transport, repo: &str, branch: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/commits/{branch}");
    let req = Request {
        method: "GET",
        url: url.clone(),
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Accept".into(), "application/vnd.github.sha".into()),
        ],
        body: None,
    };
    let resp = transport(&req).map_err(|e| ArchiveError(format!("cannot reach {url} ({e})")))?;
    let sha = String::from_utf8_lossy(&resp.body).trim().to_string();
    if !(200..300).contains(&resp.status) || sha.len() != 40 {
        return fail(format!("{url}: HTTP {}", resp.status));
    }
    Ok(sha)
}

/// Line comments (`// ...`) directly above line `index`, joined, as Go and TypeScript doc comments are written.
pub fn comment_above(lines: &[&str], index: usize) -> String {
    let mut parts = Vec::new();
    let mut i = index;
    while i > 0 {
        let line = lines[i - 1].trim();
        match line.strip_prefix("//") {
            Some(text) => parts.push(text.trim().to_string()),
            None => break,
        }
        i -= 1;
    }
    parts.reverse();
    parts.join(" ")
}

/// Text inside the brackets that open at `open` (which must be `(`, `{` or `[`), skipping strings and comments.
pub fn balanced(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let (o, c) = match bytes.get(open)? {
        b'(' => (b'(', b')'),
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => return None,
    };
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 1;
            } else if b == q {
                quote = None;
            }
        } else if bytes[i..].starts_with(b"//") {
            // A comment's apostrophes are not quotes.
            i += bytes[i..]
                .iter()
                .position(|&x| x == b'\n')
                .unwrap_or(bytes.len() - i);
            continue;
        } else if bytes[i..].starts_with(b"/*") {
            i += bytes[i..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len() - i, |e| e + 2);
            continue;
        } else if b == b'"' || b == b'\'' || b == b'`' {
            quote = Some(b);
        } else if b == o {
            depth += 1;
        } else if b == c {
            depth -= 1;
            if depth == 0 {
                return Some(&text[open + 1..i]);
            }
        }
        i += 1;
    }
    None
}

/// Split at top-level commas (outside brackets and strings).
pub fn split_top(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut escape = false;
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '(' | '{' | '[' | '<' => depth += 1,
            ')' | '}' | ']' | '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// `PascalCase` -> `camelCase`.
pub fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `camelCase` -> `PascalCase`.
pub fn upper_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

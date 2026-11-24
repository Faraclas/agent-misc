//! Files that tools upload and download, and answers too large to print.
//!
//! Who touches files: the `utcp` process the agent runs. It reads the files a tool uploads and
//! writes the files a tool downloads, with the agent's own permissions, so an agent's sandbox stays
//! the only boundary. The daemon never opens a user's file: over IPC it asks the caller for an
//! upload's bytes and hands a download's bytes back (see `ipc.rs`). In direct mode the same code
//! runs in one process through [`LocalFiles`] and [`DownloadSink`].
//!
//! Limits (`max_upload_mb`, `max_download_mb` in `settings.toml`): absent or commented out means no
//! limit, `0` turns the direction off, `N` is the largest size in MB. Every refusal says it failed
//! only because of the limit, and how to change it.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use crate::paths::{UtcpPaths, UtcpSettings};

/// A JSON or text answer larger than this (after `--fields`) is saved to a file instead of printed.
pub const LARGE_ANSWER_BYTES: usize = 10 * 1024 * 1024;
/// How long files stay in utcp's own downloads folder (`<cache>/downloads`).
pub const CACHE_DOWNLOADS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MB: u64 = 1024 * 1024;

/// The bytes of an upload or download, in chunks.
pub type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Set once this process's stdout turns out to be closed (`utcp ... | head`): later output is dropped,
/// and the command ends quietly. SIGPIPE stays ignored, so a closed socket or MCP server pipe is still an
/// ordinary error rather than killing the process.
static STDOUT_CLOSED: AtomicBool = AtomicBool::new(false);

/// Whether a write to stdout found it closed (a broken pipe).
pub fn stdout_closed() -> bool {
    STDOUT_CLOSED.load(Ordering::Relaxed)
}

/// The error of a failed write to stdout. A broken pipe marks stdout closed (see [`stdout_closed`]).
pub fn stdout_write_error(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        STDOUT_CLOSED.store(true, Ordering::Relaxed);
        anyhow!("stdout was closed before the output ended")
    } else {
        anyhow::Error::new(e).context("cannot write to stdout")
    }
}

/// Why a file answer exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// The tool answered with a file (an archive, an image, a log...).
    Download,
    /// A JSON or text answer too large to print ([`LARGE_ANSWER_BYTES`]).
    LargeAnswer,
}

/// What is known about a file answer before its bytes arrive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub kind: FileKind,
    pub mime_type: String,
    /// The name the server suggested (`Content-Disposition`), already made safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// The size, when the server said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The tool's name, for a file name when the server suggests none.
    pub tool: String,
}

/// A file answer: its metadata and its bytes, not yet read.
pub struct FileDownload {
    pub meta: FileMeta,
    pub stream: ByteStream,
}

impl std::fmt::Debug for FileDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDownload")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

/// What a tool call produced.
#[derive(Debug)]
pub enum CallOutput {
    Json(Value),
    /// A JSON answer that is one page of a longer list. `next_page` holds the arguments that,
    /// added to the call's own, fetch the next page.
    Page {
        value: Value,
        next_page: Value,
    },
    File(FileDownload),
}

/// A page of a longer list as `utcp call` prints it: the answer under `result`, the arguments for
/// the next page under `next_page`.
pub fn page_value(result: Value, next_page: Value) -> Value {
    serde_json::json!({ "result": result, "next_page": next_page })
}

/// The answer and next-page arguments of a value built by [`page_value`]; any other value back as `Err`.
pub fn split_page(value: Value) -> std::result::Result<(Value, Value), Value> {
    let is_page = value.as_object().is_some_and(|map| {
        map.len() == 2
            && map.contains_key("result")
            && map.get("next_page").is_some_and(Value::is_object)
    });
    if !is_page {
        return Err(value);
    }
    let Value::Object(mut map) = value else {
        unreachable!()
    };
    let result = map.remove("result").unwrap_or(Value::Null);
    let next_page = map.remove("next_page").unwrap_or(Value::Null);
    Ok((result, next_page))
}

impl FileDownload {
    /// A file answer whose bytes are already in memory.
    pub fn from_bytes(meta: FileMeta, data: Vec<u8>) -> Self {
        let chunks: Vec<std::io::Result<Bytes>> = Bytes::from(data)
            .chunks(256 * 1024)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Self {
            meta,
            stream: Box::pin(futures::stream::iter(chunks)),
        }
    }
}

/// The file a tool uploads, as the tool's arguments name it: `{"file": "<path>"}`, optionally with
/// `offset` and `length` to send one part (a chunked upload), or just the path as a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSpec {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

impl UploadSpec {
    /// Read the spec from a tool argument; `field` names the argument in the error.
    pub fn from_value(value: &Value, field: &str) -> Result<Self> {
        let wrong = || {
            anyhow!(
                "`{field}` is the file to upload: {{\"file\": \"<path>\"}} (optionally with \"offset\" and \
                 \"length\" in bytes to send one part of it)"
            )
        };
        match value {
            Value::String(path) if !path.is_empty() => Ok(Self {
                file: path.clone(),
                offset: None,
                length: None,
            }),
            Value::Object(_) => {
                let spec: Self = serde_json::from_value(value.clone()).map_err(|_| wrong())?;
                if spec.file.is_empty() {
                    return Err(wrong());
                }
                Ok(spec)
            }
            _ => Err(wrong()),
        }
    }
}

/// An upload's bytes and how many there are.
pub struct Upload {
    pub length: u64,
    pub stream: ByteStream,
}

/// Opens the files a tool uploads. Direct mode reads them itself ([`LocalFiles`]); the daemon asks
/// the `utcp` process that made the call.
#[async_trait]
pub trait CallerFiles: Send + Sync {
    async fn open_upload(&self, spec: &UploadSpec) -> Result<Upload>;
}

/// For callers that cannot provide files (the library API, Code Mode).
pub struct NoFiles;

#[async_trait]
impl CallerFiles for NoFiles {
    async fn open_upload(&self, spec: &UploadSpec) -> Result<Upload> {
        Err(anyhow!(
            "cannot upload {}: this tool uploads a file, which only `utcp call` and `utcp batch` can do",
            spec.file
        ))
    }
}

/// Direct mode: this process reads the file.
pub struct LocalFiles {
    pub limits: Limits,
}

#[async_trait]
impl CallerFiles for LocalFiles {
    async fn open_upload(&self, spec: &UploadSpec) -> Result<Upload> {
        let (file, length) = prepare_upload(spec, &self.limits)?;
        Ok(Upload {
            length,
            stream: read_stream(file, length),
        })
    }
}

/// The next `length` bytes of `file`, in chunks, read off the async threads.
pub fn read_stream(file: File, length: u64) -> ByteStream {
    let state = (Some(file), length);
    Box::pin(futures::stream::unfold(state, |(file, left)| async move {
        let mut file = file?;
        if left == 0 {
            return None;
        }
        let want = left.min(256 * 1024) as usize;
        let read = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            Ok::<_, std::io::Error>((file, buf))
        })
        .await;
        match read {
            Ok(Ok((_, buf))) if buf.is_empty() => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the file got shorter while it was being uploaded",
                )),
                (None, 0),
            )),
            Ok(Ok((file, buf))) => {
                let left = left - buf.len() as u64;
                Some((Ok(Bytes::from(buf)), (Some(file), left)))
            }
            Ok(Err(e)) => Some((Err(e), (None, 0))),
            Err(e) => Some((Err(std::io::Error::other(e)), (None, 0))),
        }
    }))
}

/// Open the file an upload names, positioned at its offset, and the number of bytes to send.
/// Checks the upload limit. A relative path is relative to this process's working folder.
pub fn prepare_upload(spec: &UploadSpec, limits: &Limits) -> Result<(File, u64)> {
    let path = crate::secrets::expand_home(&spec.file);
    let mut file =
        File::open(&path).with_context(|| format!("cannot upload {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot upload {}", path.display()))?;
    if !meta.is_file() {
        return Err(anyhow!(
            "cannot upload {}: it is not a file",
            path.display()
        ));
    }
    let size = meta.len();
    let offset = spec.offset.unwrap_or(0);
    if offset > size {
        return Err(anyhow!(
            "cannot upload {}: offset {offset} is past its end ({size} bytes)",
            path.display()
        ));
    }
    let length = spec.length.unwrap_or(size - offset);
    if offset + length > size {
        return Err(anyhow!(
            "cannot upload {}: offset {offset} + length {length} is past its end ({size} bytes)",
            path.display()
        ));
    }
    limits.check_upload(&path, length)?;
    file.seek(SeekFrom::Start(offset))
        .with_context(|| format!("cannot upload {}", path.display()))?;
    Ok((file, length))
}

/// The upload and download limits from `settings.toml`, and where they come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_upload_mb: Option<u64>,
    pub max_download_mb: Option<u64>,
    pub settings_file: PathBuf,
}

impl Limits {
    pub fn load(paths: &UtcpPaths) -> Self {
        let settings = UtcpSettings::load(paths);
        Self {
            max_upload_mb: settings.max_upload_mb,
            max_download_mb: settings.max_download_mb,
            settings_file: paths.settings_file.clone(),
        }
    }

    /// No limits (tests).
    pub fn none() -> Self {
        Self {
            max_upload_mb: None,
            max_download_mb: None,
            settings_file: PathBuf::from("settings.toml"),
        }
    }

    pub fn check_upload(&self, path: &Path, length: u64) -> Result<()> {
        let file = self.settings_file.display();
        match self.max_upload_mb {
            None => Ok(()),
            Some(0) => Err(anyhow!(
                "upload refused: uploads are turned off (max_upload_mb = 0 in {file}), so {} was not sent. \
                 To allow uploads, set max_upload_mb to a size in MB there, or comment the line out for no limit.",
                path.display()
            )),
            Some(mb) if length > mb * MB => Err(anyhow!(
                "upload refused: {} is {} and the upload limit is {mb} MB, so it was not sent. It failed only \
                 because of this limit. To allow it, raise max_upload_mb in {file}, or comment that line out \
                 for no limit.",
                path.display(),
                human_size(length)
            )),
            Some(_) => Ok(()),
        }
    }

    /// Check a download before its bytes arrive (`size` when the server said it).
    pub fn check_download_start(&self, size: Option<u64>) -> Result<()> {
        let file = self.settings_file.display();
        match (self.max_download_mb, size) {
            (Some(0), _) => Err(anyhow!(
                "download refused: downloads are turned off (max_download_mb = 0 in {file}), so nothing was \
                 saved. To allow downloads, set max_download_mb to a size in MB there, or comment the line \
                 out for no limit."
            )),
            (Some(mb), Some(size)) if size > mb * MB => Err(self.download_too_big(mb, size)),
            _ => Ok(()),
        }
    }

    /// Check a download's size so far.
    pub fn check_download_progress(&self, so_far: u64) -> Result<()> {
        match self.max_download_mb {
            Some(mb) if so_far > mb * MB => Err(self.download_too_big(mb, so_far)),
            _ => Ok(()),
        }
    }

    fn download_too_big(&self, mb: u64, size: u64) -> anyhow::Error {
        anyhow!(
            "download stopped: the file is over the download limit of {mb} MB (at least {}), so nothing was \
             kept. It failed only because of this limit. To allow it, raise max_download_mb in {}, or comment \
             that line out for no limit.",
            human_size(size),
            self.settings_file.display()
        )
    }
}

/// `1.5 MB`, `230 KB`, `12 bytes`.
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < MB {
        format!("{:.1} KB", b / 1024.0)
    } else if bytes < 1024 * MB {
        format!("{:.1} MB", b / MB as f64)
    } else {
        format!("{:.2} GB", b / (1024 * MB) as f64)
    }
}

/// Where downloads go when no `--out` names a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadFolderSource {
    /// `download_dir` in `settings.toml`.
    Setting,
    /// The user's Downloads folder (Windows Known Folder, Linux `user-dirs.dirs`).
    UserDownloads,
    /// No Downloads folder: utcp's own, cleaned after a day.
    Cache,
}

/// The folder downloads go to, and why.
pub fn download_folder(paths: &UtcpPaths) -> (PathBuf, DownloadFolderSource) {
    let settings = UtcpSettings::load(paths);
    if let Some(dir) = settings.download_dir.as_deref().map(str::trim) {
        if !dir.is_empty() {
            return (
                crate::secrets::expand_home(dir),
                DownloadFolderSource::Setting,
            );
        }
    }
    if let Some(dir) =
        directories::UserDirs::new().and_then(|u| u.download_dir().map(Path::to_path_buf))
    {
        if dir.is_dir() {
            return (dir, DownloadFolderSource::UserDownloads);
        }
    }
    (cache_downloads_dir(paths), DownloadFolderSource::Cache)
}

/// utcp's own downloads folder, for answers too large to print and when there is no Downloads folder.
pub fn cache_downloads_dir(paths: &UtcpPaths) -> PathBuf {
    paths.cache_dir.join("downloads")
}

/// Delete files older than [`CACHE_DOWNLOADS_TTL`] from utcp's own downloads folder.
pub fn prune_cache_downloads(paths: &UtcpPaths) {
    let Ok(entries) = fs::read_dir(cache_downloads_dir(paths)) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > CACHE_DOWNLOADS_TTL);
        if meta.is_file() && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Where a caller wants one call's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutTarget {
    /// This path (overwritten, like `curl -o` and `cp`), or inside it when it is a folder.
    Path(PathBuf),
    /// Standard output (`--out -`).
    Stdout,
}

impl OutTarget {
    /// `-` is stdout; anything else a path, relative to the working folder.
    pub fn parse(s: &str) -> Self {
        if s == "-" {
            Self::Stdout
        } else {
            Self::Path(PathBuf::from(s))
        }
    }
}

/// Make a server-suggested file name safe: its last path part, no control or reserved characters.
pub fn sanitize_filename(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() {
        return None;
    }
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let mut cleaned = if reserved {
        format!("_{cleaned}")
    } else {
        cleaned
    };
    if cleaned.len() > 200 {
        let mut cut = 200;
        while !cleaned.is_char_boundary(cut) {
            cut -= 1;
        }
        cleaned.truncate(cut);
    }
    Some(cleaned)
}

/// The file name in a `Content-Disposition` header (`filename*=UTF-8''...` wins over `filename=`).
pub fn content_disposition_filename(header: &str) -> Option<String> {
    let mut plain = None;
    for part in header.split(';').map(str::trim) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let encoded = value.trim().splitn(3, '\'').nth(2).unwrap_or("");
                if let Ok(decoded) = urlencoding::decode(encoded) {
                    if let Some(name) = sanitize_filename(&decoded) {
                        return Some(name);
                    }
                }
            }
            "filename" => {
                plain = sanitize_filename(value.trim().trim_matches('"'));
            }
            _ => {}
        }
    }
    plain
}

/// The type and subtype of a `Content-Type` value, lowercase and without parameters such as
/// `charset` or Azure DevOps' `api-version`; `None` when nothing is left.
pub fn mime_essence(content_type: &str) -> Option<String> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    (!essence.is_empty()).then_some(essence)
}

/// A file extension for a MIME type.
pub fn extension_for(mime_type: &str) -> &'static str {
    let essence = mime_essence(mime_type).unwrap_or_default();
    match essence.as_str() {
        "application/json" => "json",
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/pdf" => "pdf",
        "application/xml" | "text/xml" => "xml",
        "application/yaml" | "application/x-yaml" | "text/yaml" => "yaml",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/webp" => "webp",
        _ if essence.ends_with("+json") => "json",
        _ if essence.ends_with("+xml") => "xml",
        _ => "bin",
    }
}

fn default_name(meta: &FileMeta) -> String {
    if let Some(name) = &meta.filename {
        return name.clone();
    }
    let tool = meta.tool.rsplit('.').next().unwrap_or("download");
    let tool = sanitize_filename(tool).unwrap_or_else(|| "download".into());
    format!("{tool}.{}", extension_for(&meta.mime_type))
}

/// `name`, else `name (1).ext`, `name (2).ext`... whichever is free in `dir`.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let as_path = Path::new(name);
    let stem = as_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = as_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} ({}){ext}", std::process::id()))
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

enum SinkTarget {
    File {
        part: PathBuf,
        file: File,
        /// The final path; with `overwrite == false`, the folder and wanted name, made unique at the end.
        dir: PathBuf,
        name: String,
        overwrite: bool,
    },
    Stdout,
}

/// Writes one file answer to its place: a `.part` file renamed when it is complete (deleted if it
/// is not), or stdout. Checks the download limit as bytes arrive.
pub struct DownloadSink {
    target: SinkTarget,
    meta: FileMeta,
    limits: Limits,
    written: u64,
    finished: bool,
    /// For a large answer, the folder whose files utcp deletes after a day.
    cache_dir: Option<PathBuf>,
}

impl DownloadSink {
    pub fn create(
        meta: FileMeta,
        out: Option<&OutTarget>,
        paths: &UtcpPaths,
        limits: &Limits,
    ) -> Result<Self> {
        if meta.kind == FileKind::Download {
            limits.check_download_start(meta.size)?;
        }
        let mut cache_dir = None;
        let (dir, name, overwrite) = match out {
            Some(OutTarget::Stdout) => {
                return Ok(Self {
                    target: SinkTarget::Stdout,
                    meta,
                    limits: limits.clone(),
                    written: 0,
                    finished: false,
                    cache_dir: None,
                })
            }
            Some(OutTarget::Path(p)) if p.is_dir() => (p.clone(), default_name(&meta), true),
            Some(OutTarget::Path(p)) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| anyhow!("--out {}: not a file path", p.display()))?;
                let dir = p
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                (dir, name, true)
            }
            None => {
                let (dir, source) = match meta.kind {
                    FileKind::LargeAnswer => {
                        (cache_downloads_dir(paths), DownloadFolderSource::Cache)
                    }
                    FileKind::Download => download_folder(paths),
                };
                if source == DownloadFolderSource::Cache {
                    prune_cache_downloads(paths);
                    cache_dir = Some(dir.clone());
                }
                (dir, default_name(&meta), false)
            }
        };
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        #[cfg(unix)]
        if cache_dir.is_some() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let wanted = if overwrite {
            dir.join(&name)
        } else {
            free_name(&dir, &name)
        };
        let part = part_path(&wanted);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part)
            .with_context(|| format!("cannot save the file to {}", part.display()))?;
        Ok(Self {
            target: SinkTarget::File {
                part,
                file,
                dir,
                name,
                overwrite,
            },
            meta,
            limits: limits.clone(),
            written: 0,
            finished: false,
            cache_dir,
        })
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.written += chunk.len() as u64;
        if self.meta.kind == FileKind::Download {
            self.limits.check_download_progress(self.written)?;
        }
        match &mut self.target {
            SinkTarget::File { file, part, .. } => file
                .write_all(chunk)
                .with_context(|| format!("cannot write {}", part.display())),
            SinkTarget::Stdout => std::io::stdout()
                .lock()
                .write_all(chunk)
                .map_err(stdout_write_error),
        }
    }

    /// Move the complete file into place and return the reference the caller prints.
    pub fn finish(mut self) -> Result<Value> {
        self.finished = true;
        let path = match &mut self.target {
            SinkTarget::Stdout => {
                std::io::stdout().flush().map_err(stdout_write_error)?;
                "-".to_string()
            }
            SinkTarget::File {
                part,
                file,
                dir,
                name,
                overwrite,
            } => {
                let finished = file.flush().and_then(|()| file.sync_all());
                let target = if *overwrite {
                    dir.join(&*name)
                } else {
                    free_name(dir, name)
                };
                let moved = finished.and_then(|()| fs::rename(&*part, &target));
                if let Err(e) = moved {
                    let _ = fs::remove_file(&*part);
                    return Err(e).with_context(|| format!("cannot save {}", target.display()));
                }
                let full = std::path::absolute(&target).unwrap_or(target);
                full.to_string_lossy().into_owned()
            }
        };
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| default_name(&self.meta));
        let mut reference = json!({
            "type": "file",
            "path": path,
            "mime_type": self.meta.mime_type,
            "bytes": self.written,
            "filename": name,
        });
        if self.meta.kind == FileKind::LargeAnswer {
            let mut note = format!(
                "The answer is {}, too large to print, so utcp saved it to this file. Read parts of it \
                 (for example with jq), or call again with --fields to get less.",
                human_size(self.written)
            );
            if let Some(dir) = &self.cache_dir {
                note += &format!(" utcp deletes files in {} after a day.", dir.display());
            }
            reference["note"] = json!(note);
        }
        Ok(reference)
    }
}

impl Drop for DownloadSink {
    fn drop(&mut self) {
        if !self.finished {
            if let SinkTarget::File { part, .. } = &self.target {
                let _ = fs::remove_file(part);
            }
        }
    }
}

/// `value` serialized (pretty), when it is too large to print ([`LARGE_ANSWER_BYTES`] as compact JSON).
pub fn oversized_json(value: &Value) -> Option<Vec<u8>> {
    let compact = serde_json::to_vec(value).ok()?;
    if compact.len() <= LARGE_ANSWER_BYTES {
        return None;
    }
    Some(serde_json::to_vec_pretty(value).unwrap_or(compact))
}

/// Metadata for a JSON answer saved as a file.
pub fn json_file_meta(kind: FileKind, tool: &str, size: usize) -> FileMeta {
    FileMeta {
        kind,
        mime_type: "application/json".into(),
        filename: None,
        size: Some(size as u64),
        tool: tool.to_string(),
    }
}

/// Write an in-memory answer through a sink (`--out` for a JSON answer, or a large answer).
pub fn save_bytes(
    meta: FileMeta,
    data: &[u8],
    out: Option<&OutTarget>,
    paths: &UtcpPaths,
    limits: &Limits,
) -> Result<Value> {
    let mut sink = DownloadSink::create(meta, out, paths, limits)?;
    sink.write(data)?;
    sink.finish()
}

/// Direct mode: turn a call's output into what the caller prints. A file answer is streamed to its
/// place; a JSON answer gets `fields`, then goes to `out` when given, or to a file when it is too
/// large to print.
pub async fn finish_output_locally(
    output: CallOutput,
    tool: &str,
    fields: Option<&[String]>,
    out: Option<&OutTarget>,
    paths: &UtcpPaths,
    limits: &Limits,
) -> Result<Value> {
    use futures::StreamExt;
    match output {
        CallOutput::File(FileDownload { meta, mut stream }) => {
            let mut sink = DownloadSink::create(meta, out, paths, limits)?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("the download broke off")?;
                sink.write(&chunk)?;
            }
            sink.finish()
        }
        CallOutput::Json(value) => {
            let value = match fields {
                Some(f) if !f.is_empty() => crate::catalog::project_json_fields(value, f),
                _ => value,
            };
            json_to_place(value, out, tool, paths, limits)
        }
        CallOutput::Page { value, next_page } => {
            let value = match fields {
                Some(f) if !f.is_empty() => crate::catalog::project_json_fields(value, f),
                _ => value,
            };
            let result = json_to_place(value, out, tool, paths, limits)?;
            Ok(page_value(result, next_page))
        }
    }
}

/// A JSON answer: to `out` when it names a file, to a file when it is too large to print, else as is.
pub fn json_to_place(
    value: Value,
    out: Option<&OutTarget>,
    tool: &str,
    paths: &UtcpPaths,
    limits: &Limits,
) -> Result<Value> {
    match out {
        Some(target @ OutTarget::Path(_)) => {
            let data = serde_json::to_vec_pretty(&value)?;
            let meta = json_file_meta(FileKind::Download, tool, data.len());
            // A JSON answer is never subject to the download limit.
            save_bytes(meta, &data, Some(target), paths, &Limits::none())
        }
        _ => match oversized_json(&value) {
            Some(data) => {
                let meta = json_file_meta(FileKind::LargeAnswer, tool, data.len());
                save_bytes(meta, &data, None, paths, limits)
            }
            None => Ok(value),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_support::paths_in;

    fn meta(kind: FileKind, size: Option<u64>) -> FileMeta {
        FileMeta {
            kind,
            mime_type: "application/zip".into(),
            filename: Some("build.zip".into()),
            size,
            tool: "pipelines.get_artifact".into(),
        }
    }

    fn limits(up: Option<u64>, down: Option<u64>) -> Limits {
        Limits {
            max_upload_mb: up,
            max_download_mb: down,
            settings_file: PathBuf::from("/cfg/settings.toml"),
        }
    }

    #[test]
    fn pages_split_back_into_answer_and_next_page_arguments() {
        use serde_json::json;
        let page = page_value(json!([1, 2]), json!({"page": "2"}));
        assert_eq!(page, json!({"result": [1, 2], "next_page": {"page": "2"}}));
        assert_eq!(
            split_page(page).unwrap(),
            (json!([1, 2]), json!({"page": "2"}))
        );
        // Anything else is an ordinary answer, returned unchanged.
        for other in [
            json!({"result": 1}),
            json!({"result": 1, "next_page": "2"}),
            json!({"result": 1, "next_page": null}),
            json!({"result": 1, "next_page": {}, "count": 3}),
            json!({"value": 1, "next_page": {}}),
            json!([{"result": 1, "next_page": {}}]),
            json!("result"),
        ] {
            assert_eq!(split_page(other.clone()), Err(other));
        }
    }

    #[test]
    fn mime_types_lose_their_parameters() {
        assert_eq!(
            mime_essence("application/zip; api-version=7.1").as_deref(),
            Some("application/zip")
        );
        assert_eq!(
            mime_essence(" Text/Plain ;charset=utf-8").as_deref(),
            Some("text/plain")
        );
        assert_eq!(mime_essence("; charset=utf-8"), None);
        assert_eq!(extension_for("application/zip; api-version=7.1"), "zip");
    }

    #[test]
    fn names_are_safe_and_never_overwrite_in_the_default_folder() {
        assert_eq!(
            sanitize_filename("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(
            sanitize_filename("a\\b\\c:d.txt").as_deref(),
            Some("cd.txt")
        );
        assert_eq!(sanitize_filename("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(sanitize_filename(" .. "), None);
        assert_eq!(
            content_disposition_filename("attachment; filename=\"a b.zip\"").as_deref(),
            Some("a b.zip")
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename=\"x.zip\"; filename*=UTF-8''r%C3%A9sum%C3%A9.zip"
            )
            .as_deref(),
            Some("résumé.zip")
        );

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("dl");
        let paths = paths_in(tmp.path());
        let out = OutTarget::Path(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        // Into a folder: the server's name, overwritten like cp.
        for _ in 0..2 {
            save_bytes(
                meta(FileKind::Download, None),
                b"zip",
                Some(&out),
                &paths,
                &Limits::none(),
            )
            .unwrap();
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        // The default folder (here set by download_dir) never overwrites.
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            &paths.settings_file,
            format!(
                "download_dir = {}\n",
                toml::Value::String(dir.to_string_lossy().into())
            ),
        )
        .unwrap();
        let r = save_bytes(
            meta(FileKind::Download, None),
            b"zip2",
            None,
            &paths,
            &Limits::none(),
        )
        .unwrap();
        assert!(
            r["path"].as_str().unwrap().ends_with("build (1).zip"),
            "{r}"
        );
        assert_eq!(r["bytes"], 4);
        assert_eq!(r["type"], "file");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.ends_with(".part")), "{names:?}");
    }

    #[test]
    fn limits_refuse_with_the_setting_and_how_to_change_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("big.bin");
        fs::write(&file, vec![0u8; 2 * MB as usize]).unwrap();
        let spec = UploadSpec {
            file: file.to_string_lossy().into(),
            offset: None,
            length: None,
        };
        let err = prepare_upload(&spec, &limits(Some(1), None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("only because of this limit"), "{err}");
        assert!(err.contains("max_upload_mb in /cfg/settings.toml"), "{err}");
        let err = prepare_upload(&spec, &limits(Some(0), None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("uploads are turned off"), "{err}");
        let (_, len) = prepare_upload(&spec, &limits(Some(2), None)).unwrap();
        assert_eq!(len, 2 * MB);
        let part = UploadSpec {
            offset: Some(MB),
            length: Some(10),
            ..spec.clone()
        };
        let (mut f, len) = prepare_upload(&part, &Limits::none()).unwrap();
        assert_eq!(len, 10);
        assert_eq!(f.stream_position().unwrap(), MB);
        let past = UploadSpec {
            offset: Some(2 * MB),
            length: Some(1),
            ..spec
        };
        assert!(prepare_upload(&past, &Limits::none()).is_err());

        let paths = paths_in(tmp.path());
        let err = DownloadSink::create(
            meta(FileKind::Download, Some(3 * MB)),
            None,
            &paths,
            &limits(None, Some(1)),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("max_download_mb"), "{err}");
        let out = OutTarget::Path(tmp.path().join("x.zip"));
        let mut sink = DownloadSink::create(
            meta(FileKind::Download, None),
            Some(&out),
            &paths,
            &limits(None, Some(1)),
        )
        .unwrap();
        sink.write(&vec![0u8; MB as usize]).unwrap();
        assert!(sink.write(&[0u8]).is_err());
        drop(sink);
        assert!(
            !tmp.path().join("x.zip.part").exists(),
            "a stopped download leaves nothing"
        );
        assert!(!tmp.path().join("x.zip").exists());
        let err = DownloadSink::create(
            meta(FileKind::Download, Some(0)),
            Some(&out),
            &paths,
            &limits(None, Some(0)),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("downloads are turned off"), "{err}");
        // A large JSON answer is never refused by the download limit.
        save_bytes(
            json_file_meta(FileKind::LargeAnswer, "t", 2),
            b"{}",
            Some(&out),
            &paths,
            &limits(None, Some(0)),
        )
        .unwrap();
    }

    #[test]
    fn large_answers_go_to_the_cache_folder_with_a_note() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let small = json!({"a": 1});
        assert_eq!(
            json_to_place(small.clone(), None, "m.t", &paths, &Limits::none()).unwrap(),
            small
        );
        let big = json!({"blob": "x".repeat(LARGE_ANSWER_BYTES + 1)});
        let r = json_to_place(big, None, "m.get_tags", &paths, &Limits::none()).unwrap();
        let path = PathBuf::from(r["path"].as_str().unwrap());
        assert!(path.starts_with(cache_downloads_dir(&paths)), "{r}");
        assert!(path.ends_with("get_tags.json"));
        assert!(r["note"].as_str().unwrap().contains("after a day"));
        let back: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["blob"].as_str().unwrap().len(), LARGE_ANSWER_BYTES + 1);

        // --out takes a JSON answer of any size, and overwrites.
        let out = OutTarget::Path(tmp.path().join("answer.json"));
        fs::write(tmp.path().join("answer.json"), "old").unwrap();
        let r = json_to_place(small.clone(), Some(&out), "m.t", &paths, &Limits::none()).unwrap();
        assert_eq!(r["mime_type"], "application/json");
        let back: Value =
            serde_json::from_slice(&fs::read(tmp.path().join("answer.json")).unwrap()).unwrap();
        assert_eq!(back, small);
    }

    #[test]
    fn old_files_in_the_cache_folder_are_pruned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = paths_in(tmp.path());
        let dir = cache_downloads_dir(&paths);
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.json");
        fs::write(&old, "{}").unwrap();
        let two_days = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(two_days)
            .unwrap();
        fs::write(dir.join("new.json"), "{}").unwrap();
        prune_cache_downloads(&paths);
        assert!(!old.exists());
        assert!(dir.join("new.json").exists());
    }

    #[test]
    fn upload_specs_take_a_path_or_an_object() {
        assert_eq!(
            UploadSpec::from_value(&json!("a.txt"), "body")
                .unwrap()
                .file,
            "a.txt"
        );
        let s = UploadSpec::from_value(&json!({"file": "a", "offset": 4, "length": 2}), "body")
            .unwrap();
        assert_eq!((s.offset, s.length), (Some(4), Some(2)));
        let err = UploadSpec::from_value(&json!({"name": "a"}), "body")
            .unwrap_err()
            .to_string();
        assert!(err.contains("{\"file\": \"<path>\"}"), "{err}");
    }
}

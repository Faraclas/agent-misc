//! File transfer on one IPC connection, between the `utcp` process that makes a call and the daemon.
//!
//! Framing: every frame starts with a 4-byte little-endian length. A JSON frame (the request, the
//! final [`IpcResponse`], a [`StreamFrame`]) has the top bit clear. A data frame has it set; its
//! payload is a 4-byte little-endian stream id and then the bytes. A data frame with no bytes ends
//! that stream.
//!
//! - Upload: the daemon sends [`StreamFrame::SendFile`]; the caller opens the file (checking the upload
//!   limit), answers `UploadReady` (or `UploadFailed`) and sends the bytes as data frames.
//! - Download: the daemon sends [`StreamFrame::File`] and the bytes. The caller writes them to a `.part`
//!   file (checking the download limit), or answers `Cancel` to stop them. In the final answer, the
//!   call's result is `{"utcp_file_stream": id}`, which the caller replaces with the saved file's
//!   reference.
//!
//! The caller stays single-threaded and does one thing at a time (on Windows a synchronous pipe handle
//! cannot read and write at once); the daemon reads and writes in separate tasks and always drains
//! what the caller sends, so neither side waits on the other forever.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::ipc::{IpcPayload, IpcResponse};
use crate::paths::UtcpPaths;
use crate::transfer::{
    prepare_upload, CallerFiles, DownloadSink, FileDownload, FileMeta, Limits, OutTarget, Upload,
    UploadSpec,
};

/// Top bit of a frame's length: a data frame.
pub(crate) const DATA_FLAG: u32 = 0x8000_0000;
/// Largest data frame payload.
const MAX_DATA_CHUNK: usize = 1024 * 1024;
/// Largest JSON frame the caller accepts from the daemon (a batch of large answers).
pub(crate) const MAX_IPC_RESPONSE_BYTES: usize = 1024 * 1024 * 1024;
/// In a call's result: the file the daemon streamed under this id.
pub const FILE_STREAM_KEY: &str = "utcp_file_stream";

/// The JSON frames of a transfer, other than the request and the final [`IpcResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum StreamFrame {
    /// Daemon to caller: send this file's bytes.
    SendFile {
        id: u32,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<u64>,
    },
    /// Daemon to caller: a file answer follows as data frames. `item` is the batch item's index.
    File {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<usize>,
        meta: FileMeta,
    },
    /// Caller to daemon: `size` bytes follow.
    UploadReady { id: u32, size: u64 },
    /// Caller to daemon: the file cannot be sent.
    UploadFailed { id: u32, message: String },
    /// Caller to daemon: stop sending this download.
    Cancel { id: u32, message: String },
}

// ---------------------------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------------------------

pub(crate) enum OutFrame {
    Json(Vec<u8>),
    Data(u32, Bytes),
    End(u32),
}

struct UploadSlot {
    ready: Option<oneshot::Sender<std::result::Result<u64, String>>>,
    data: Option<mpsc::Sender<Bytes>>,
}

#[derive(Default)]
struct ConnShared {
    uploads: HashMap<u32, UploadSlot>,
    cancelled: HashMap<u32, String>,
    closed: bool,
}

/// The daemon's end of one call connection: asks the caller for uploads, streams downloads to it.
pub(crate) struct ConnFiles {
    out_tx: mpsc::Sender<OutFrame>,
    next_id: AtomicU32,
    shared: Arc<Mutex<ConnShared>>,
}

const HUNG_UP: &str = "the utcp process that made this call stopped (it hung up)";

impl ConnFiles {
    fn next_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    async fn send_json(&self, frame: &StreamFrame) -> Result<()> {
        let bytes = serde_json::to_vec(frame)?;
        self.out_tx
            .send(OutFrame::Json(bytes))
            .await
            .map_err(|_| anyhow!(HUNG_UP))
    }

    fn cancelled(&self, id: u32) -> Option<String> {
        let shared = self.shared.lock().ok()?;
        shared
            .cancelled
            .get(&id)
            .cloned()
            .or_else(|| shared.closed.then(|| HUNG_UP.to_string()))
    }

    /// Stream a file answer to the caller; the result to put in the call's answer.
    pub(crate) async fn pump(&self, mut file: FileDownload, item: Option<usize>) -> Result<Value> {
        let id = self.next_id();
        self.send_json(&StreamFrame::File {
            id,
            item,
            meta: file.meta.clone(),
        })
        .await?;
        let sent: Result<()> = async {
            while let Some(chunk) = file.stream.next().await {
                if let Some(message) = self.cancelled(id) {
                    return Err(anyhow!(message));
                }
                let chunk = chunk.map_err(|e| anyhow!("the download broke off: {e}"))?;
                let mut start = 0;
                while start < chunk.len() {
                    let end = (start + MAX_DATA_CHUNK).min(chunk.len());
                    self.out_tx
                        .send(OutFrame::Data(id, chunk.slice(start..end)))
                        .await
                        .map_err(|_| anyhow!(HUNG_UP))?;
                    start = end;
                }
            }
            Ok(())
        }
        .await;
        let _ = self.out_tx.send(OutFrame::End(id)).await;
        sent?;
        if let Some(message) = self.cancelled(id) {
            return Err(anyhow!(message));
        }
        // A cancel for the last bytes may still be on its way; the caller knows it either way.
        Ok(json!({ FILE_STREAM_KEY: id }))
    }
}

#[async_trait::async_trait]
impl CallerFiles for ConnFiles {
    async fn open_upload(&self, spec: &UploadSpec) -> Result<Upload> {
        let id = self.next_id();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (data_tx, data_rx) = mpsc::channel::<Bytes>(16);
        {
            let mut shared = self.shared.lock().map_err(|_| anyhow!(HUNG_UP))?;
            if shared.closed {
                return Err(anyhow!(HUNG_UP));
            }
            shared.uploads.insert(
                id,
                UploadSlot {
                    ready: Some(ready_tx),
                    data: Some(data_tx),
                },
            );
        }
        self.send_json(&StreamFrame::SendFile {
            id,
            path: spec.file.clone(),
            offset: spec.offset,
            length: spec.length,
        })
        .await?;
        let length = match ready_rx.await {
            Ok(Ok(size)) => size,
            Ok(Err(message)) => return Err(anyhow!(message)),
            Err(_) => return Err(anyhow!(HUNG_UP)),
        };
        let path = spec.file.clone();
        let state = (data_rx, 0u64, false);
        let stream = futures::stream::unfold(state, move |(mut rx, got, done)| {
            let path = path.clone();
            async move {
                if done {
                    return None;
                }
                match rx.recv().await {
                    Some(chunk) if got + chunk.len() as u64 <= length => {
                        let got = got + chunk.len() as u64;
                        Some((Ok(chunk), (rx, got, false)))
                    }
                    Some(_) => Some((
                        Err(std::io::Error::other(format!(
                            "{path} is longer than the {length} bytes announced"
                        ))),
                        (rx, got, true),
                    )),
                    None if got == length => None,
                    None => Some((
                        Err(std::io::Error::other(format!(
                            "the upload of {path} stopped after {got} of {length} bytes"
                        ))),
                        (rx, got, true),
                    )),
                }
            }
        });
        Ok(Upload {
            length,
            stream: Box::pin(stream),
        })
    }
}

/// Reads what the caller sends after its request: upload replies and bytes, and cancels.
async fn conn_reader<R: AsyncRead + Unpin>(mut reader: R, shared: Arc<Mutex<ConnShared>>) {
    let mut len_buf = [0u8; 4];
    loop {
        if reader.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let header = u32::from_le_bytes(len_buf);
        if header & DATA_FLAG != 0 {
            let len = (header & !DATA_FLAG) as usize;
            if !(4..=MAX_DATA_CHUNK + 4).contains(&len) {
                break;
            }
            let mut id_buf = [0u8; 4];
            if reader.read_exact(&mut id_buf).await.is_err() {
                break;
            }
            let id = u32::from_le_bytes(id_buf);
            let mut data = vec![0u8; len - 4];
            if reader.read_exact(&mut data).await.is_err() {
                break;
            }
            let sender = {
                let Ok(mut shared) = shared.lock() else { break };
                if data.is_empty() {
                    // The end of this upload: closing the channel ends the upload's stream.
                    shared.uploads.remove(&id);
                    None
                } else {
                    shared.uploads.get(&id).and_then(|s| s.data.clone())
                }
            };
            if let Some(sender) = sender {
                if sender.send(Bytes::from(data)).await.is_err() {
                    // The request stopped reading (it failed); drop the rest of this upload.
                    if let Ok(mut shared) = shared.lock() {
                        shared.uploads.remove(&id);
                    }
                }
            }
            continue;
        }
        let len = header as usize;
        if len == 0 || len > crate::ipc::MAX_IPC_FRAME_BYTES {
            break;
        }
        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).await.is_err() {
            break;
        }
        let Ok(frame) = serde_json::from_slice::<StreamFrame>(&payload) else {
            continue;
        };
        let Ok(mut shared) = shared.lock() else { break };
        match frame {
            StreamFrame::UploadReady { id, size } => {
                if let Some(ready) = shared.uploads.get_mut(&id).and_then(|s| s.ready.take()) {
                    let _ = ready.send(Ok(size));
                }
            }
            StreamFrame::UploadFailed { id, message } => {
                if let Some(mut slot) = shared.uploads.remove(&id) {
                    if let Some(ready) = slot.ready.take() {
                        let _ = ready.send(Err(message));
                    }
                }
            }
            StreamFrame::Cancel { id, message } => {
                shared.cancelled.insert(id, message);
            }
            StreamFrame::SendFile { .. } | StreamFrame::File { .. } => {}
        }
    }
    if let Ok(mut shared) = shared.lock() {
        shared.closed = true;
        for (_, mut slot) in shared.uploads.drain() {
            if let Some(ready) = slot.ready.take() {
                let _ = ready.send(Err(HUNG_UP.to_string()));
            }
        }
    }
}

/// Writes the daemon's frames to the caller, in order.
async fn conn_writer<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::Receiver<OutFrame>,
) -> std::io::Result<()> {
    while let Some(frame) = rx.recv().await {
        match frame {
            OutFrame::Json(bytes) => {
                writer
                    .write_all(&(bytes.len() as u32).to_le_bytes())
                    .await?;
                writer.write_all(&bytes).await?;
            }
            OutFrame::Data(id, bytes) => {
                let mut head = [0u8; 8];
                head[..4].copy_from_slice(&((4 + bytes.len() as u32) | DATA_FLAG).to_le_bytes());
                head[4..].copy_from_slice(&id.to_le_bytes());
                writer.write_all(&head).await?;
                writer.write_all(&bytes).await?;
            }
            OutFrame::End(id) => {
                let mut head = [0u8; 8];
                head[..4].copy_from_slice(&(4 | DATA_FLAG).to_le_bytes());
                head[4..].copy_from_slice(&id.to_le_bytes());
                writer.write_all(&head).await?;
            }
        }
        if rx.is_empty() {
            writer.flush().await?;
        }
    }
    writer.flush().await
}

/// Serve one call or batch: `handle` runs it with the connection's files, and its answer is the last
/// frame written.
pub(crate) async fn serve_call<R, W, F, Fut>(reader: R, writer: W, handle: F) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: FnOnce(Arc<ConnFiles>) -> Fut,
    Fut: std::future::Future<Output = IpcResponse>,
{
    let (out_tx, out_rx) = mpsc::channel(32);
    let shared = Arc::new(Mutex::new(ConnShared::default()));
    let writer_task = tokio::spawn(conn_writer(writer, out_rx));
    let reader_task = tokio::spawn(conn_reader(reader, shared.clone()));
    let conn = Arc::new(ConnFiles {
        out_tx: out_tx.clone(),
        next_id: AtomicU32::new(0),
        shared,
    });
    let resp = handle(conn.clone()).await;
    drop(conn);
    let bytes = serde_json::to_vec(&resp)?;
    let _ = out_tx.send(OutFrame::Json(bytes)).await;
    drop(out_tx);
    let written = writer_task.await;
    reader_task.abort();
    written??;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Caller side (synchronous)
// ---------------------------------------------------------------------------------------------

/// The caller's end: reads uploads, saves downloads, and where each answer goes.
pub struct ClientFiles {
    paths: UtcpPaths,
    limits: Option<Limits>,
    /// `--out` of a single call.
    pub call_out: Option<OutTarget>,
    /// `out` of batch items, by index.
    pub item_outs: HashMap<usize, OutTarget>,
    /// The call's tool, for naming a JSON answer saved into a folder.
    pub tool: String,
    sinks: HashMap<u32, DownloadSink>,
    complete: HashSet<u32>,
    failed: HashMap<u32, String>,
    buf: Vec<u8>,
}

impl ClientFiles {
    pub fn new(paths: &UtcpPaths) -> Self {
        Self {
            paths: paths.clone(),
            limits: None,
            call_out: None,
            item_outs: HashMap::new(),
            tool: "answer".into(),
            sinks: HashMap::new(),
            complete: HashSet::new(),
            failed: HashMap::new(),
            buf: Vec::new(),
        }
    }

    fn limits(&mut self) -> &Limits {
        let paths = &self.paths;
        self.limits.get_or_insert_with(|| Limits::load(paths))
    }

    fn send_file<S: Write>(&mut self, s: &mut S, id: u32, spec: UploadSpec) -> Result<()> {
        let opened = prepare_upload(&spec, self.limits());
        let (mut file, size) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                return write_frame(
                    s,
                    &StreamFrame::UploadFailed {
                        id,
                        message: format!("{e:#}"),
                    },
                )
            }
        };
        write_frame(s, &StreamFrame::UploadReady { id, size })?;
        let mut left = size;
        let mut chunk = vec![0u8; 256 * 1024];
        while left > 0 {
            let want = (left as usize).min(chunk.len());
            let n = match file.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break, // the daemon sees a short upload and says so
                Ok(n) => n,
            };
            write_data(s, id, &chunk[..n])?;
            left -= n as u64;
        }
        write_data(s, id, &[])
    }

    fn start<S: Write>(
        &mut self,
        s: &mut S,
        id: u32,
        item: Option<usize>,
        meta: FileMeta,
    ) -> Result<()> {
        let out = match item {
            Some(i) => self.item_outs.get(&i).cloned(),
            None => self.call_out.clone(),
        };
        let limits = self.limits().clone();
        match DownloadSink::create(meta, out.as_ref(), &self.paths, &limits) {
            Ok(sink) => {
                self.sinks.insert(id, sink);
                Ok(())
            }
            Err(e) => self.cancel(s, id, format!("{e:#}")),
        }
    }

    fn data<S: Write>(&mut self, s: &mut S, id: u32, bytes: &[u8]) -> Result<()> {
        let Some(sink) = self.sinks.get_mut(&id) else {
            return Ok(());
        };
        if let Err(e) = sink.write(bytes) {
            self.sinks.remove(&id);
            return self.cancel(s, id, format!("{e:#}"));
        }
        Ok(())
    }

    fn cancel<S: Write>(&mut self, s: &mut S, id: u32, message: String) -> Result<()> {
        self.failed.insert(id, message.clone());
        write_frame(s, &StreamFrame::Cancel { id, message })
    }

    /// The saved file's reference for a streamed result, or the error that stopped it.
    fn finish_stream(&mut self, id: u32) -> std::result::Result<Value, String> {
        if let Some(message) = self.failed.remove(&id) {
            return Err(message);
        }
        match (self.sinks.remove(&id), self.complete.contains(&id)) {
            (Some(sink), true) => sink.finish().map_err(|e| format!("{e:#}")),
            _ => Err("the file answer was cut off".into()),
        }
    }

    /// A result as the caller prints it: a streamed file's reference, or a JSON answer saved to `out`
    /// (kept as is, with a note, when it can't be saved). A page of a longer list is placed by its
    /// answer; its next-page arguments (and any `--where` note) stay beside it.
    fn place(
        &mut self,
        value: Value,
        out: Option<OutTarget>,
    ) -> std::result::Result<Value, String> {
        let mut shaped = crate::transfer::ShapedAnswer::from_value(value);
        let value = std::mem::take(&mut shaped.result);
        shaped.result = if let Some(id) = stream_id(&value) {
            self.finish_stream(id)?
        } else {
            match out {
                Some(target @ OutTarget::Path(_)) => {
                    let limits = self.limits().clone();
                    let (value, note) = crate::transfer::json_to_place_or_keep(
                        value,
                        Some(&target),
                        &self.tool,
                        &self.paths,
                        &limits,
                    );
                    if let Some(note) = note {
                        shaped.note = Some(match shaped.note.take() {
                            Some(first) => format!("{first}; {note}"),
                            None => note,
                        });
                    }
                    value
                }
                _ => value,
            }
        };
        Ok(shaped.into_value())
    }

    fn resolve(&mut self, resp: IpcResponse) -> IpcResponse {
        let resolved = match resp {
            IpcResponse::Ok {
                data: IpcPayload::CallResult(value),
            } => match self.place(value, self.call_out.clone()) {
                Ok(value) => IpcResponse::Ok {
                    data: IpcPayload::CallResult(value),
                },
                Err(message) => IpcResponse::Error { message },
            },
            IpcResponse::Ok {
                data: IpcPayload::BatchResults(mut items),
            } => {
                for (i, item) in items.iter_mut().enumerate() {
                    let Some(value) = item.result.take() else {
                        continue;
                    };
                    match self.place(value, self.item_outs.get(&i).cloned()) {
                        Ok(value) => item.result = Some(value),
                        Err(message) => {
                            item.ok = false;
                            item.status = "ERR".into();
                            item.error = Some(message);
                        }
                    }
                }
                IpcResponse::Ok {
                    data: IpcPayload::BatchResults(items),
                }
            }
            other => other,
        };
        // Anything not claimed by the answer was a failed call; its `.part` file goes.
        self.sinks.clear();
        resolved
    }
}

/// The id of a streamed file result (`{"utcp_file_stream": id}`).
pub fn stream_id(value: &Value) -> Option<u32> {
    let map = value.as_object()?;
    if map.len() != 1 {
        return None;
    }
    map.get(FILE_STREAM_KEY)?
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
}

fn write_frame<S: Write>(s: &mut S, frame: &StreamFrame) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    s.write_all(&(bytes.len() as u32).to_le_bytes())?;
    s.write_all(&bytes)?;
    s.flush()?;
    Ok(())
}

fn write_data<S: Write>(s: &mut S, id: u32, bytes: &[u8]) -> Result<()> {
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(&((4 + bytes.len() as u32) | DATA_FLAG).to_le_bytes());
    head[4..].copy_from_slice(&id.to_le_bytes());
    s.write_all(&head)?;
    s.write_all(bytes)?;
    if bytes.is_empty() {
        s.flush()?;
    }
    Ok(())
}

fn read_u32<S: Read>(s: &mut S) -> Result<u32> {
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// Send one request and read until its answer, serving the daemon's upload requests and saving
/// its file answers on the way.
pub(crate) fn exchange<S: Read + Write>(
    s: &mut S,
    payload: &[u8],
    files: &mut ClientFiles,
) -> Result<IpcResponse> {
    s.write_all(&(payload.len() as u32).to_le_bytes())?;
    s.write_all(payload)?;
    s.flush()?;
    loop {
        let header = read_u32(s)?;
        if header & DATA_FLAG != 0 {
            let len = (header & !DATA_FLAG) as usize;
            if !(4..=MAX_DATA_CHUNK + 4).contains(&len) {
                return Err(anyhow!("Invalid IPC data frame size: {len}"));
            }
            let id = read_u32(s)?;
            if len == 4 {
                files.complete.insert(id);
                continue;
            }
            let mut buf = std::mem::take(&mut files.buf);
            buf.resize(len - 4, 0);
            s.read_exact(&mut buf)?;
            let wrote = files.data(s, id, &buf);
            files.buf = buf;
            wrote?;
            continue;
        }
        let len = header as usize;
        if len > MAX_IPC_RESPONSE_BYTES {
            return Err(anyhow!("IPC response exceeded maximum frame size"));
        }
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload)?;
        let value: Value = serde_json::from_slice(&payload)?;
        if value.get("frame").is_some() {
            match serde_json::from_value::<StreamFrame>(value)? {
                StreamFrame::SendFile {
                    id,
                    path,
                    offset,
                    length,
                } => files.send_file(
                    s,
                    id,
                    UploadSpec {
                        file: path,
                        offset,
                        length,
                    },
                )?,
                StreamFrame::File { id, item, meta } => files.start(s, id, item, meta)?,
                _ => {}
            }
            continue;
        }
        let resp: IpcResponse = serde_json::from_value(value)?;
        return Ok(files.resolve(resp));
    }
}

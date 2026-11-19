pub mod cli;
pub mod graphql;
#[cfg(feature = "grpc")]
pub mod grpc;
pub mod http;
pub mod http_stream;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod rate_limit;
pub mod registry;
pub mod sse;
pub mod stream;
pub mod tcp;
pub mod text;
pub mod udp;
#[cfg(feature = "webrtc")]
pub mod webrtc;
pub mod websocket;

use crate::providers::base::Provider;
use crate::tools::Tool;
use crate::transports::stream::StreamResult;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

/// Start a tool's process without a console window of its own. The daemon runs detached, with
/// no console, so Windows would otherwise open a new console window for every console program
/// it starts (an MCP server, `npx`, a CLI tool). The child's stdio is piped, so nothing is lost.
pub(crate) fn hide_console_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Core transport abstraction all communication protocols implement.
#[async_trait]
pub trait ClientTransport: Send + Sync {
    /// Register a tool provider with the underlying transport, returning discovered tools.
    async fn register_tool_provider(&self, prov: &dyn Provider) -> Result<Vec<Tool>>;
    /// Deregister a tool provider and release any associated resources.
    async fn deregister_tool_provider(&self, prov: &dyn Provider) -> Result<()>;
    /// Invoke a tool over the transport and return the result payload.
    async fn call_tool(
        &self,
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Value>;
    /// Invoke a tool and stream incremental responses back to the caller.
    async fn call_tool_stream(
        &self,
        tool_name: &str,
        args: HashMap<String, Value>,
        prov: &dyn Provider,
    ) -> Result<Box<dyn StreamResult>>;
}

// CommunicationProtocol is the new name for transports; kept as a re-export for backwards
// compatibility so plugins can implement the updated terminology without breaking old code.
pub use ClientTransport as CommunicationProtocol;

/// What a local (stdio) MCP server is doing, for `utcp daemon servers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    /// No call has needed it yet.
    NotStarted,
    /// Being started (an `npx`/`uvx` download can take a while); calls wait for it.
    Starting,
    Running,
    /// It exited or crashed, or its last start failed; the next call starts it again.
    Exited,
    /// Stopped with `utcp daemon stop-server`; the next call starts it again.
    Stopped,
}

impl McpServerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not started",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Stopped => "stopped",
        }
    }
}

/// A local (stdio) MCP server's state as the transport sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub state: McpServerState,
    /// Process id of the started command (for `npx` and the like, the launcher). Running only.
    pub pid: Option<u32>,
    /// Seconds since it started. Running only.
    pub uptime_secs: Option<u64>,
    /// Starts after the first: crash recovery, changed settings, `restart-server`.
    pub restarts: u64,
}

impl McpServerStatus {
    pub fn not_started() -> Self {
        Self {
            state: McpServerState::NotStarted,
            pid: None,
            uptime_secs: None,
            restarts: 0,
        }
    }
}

/// Call before the process exits: waits, at most `max`, for stdio MCP servers that are still
/// being stopped, so one that ignores SIGTERM is still killed instead of left running.
pub fn wait_for_stopping_servers(max: std::time::Duration) {
    #[cfg(feature = "mcp")]
    mcp::wait_for_stopping_servers(max);
    #[cfg(not(feature = "mcp"))]
    let _ = max;
}

pub use registry::{
    communication_protocols_snapshot, register_communication_protocol,
    CommunicationProtocolRegistry,
};

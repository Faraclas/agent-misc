//! A throwaway utcp setup for the live tests: the manuals registered in a temporary config, called with `--direct`,
//! exactly as an agent calls them, without touching the user's utcp config, cache or daemon.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::text::py_strip;

static STATUS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"status:? ([0-9]{3})").unwrap());

pub struct Utcp {
    bin: PathBuf,
    tmp: tempfile::TempDir,
}

/// The outcome of one `utcp call`: `status` is "ok", "http NNN" or "error", with `bytes` and `seconds`, or `error`.
pub struct Called {
    pub result: Map<String, Value>,
    /// The parsed response (a JSON string when it is not JSON); `None` when the call failed.
    pub data: Option<Value>,
}

impl Called {
    pub fn status(&self) -> &str {
        self.result["status"].as_str().unwrap_or("")
    }
}

impl Utcp {
    pub fn new(bin: PathBuf, prefix: &str) -> std::io::Result<Self> {
        let tmp = tempfile::Builder::new().prefix(prefix).tempdir()?;
        Ok(Utcp { bin, tmp })
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).arg("--direct");
        for name in ["config", "cache", "state"] {
            cmd.env(
                format!("UTCP_{}_DIR", name.to_uppercase()),
                self.tmp.path().join(name),
            );
        }
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())?;
        }
        child.wait_with_output()
    }

    pub fn register<'a>(
        &self,
        folder: &Path,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), String> {
        for name in names {
            let file = folder.join(format!("{name}.utcp.json"));
            let out = self
                .run(&["register", name, &file.to_string_lossy()], None)
                .map_err(|e| format!("cannot run {}: {e}", self.bin.display()))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let msg: String = py_strip(&stderr).chars().take(300).collect();
                return Err(format!("utcp register {name} failed: {msg}"));
            }
        }
        Ok(())
    }

    /// `utcp call <manual>.<tool> --args-stdin`.
    pub fn call(&self, manual: &str, tool: &str, args: &Map<String, Value>) -> Called {
        let start = Instant::now();
        let target = format!("{manual}.{tool}");
        let stdin = serde_json::to_string(args).expect("JSON values always serialize");
        let out = self.run(&["call", &target, "--args-stdin"], Some(&stdin));
        let seconds = (start.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let mut result = Map::new();
        let data = match out {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let text = py_strip(&stdout);
                result.insert("status".into(), json!("ok"));
                result.insert("bytes".into(), json!(text.len()));
                result.insert("seconds".into(), json!(seconds));
                Some(serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let status = STATUS.captures(&stderr).map(|m| format!("http {}", &m[1]));
                result.insert(
                    "status".into(),
                    json!(status.unwrap_or_else(|| "error".into())),
                );
                result.insert("seconds".into(), json!(seconds));
                result.insert(
                    "error".into(),
                    json!(py_strip(&stderr).chars().take(500).collect::<String>()),
                );
                None
            }
            Err(e) => {
                result.insert("status".into(), json!("error"));
                result.insert("seconds".into(), json!(seconds));
                result.insert("error".into(), json!(format!("cannot run utcp: {e}")));
                None
            }
        };
        Called { result, data }
    }
}

//! Runs utcp for `generate --stage` and the live tests.
//!
//! `read-test` uses the user's real utcp, exactly as agents do: through the daemon when it runs, with the secrets
//! utcp already has, calling the staged manuals with `--staged`. `write-test` uses an isolated utcp setup instead:
//! its own config, cache and state in a temporary folder, so it never touches the user's manuals, daemon or agent
//! skills, and its token comes from the environment.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;
use serde_json::{json, Map, Value};

static STATUS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"status:? ([0-9]{3})").unwrap());

pub struct Utcp {
    bin: PathBuf,
    /// For an isolated setup (write-test): the folder holding its config, cache and state.
    isolated: Option<tempfile::TempDir>,
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
    /// The user's utcp: its manuals, secrets and daemon.
    pub fn shared(bin: PathBuf) -> Self {
        Utcp {
            bin,
            isolated: None,
        }
    }

    /// A throwaway utcp setup in a temporary folder, called with `--direct`. utcp gives a setup with its own config
    /// folder its own daemon endpoint and leaves the agent skills of the user's setup alone.
    pub fn isolated(bin: PathBuf, prefix: &str) -> std::io::Result<Self> {
        let tmp = tempfile::Builder::new().prefix(prefix).tempdir()?;
        Ok(Utcp {
            bin,
            isolated: Some(tmp),
        })
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        if let Some(tmp) = &self.isolated {
            cmd.arg("--direct");
            for name in ["config", "cache", "state"] {
                cmd.env(
                    format!("UTCP_{}_DIR", name.to_uppercase()),
                    tmp.path().join(name),
                );
            }
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

    fn run_ok(&self, args: &[&str]) -> Result<String, String> {
        let out = self
            .run(args, None)
            .map_err(|e| format!("cannot run {}: {e}", self.bin.display()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg: String = stderr.trim().chars().take(300).collect();
            return Err(format!("utcp {} failed: {msg}", args.join(" ")));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `utcp register <name> <folder>/<name>.utcp.json` for each name (write-test's isolated setup).
    pub fn register<'a>(
        &self,
        folder: &Path,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), String> {
        for name in names {
            let file = folder.join(format!("{name}.utcp.json"));
            self.run_ok(&["register", name, &file.to_string_lossy()])?;
        }
        Ok(())
    }

    /// The names of the manuals with tools in `utcp list` (or `utcp list --staged`).
    pub fn manual_names(&self, staged: bool) -> Result<BTreeSet<String>, String> {
        let args: &[&str] = if staged {
            &["list", "--json", "--staged"]
        } else {
            &["list", "--json"]
        };
        let out = self.run_ok(args)?;
        let tools: Value = serde_json::from_str(out.trim())
            .map_err(|e| format!("utcp {} did not print JSON: {e}", args.join(" ")))?;
        Ok(tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["manual"].as_str().map(String::from))
            .collect())
    }

    /// `utcp call [--staged] <manual>.<tool> --args-stdin`.
    pub fn call(
        &self,
        manual: &str,
        tool: &str,
        args: &Map<String, Value>,
        staged: bool,
    ) -> Called {
        self.call_to(manual, tool, args, staged, None)
    }

    /// [`Self::call`], saving the answer to `out` (`--out`). A file answer's `data` is utcp's reference to the saved
    /// file; for a JSON file (an answer too large to print, which is then deleted) it is the file's content.
    pub fn call_to(
        &self,
        manual: &str,
        tool: &str,
        args: &Map<String, Value>,
        staged: bool,
        out: Option<&Path>,
    ) -> Called {
        let start = Instant::now();
        let target = format!("{manual}.{tool}");
        let stdin = serde_json::to_string(args).expect("JSON values always serialize");
        let out_arg = out.map(|p| p.to_string_lossy().into_owned());
        let mut argv = vec!["call", &target, "--args-stdin"];
        if staged {
            argv.push("--staged");
        }
        if let Some(o) = &out_arg {
            argv.extend(["--out", o.as_str()]);
        }
        let out = self.run(&argv, Some(&stdin));
        let seconds = (start.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        let mut result = Map::new();
        let data = match out {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let text = stdout.trim();
                result.insert("status".into(), json!("ok"));
                result.insert("bytes".into(), json!(text.len()));
                result.insert("seconds".into(), json!(seconds));
                let answer = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
                Some(match saved_file(&answer) {
                    Some(path) => {
                        result.insert("bytes".into(), answer["bytes"].clone());
                        result.insert("file".into(), json!(true));
                        let content = answer["mime_type"]
                            .as_str()
                            .is_some_and(|m| m.contains("json"))
                            .then(|| std::fs::read(&path).ok())
                            .flatten()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                        // An answer too large to print sits in utcp's own folder; it was only needed here.
                        if answer.get("note").is_some() && out_arg.is_none() {
                            let _ = std::fs::remove_file(&path);
                        }
                        content.unwrap_or(answer)
                    }
                    None => answer,
                })
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
                    json!(stderr.trim().chars().take(500).collect::<String>()),
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

/// The path of a file utcp saved, from its reference `{"type": "file", "path": ...}`.
pub fn saved_file(answer: &Value) -> Option<PathBuf> {
    (answer["type"] == "file")
        .then(|| answer["path"].as_str())
        .flatten()
        .filter(|p| *p != "-")
        .map(PathBuf::from)
}

/// The oldest utcp that sends every generated tool correctly: 0.2.2 added DELETE bodies and text bodies.
pub const MIN_VERSION: (u64, u64, u64) = (0, 2, 2);

/// "0.2.2", for messages.
pub fn min_version_text() -> String {
    let (a, b, c) = MIN_VERSION;
    format!("{a}.{b}.{c}")
}

/// The version `utcp --version` prints ("utcp 0.2.2"), or `None` when it cannot be run or read.
pub fn version(bin: &Path) -> Option<(u64, u64, u64)> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let word = text.split_whitespace().nth(1)?;
    let core = word.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// `Ok(())` when `bin` is utcp [`MIN_VERSION`] or newer, else why not.
pub fn check_version(bin: &Path) -> Result<(), String> {
    match version(bin) {
        Some(v) if v >= MIN_VERSION => Ok(()),
        Some((a, b, c)) => Err(format!(
            "{} is utcp {a}.{b}.{c}; utcp {} or newer is needed",
            bin.display(),
            min_version_text()
        )),
        None => Err(format!(
            "cannot read the version of {} (utcp --version); utcp {} or newer is needed",
            bin.display(),
            min_version_text()
        )),
    }
}

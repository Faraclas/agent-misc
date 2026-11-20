//! The secrets command: `[secrets] command` in `settings.toml` names a program that prints the
//! secrets registered manuals need, so they can stay wherever the user keeps them (a shell file,
//! `pass`, a keyring, a password manager). utcp runs it with the wanted variable names as extra
//! arguments and reads `NAME=value` lines from its stdout. It keeps only wanted, non-empty values,
//! in memory only, and never logs a value: errors name the problem, never the output or stderr.
//!
//! The daemon runs it at startup, on every reload (a change in the config folder, such as
//! `register`) and on `utcp daemon reload-env`. Without a daemon, a `utcp` process runs it at most
//! once a minute for the same names ([`cached_run`]).

use crate::paths::{SecretsSettings, UtcpPaths};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const DIRECT_CACHE_TTL: Duration = Duration::from_secs(60);

/// What the last run of the secrets command did: names only, never values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretsCommandStatus {
    /// The configured command, as written in `settings.toml` (without the appended names).
    pub command: String,
    /// Why the last run failed; `None` when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The wanted names it printed a non-empty value for, sorted.
    #[serde(default)]
    pub provided: Vec<String>,
}

/// The result of one run: the kept values and the status to report.
#[derive(Debug, Clone)]
pub struct SecretsRun {
    pub values: HashMap<String, String>,
    pub status: SecretsCommandStatus,
}

/// Runs the secrets command once, asking for `wanted`. A failure gives no values and the reason in
/// `status.error`; it never panics or blocks past the configured timeout.
pub fn run_secrets_command(settings: &SecretsSettings, wanted: &BTreeSet<String>) -> SecretsRun {
    let command = settings.command.join(" ");
    match run_inner(settings, wanted) {
        Ok(values) => {
            let mut provided: Vec<String> = values.keys().cloned().collect();
            provided.sort();
            SecretsRun {
                values,
                status: SecretsCommandStatus {
                    command,
                    error: None,
                    provided,
                },
            }
        }
        Err(error) => SecretsRun {
            values: HashMap::new(),
            status: SecretsCommandStatus {
                command,
                error: Some(error),
                provided: Vec::new(),
            },
        },
    }
}

type CacheKey = (PathBuf, Vec<String>, BTreeSet<String>);
static DIRECT_CACHE: Mutex<Vec<(CacheKey, Instant, SecretsRun)>> = Mutex::new(Vec::new());

/// [`run_secrets_command`] for a `utcp` process without the daemon: reuses a run with the same
/// settings file, command and names for up to a minute, so one CLI call or a TUI refresh does not
/// run the command several times.
pub fn cached_run(
    paths: &UtcpPaths,
    settings: &SecretsSettings,
    wanted: &BTreeSet<String>,
) -> SecretsRun {
    let key = (
        paths.settings_file.clone(),
        settings.command.clone(),
        wanted.clone(),
    );
    let mut cache = DIRECT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(_, at, _)| at.elapsed() < DIRECT_CACHE_TTL);
    if let Some((_, _, run)) = cache.iter().find(|(k, _, _)| *k == key) {
        return run.clone();
    }
    let run = run_secrets_command(settings, wanted);
    cache.push((key, Instant::now(), run.clone()));
    run
}

fn run_inner(
    settings: &SecretsSettings,
    wanted: &BTreeSet<String>,
) -> Result<HashMap<String, String>, String> {
    let Some((program, args)) = settings.command.split_first() else {
        return Err("`command` in [secrets] is empty".into());
    };
    let mut cmd = Command::new(expand_home(program));
    cmd.args(args.iter().map(|a| expand_home(a)))
        .args(wanted)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start `{program}`: {e}"))?;

    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });

    let timeout = Duration::from_secs(settings.timeout_secs.max(1));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Err(e) => return Err(format!("could not wait for it: {e}")),
        }
    };
    // A program it left running in the background can hold stdout open: wait only until the
    // deadline (at least briefly, for output still in the pipe).
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    let out = rx
        .recv_timeout(remaining)
        .map_err(|_| "its output did not end (a program it started keeps it open)".to_string())?;

    if !status.success() {
        let how = match status.code() {
            Some(code) => format!("exited with code {code}"),
            None => "was stopped by a signal".to_string(),
        };
        return Err(format!("{how}; run it by hand to see its error output"));
    }
    if out.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(format!("printed more than {MAX_OUTPUT_BYTES} bytes"));
    }
    let out = String::from_utf8(out).map_err(|_| "printed output that is not UTF-8".to_string())?;
    parse_output(&out, wanted)
}

/// Reads `NAME=value` lines. Blank lines and `#` comments are skipped, a leading `export ` is
/// allowed, and one pair of matching quotes around a value is removed. Values are taken literally
/// (no `$VAR` expansion or escapes), so a secret containing `$` or `\` survives. Only wanted names
/// with a non-empty value are kept. Errors give the line number only, never its content.
pub fn parse_output(
    out: &str,
    wanted: &BTreeSet<String>,
) -> Result<HashMap<String, String>, String> {
    let mut values = HashMap::new();
    for (i, line) in out.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let parsed = line
            .split_once('=')
            .map(|(name, value)| (name.trim(), unquote(value.trim())))
            .filter(|(name, _)| is_var_name(name));
        let Some((name, value)) = parsed else {
            return Err(format!("output line {} is not NAME=value", i + 1));
        };
        if !value.is_empty() && wanted.contains(name) {
            values.insert(name.to_string(), value.to_string());
        }
    }
    Ok(values)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `~` or a leading `~/` (`~\` on Windows) becomes the home folder.
fn expand_home(arg: &str) -> PathBuf {
    let rest = if arg == "~" {
        Some("")
    } else {
        arg.strip_prefix("~/")
            .or_else(|| arg.strip_prefix("~\\").filter(|_| cfg!(windows)))
    };
    match (
        rest,
        directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()),
    ) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(arg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_literal_values_and_keeps_only_wanted_names() {
        let out =
            "# from the vault\n\nexport GITHUB_TOKEN=ghp_a$b\\c\r\nCONTEXT7_API_KEY=\"k 1\"\n\
                   OTHER_SECRET=nope\nQUOTED='x'\nEMPTY=\n";
        let got = parse_output(
            out,
            &names(&["GITHUB_TOKEN", "CONTEXT7_API_KEY", "QUOTED", "EMPTY"]),
        )
        .unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got["GITHUB_TOKEN"], "ghp_a$b\\c");
        assert_eq!(got["CONTEXT7_API_KEY"], "k 1");
        assert_eq!(got["QUOTED"], "x");
    }

    #[test]
    fn a_bad_line_is_an_error_that_does_not_show_its_content() {
        let err = parse_output("OK=1\nsecret-without-a-name\n", &names(&["OK"])).unwrap_err();
        assert_eq!(err, "output line 2 is not NAME=value");
        let err = parse_output("1BAD=x\n", &names(&[])).unwrap_err();
        assert_eq!(err, "output line 1 is not NAME=value");
    }

    #[test]
    fn empty_command_is_reported_not_run() {
        let run = run_secrets_command(
            &SecretsSettings {
                command: Vec::new(),
                timeout_secs: 1,
            },
            &names(&["X"]),
        );
        assert!(run.values.is_empty());
        assert!(run.status.error.unwrap().contains("empty"));
    }

    #[test]
    fn missing_program_is_reported() {
        let run = run_secrets_command(
            &SecretsSettings {
                command: vec!["utcp-no-such-secrets-program".into()],
                timeout_secs: 1,
            },
            &names(&["X"]),
        );
        assert!(run.values.is_empty());
        assert!(
            run.status
                .error
                .as_deref()
                .unwrap()
                .contains("could not start"),
            "{:?}",
            run.status
        );
    }

    #[test]
    fn expands_only_a_leading_tilde() {
        let home = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/bin/x.sh"), home.join("bin/x.sh"));
        assert_eq!(expand_home("/bin/~/x"), PathBuf::from("/bin/~/x"));
        assert_eq!(expand_home("~other/x"), PathBuf::from("~other/x"));
    }

    /// `node secret_source.cjs <config>` with `config` written to a temp file. `UTCP_TEST_FIXTURES`
    /// overrides the fixtures folder, so the compiled tests can run without the source tree.
    fn node_source(
        tmp: &tempfile::TempDir,
        config: serde_json::Value,
    ) -> (SecretsSettings, PathBuf) {
        let script = std::env::var_os("UTCP_TEST_FIXTURES")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
            })
            .join("secret_source.cjs");
        let config_file = tmp.path().join("secrets.json");
        std::fs::write(&config_file, config.to_string()).unwrap();
        let settings = SecretsSettings {
            command: vec![
                "node".into(),
                script.display().to_string(),
                config_file.display().to_string(),
            ],
            timeout_secs: 5,
        };
        (settings, tmp.path().join("secrets.json.runs"))
    }

    #[test]
    fn runs_the_command_with_the_wanted_names_and_keeps_only_those() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (settings, runs) = node_source(
            &tmp,
            serde_json::json!({"values": {"GITHUB_TOKEN": "ghp_1", "my__api_KEY": "k2"}}),
        );
        let run = run_secrets_command(&settings, &names(&["GITHUB_TOKEN", "my__api_KEY", "NONE"]));
        assert_eq!(run.status.error, None);
        assert_eq!(run.status.provided, ["GITHUB_TOKEN", "my__api_KEY"]);
        assert_eq!(run.values.len(), 2, "UNASKED_SECRET must be dropped");
        assert_eq!(run.values["GITHUB_TOKEN"], "ghp_1");
        assert_eq!(
            std::fs::read_to_string(runs).unwrap(),
            "GITHUB_TOKEN NONE my__api_KEY\n"
        );
    }

    #[test]
    fn failures_give_no_values_and_never_show_output_or_stderr() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (settings, _) = node_source(&tmp, serde_json::json!({"exit": 3}));
        let run = run_secrets_command(&settings, &names(&["X"]));
        let err = run.status.error.unwrap();
        assert!(err.starts_with("exited with code 3"), "{err}");
        assert!(!err.contains("SECRET-VALUE"), "{err}");
        assert!(run.values.is_empty());

        let (settings, _) =
            node_source(&tmp, serde_json::json!({"values": {"X": "1"}, "bad": true}));
        let run = run_secrets_command(&settings, &names(&["X"]));
        let err = run.status.error.unwrap();
        assert!(err.contains("is not NAME=value"), "{err}");
        assert!(!err.contains("SECRET-VALUE"), "{err}");
        assert!(run.values.is_empty());
    }

    #[test]
    fn a_hanging_command_is_stopped_at_the_timeout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut settings, _) = node_source(&tmp, serde_json::json!({"sleep_ms": 30000}));
        settings.timeout_secs = 1;
        let started = Instant::now();
        let run = run_secrets_command(&settings, &names(&["X"]));
        assert_eq!(run.status.error.as_deref(), Some("timed out after 1s"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn direct_runs_are_reused_for_the_same_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (settings, runs) = node_source(&tmp, serde_json::json!({"values": {"A": "1"}}));
        let paths = crate::paths::test_support::paths_in(tmp.path());
        assert_eq!(
            cached_run(&paths, &settings, &names(&["A"])).values["A"],
            "1"
        );
        assert_eq!(
            cached_run(&paths, &settings, &names(&["A"])).values["A"],
            "1"
        );
        cached_run(&paths, &settings, &names(&["A", "B"]));
        assert_eq!(std::fs::read_to_string(runs).unwrap(), "A\nA B\n");
    }
}

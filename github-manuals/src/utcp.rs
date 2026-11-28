//! The utcp version check for `generate --stage` and `search-test`, whether utcp already has the token, and running
//! utcp for its output.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// The oldest utcp that runs every generated tool correctly: 0.2.2 added DELETE bodies and text bodies, 0.2.3
/// reports GitHub's next page (`next_page`, from the `Link` header) instead of silently returning only the first,
/// and 0.2.4 takes the `'github_*'` pattern the next steps use (`utcp diff`, `utcp promote`) and adds `--where`.
pub const MIN_VERSION: (u64, u64, u64) = (0, 2, 4);

/// "0.2.4", for messages.
pub fn min_version_text() -> String {
    let (a, b, c) = MIN_VERSION;
    format!("{a}.{b}.{c}")
}

/// The version `utcp --version` prints ("utcp 0.2.4"), or `None` when it cannot be run or read.
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

/// What `bin args` prints on standard output, or why it could not run or failed (with utcp's error output).
pub fn output(bin: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "utcp {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Whether utcp (its daemon, when one runs) has the secret `var`, from `utcp doctor --json`. `None` when doctor
/// cannot be run or read, or lists no manual that uses `var`.
pub fn secret_is_set(bin: &Path, var: &str) -> Option<bool> {
    let out = Command::new(bin).args(["doctor", "--json"]).output().ok()?;
    let doctor: Value = serde_json::from_slice(&out.stdout).ok()?;
    secret_is_set_in(&doctor, var)
}

pub(crate) fn secret_is_set_in(doctor: &Value, var: &str) -> Option<bool> {
    let mut found = None;
    for c in doctor["doctor"]["credentials"].as_array()? {
        if c["env_var"] == var {
            if c["is_set"] == true {
                return Some(true);
            }
            found = Some(false);
        }
    }
    found
}

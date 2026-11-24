//! The utcp version check for `generate --stage`.

use std::path::Path;
use std::process::Command;

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

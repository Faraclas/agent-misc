//! Finding the program that a configured command names.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The program to start for `program`, a command from a manual (`import-mcp --command`, a CLI
/// tool) or from `[secrets] command` in `settings.toml`.
///
/// On Windows a bare name such as `npx` is looked up on `PATH` with the extensions in `PATHEXT`,
/// as a shell does. Rust's `Command` only looks for `npx.exe`, but Node.js, Python tools and the
/// Azure CLI install their launchers as `npx.cmd` or `az.cmd`. A name with a folder or with an
/// extension from `PATHEXT`, or one that is not found, is returned unchanged, so starting it fails
/// with the usual "program not found". On other systems the name is always returned unchanged.
pub fn resolve(program: &Path) -> PathBuf {
    resolve_with_env(program, None)
}

/// [`resolve`] for a program started with extra environment variables `env`. When they set
/// `PATH`, the lookup uses that `PATH`, as Rust's `Command` does for a child with its own `PATH`.
pub fn resolve_with_env(
    program: &Path,
    env: Option<&std::collections::HashMap<String, String>>,
) -> PathBuf {
    #[cfg(windows)]
    {
        let child_path = env.and_then(|vars| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
                .map(|(_, v)| std::ffi::OsString::from(v))
        });
        let path = child_path.or_else(|| std::env::var_os("PATH"));
        if let Some(found) = program.to_str().and_then(|name| {
            find_with_extensions(
                name,
                path.as_deref(),
                std::env::var_os("PATHEXT").as_deref(),
            )
        }) {
            return found;
        }
    }
    #[cfg(not(windows))]
    let _ = env;
    program.to_path_buf()
}

/// Windows' own default `PATHEXT`, cut to the extensions it can start without a file association.
const STARTABLE_EXTENSIONS: [&str; 4] = [".com", ".exe", ".bat", ".cmd"];

/// Looks for `name` plus each startable `PATHEXT` extension in every `PATH` folder, in order: the
/// first folder that has any of them wins, and within a folder the `PATHEXT` order decides.
#[cfg_attr(not(windows), allow(dead_code))]
fn find_with_extensions(
    name: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\', ':']) {
        return None;
    }
    let listed: Vec<String> = pathext
        .and_then(OsStr::to_str)
        .map(|s| {
            s.split(';')
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| STARTABLE_EXTENSIONS.contains(&e.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let extensions: Vec<String> = if listed.is_empty() {
        STARTABLE_EXTENSIONS.iter().map(|e| e.to_string()).collect()
    } else {
        listed
    };
    let lower = name.to_ascii_lowercase();
    if STARTABLE_EXTENSIONS.iter().any(|e| lower.ends_with(e)) {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            extensions
                .iter()
                .map(|ext| dir.join(format!("{name}{ext}")))
                .find(|candidate| candidate.is_file())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn touch(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), b"").unwrap();
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn finds_cmd_launchers_in_path_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        touch(first.path(), "az.cmd");
        touch(second.path(), "npx.cmd");
        touch(second.path(), "az.exe");
        let path = path_of(&[first.path(), second.path()]);
        let pathext = OsString::from(".COM;.EXE;.BAT;.CMD;.VBS;.JS");

        assert_eq!(
            find_with_extensions("npx", Some(&path), Some(&pathext)),
            Some(second.path().join("npx.cmd"))
        );
        // The first folder that has the program wins, even with a "better" extension later.
        assert_eq!(
            find_with_extensions("az", Some(&path), Some(&pathext)),
            Some(first.path().join("az.cmd"))
        );
    }

    #[test]
    fn pathext_order_decides_within_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "tool.cmd");
        touch(dir.path(), "tool.exe");
        let path = path_of(&[dir.path()]);

        assert_eq!(
            find_with_extensions("tool", Some(&path), Some(OsStr::new(".EXE;.CMD"))),
            Some(dir.path().join("tool.exe"))
        );
        assert_eq!(
            find_with_extensions("tool", Some(&path), Some(OsStr::new(".CMD;.EXE"))),
            Some(dir.path().join("tool.cmd"))
        );
        // No usable PATHEXT: Windows' default order (.COM;.EXE;.BAT;.CMD).
        assert_eq!(
            find_with_extensions("tool", Some(&path), None),
            Some(dir.path().join("tool.exe"))
        );
        assert_eq!(
            find_with_extensions("tool", Some(&path), Some(OsStr::new(".VBS;.JS"))),
            Some(dir.path().join("tool.exe"))
        );
    }

    #[test]
    fn leaves_paths_extensions_and_unknown_names_alone() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "npx.cmd");
        touch(dir.path(), "npx.cmd.cmd");
        touch(dir.path(), "script.js");
        let path = path_of(&[dir.path()]);
        let pathext = OsString::from(".COM;.EXE;.BAT;.CMD;.JS");

        for name in [
            "npx.cmd",
            "NPX.CMD",
            r"C:\nodejs\npx",
            "tools/npx",
            "C:npx",
            "missing",
            "script",
            "",
        ] {
            assert_eq!(
                find_with_extensions(name, Some(&path), Some(&pathext)),
                None,
                "{name}"
            );
        }
        assert_eq!(find_with_extensions("npx", None, Some(&pathext)), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn other_systems_keep_the_name() {
        assert_eq!(resolve(Path::new("npx")), PathBuf::from("npx"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_finds_cmd_exe_itself() {
        let found = resolve(Path::new("cmd"));
        assert!(
            found
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(r"\cmd.exe"),
            "{}",
            found.display()
        );
    }
}

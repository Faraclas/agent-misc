//! Command line: `github-manuals generate [--out <dir>] [--stage]`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::Value;

use crate::config::{Config, PAT_VAR};
use crate::generate::{self, GenerateError};
use crate::http::ureq_transport;
use crate::source::{self, Pins, SourceError};
use crate::text::write_json;
use crate::utcp;

const HELP_TEMPLATE: &str =
    "{about-with-newline}\n{before-help}{usage-heading} {usage}\n\n{all-args}{after-help}";

const GETTING_STARTED: &str = "\
Getting started (once per user):
  1. github-manuals generate --stage
     Writes the manuals and stages them in utcp (agents do not see them yet).
  2. Give utcp a GitHub token as the secret GITHUB_MANUALS_PAT, then check it with: utcp doctor
  3. Review and try the staged manuals: utcp diff 'github_*', utcp call --staged ...
  4. Make them active for your agents: utcp promote 'github_*'
generate prints these steps again when it is done. After upgrading github-manuals, run step 1 again, check what
changes with utcp diff 'github_*', then step 4.";

const HELP_EXAMPLES: &str = "\
Examples:
  github-manuals help                prints this message
  github-manuals help generate       prints the help for the generate command
  github-manuals generate --help     the same";

const GENERATE_LONG: &str = "\
Write the manuals for GitHub's REST API.

Downloads GitHub's OpenAPI description on the first run (about 14 MB, checked against the pinned hash and cached
afterwards) and writes one utcp manual per area of GitHub, plus report.json (every tool, and every operation left
out with the reason). No token is needed for this.

With --stage the manuals go to utcp's staging area, where agents do not see them yet: review them with
utcp diff 'github_*', try them with utcp call --staged, then make them active with utcp promote 'github_*'.

Example (writes, stages, and prints what to do next):
  github-manuals generate --stage

Run it again to update the manuals, for example after upgrading github-manuals.";

/// Generate utcp HTTP manuals for the GitHub REST API from GitHub's OpenAPI description.
#[derive(Parser)]
#[command(
    name = "github-manuals",
    version,
    about,
    help_template = HELP_TEMPLATE,
    before_help = GETTING_STARTED,
    after_help = HELP_EXAMPLES,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the manuals (no token needed)
    #[command(long_about = GENERATE_LONG)]
    Generate(GenerateArgs),
    /// Maintainers: pin another commit of GitHub's description in a pins file
    Pins(PinsArgs),
    /// Print this message, or the help for one command
    Help {
        /// The command to print the help for
        command: Option<String>,
    },
}

#[derive(Args)]
struct GenerateArgs {
    /// Folder for the manuals [default: ~/.local/share/github-manuals, on Windows
    /// %LOCALAPPDATA%\github-manuals\manuals]
    #[arg(long)]
    out: Option<PathBuf>,
    /// Folder for the downloaded description [default: ~/.cache/github-manuals, on Windows
    /// %LOCALAPPDATA%\github-manuals\cache]
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Also stage every manual in utcp (utcp stage), so you can review and try them before agents see them
    #[arg(long)]
    stage: bool,
    /// utcp binary for --stage [default: utcp on PATH]
    #[arg(long)]
    utcp: Option<PathBuf>,
    /// Leave out the write tools
    #[arg(long)]
    read_only: bool,
    /// A pins file to use instead of the one built in (maintainers)
    #[arg(long, hide = true)]
    pins: Option<PathBuf>,
}

#[derive(Args)]
struct PinsArgs {
    /// Commit of github/rest-api-description to pin (full or abbreviated sha)
    #[arg(long)]
    commit: String,
    /// API version whose file to pin [default: the current one]
    #[arg(long)]
    api_version: Option<String>,
    /// Pins file to write (rebuild the binary to use it; it embeds github-manuals/pins.json)
    #[arg(long, default_value = "pins.json")]
    pins: PathBuf,
}

/// A failure reported as `github-manuals: <message>` with the given exit code.
struct Fail(i32, String);

impl From<SourceError> for Fail {
    fn from(e: SourceError) -> Fail {
        Fail(1, e.0)
    }
}

impl From<GenerateError> for Fail {
    fn from(e: GenerateError) -> Fail {
        Fail(1, e.0)
    }
}

fn io_fail(path: &Path) -> impl Fn(std::io::Error) -> Fail + '_ {
    move |e| Fail(1, format!("{}: {e}", path.display()))
}

pub fn main() -> i32 {
    let cli = match Cli::try_parse_from(help_alias(std::env::args_os().collect())) {
        Ok(cli) => cli,
        Err(e) => reword(e).exit(),
    };
    let result = match cli.command {
        Cmd::Generate(a) => cmd_generate(a),
        Cmd::Pins(a) => cmd_pins(a),
        Cmd::Help { command } => help(command.as_deref()).exit(),
    };
    match result {
        Ok(code) => code,
        Err(Fail(code, msg)) => {
            eprintln!("github-manuals: {msg}");
            code
        }
    }
}

/// `github-manuals <command> help` means `github-manuals help <command>` (no command takes a positional argument).
pub(crate) fn help_alias(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    if args.len() == 3 && args[2] == "help" {
        let known = Cli::command()
            .get_subcommands()
            .any(|c| args[1] == c.get_name());
        if known {
            args.swap(1, 2);
        }
    }
    args
}

/// The help of the whole program or of one command, as a clap "error" that prints it and exits with 0 (or, for an
/// unknown command, prints the error and exits with 2).
pub(crate) fn help(command: Option<&str>) -> clap::Error {
    let args = ["github-manuals"]
        .into_iter()
        .chain(command)
        .chain(["--help"]);
    match Cli::try_parse_from(args) {
        Err(e) => reword(e),
        Ok(_) => unreachable!("--help always stops parsing"),
    }
}

/// clap calls the commands "subcommands"; this tool calls them commands.
fn reword(e: clap::Error) -> clap::Error {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    if e.kind() != ErrorKind::InvalidSubcommand {
        return e;
    }
    let name = match e.get(ContextKind::InvalidSubcommand) {
        Some(ContextValue::String(name)) => name.clone(),
        _ => return e,
    };
    Cli::command().error(
        ErrorKind::InvalidSubcommand,
        format!("unknown command '{name}' (github-manuals help lists the commands)"),
    )
}

/// `shutil.which`: the first `name` (or `name.exe` on Windows) on PATH.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

fn find_utcp(given: Option<PathBuf>) -> Result<PathBuf, Fail> {
    let bin = given
        .or_else(|| which("utcp"))
        .ok_or_else(|| Fail(2, "no utcp binary (use --utcp)".into()))?;
    utcp::check_version(&bin).map_err(|e| Fail(2, e))?;
    Ok(bin)
}

/// What `generate` left out, one line per reason with its count.
pub(crate) fn left_out_lines(report: &Value) -> Vec<String> {
    let rows: Vec<(String, String)> = report["left_out_summary"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            (
                x["operations"].to_string(),
                x["reason"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(n, why)| format!("  {n:>width$}  {why}"))
        .collect()
}

fn cmd_generate(args: GenerateArgs) -> Result<i32, Fail> {
    let conf = if args.read_only {
        Config::real().read_only()
    } else {
        Config::real()
    };
    let utcp = if args.stage {
        Some(find_utcp(args.utcp)?)
    } else {
        None
    };
    let pins = Pins::load(args.pins.as_deref())?;
    let cache = args.cache.unwrap_or_else(source::default_cache_dir);
    let mut transport = ureq_transport();
    let (data, downloaded) = source::fetch_spec(&mut transport, &pins, &cache)?;
    if downloaded {
        println!(
            "Downloaded GitHub's OpenAPI description (cache: {}).",
            cache.display()
        );
    }
    let spec = source::parse_spec(&data)?;
    let (manuals, report) =
        generate::generate(&spec, &pins.api_version, &pins.source_json(), &conf)?;
    let out = args.out.unwrap_or_else(source::default_out_dir);
    fs::create_dir_all(&out).map_err(io_fail(&out))?;
    for (name, manual) in &manuals {
        let path = out.join(format!("{name}.utcp.json"));
        write_json(&path, manual).map_err(io_fail(&path))?;
    }
    let path = out.join("report.json");
    write_json(&path, &report).map_err(io_fail(&path))?;
    println!(
        "Wrote {} manuals (GitHub REST API {}) to {}:",
        manuals.len(),
        pins.api_version,
        out.display()
    );
    let width = manuals.keys().map(String::len).max().unwrap_or(0);
    for (name, m) in report["manuals"].as_object().into_iter().flatten() {
        println!(
            "  {name:width$}  {:>4} tools  {}",
            m["tools"].to_string(),
            m["hint"].as_str().unwrap_or("")
        );
    }
    println!("Left out (operations, reason; every one is listed in report.json in that folder):");
    for line in left_out_lines(&report) {
        println!("{line}");
    }
    let staged = match &utcp {
        None => false,
        Some(utcp) => {
            for (name, m) in report["manuals"].as_object().into_iter().flatten() {
                let file = out.join(format!("{name}.utcp.json"));
                let hint = m["hint"].as_str().unwrap_or("");
                let output = Command::new(utcp)
                    .args(["stage", name])
                    .arg(&file)
                    .args(["--hint", hint])
                    .output()
                    .map_err(|e| Fail(1, format!("cannot run {}: {e}", utcp.display())))?;
                if !output.status.success() {
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                    return Err(Fail(1, format!("utcp stage {name} failed (output above)")));
                }
            }
            println!(
                "Staged the {} manuals in utcp. Agents do not see them until you promote them.",
                manuals.len()
            );
            true
        }
    };
    println!();
    let token_set = staged
        && utcp
            .as_deref()
            .and_then(|u| crate::utcp::secret_is_set(u, PAT_VAR))
            == Some(true);
    if token_set {
        println!("utcp already has the token {PAT_VAR} (utcp doctor shows it [SET]).");
    }
    println!("Next:");
    let mut steps = Vec::new();
    if !staged {
        steps.push("Stage the manuals in utcp: github-manuals generate --stage".to_string());
    }
    steps.extend(next_steps(token_set));
    for (i, step) in steps.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
    Ok(0)
}

/// The steps after staging: the token (unless utcp has it), then review and promote.
pub(crate) fn next_steps(token_set: bool) -> Vec<String> {
    let mut steps = Vec::new();
    if !token_set {
        steps.push(format!(
            "Give utcp a GitHub token as the secret {PAT_VAR} (README: \"The token\"), in one of two ways:\n     \
             - add the line {PAT_VAR}=<the token> to utcp's .env file (utcp doctor: \"Secrets File\")\n     \
             - or, if utcp doctor shows a \"Secrets Command\", store it where that command reads secrets (if the\n       \
             command has an allowlist, add {PAT_VAR} to it)\n     \
             - either way, then run: utcp daemon reload-env"
        ));
        steps.push(format!(
            "Check it with: utcp doctor   (it should show [SET] {PAT_VAR})"
        ));
    }
    steps.extend([
        "See what changes for your agents: utcp diff 'github_*'".to_string(),
        "Try a staged tool: utcp call --staged github_users.get_authenticated_user".to_string(),
        "Make them active for your agents: utcp promote 'github_*'".to_string(),
    ]);
    steps
}

fn cmd_pins(args: PinsArgs) -> Result<i32, Fail> {
    let current = Pins::load(args.pins.exists().then_some(args.pins.as_path()))?;
    let api_version = args
        .api_version
        .unwrap_or_else(|| current.api_version.clone());
    let mut transport = ureq_transport();
    let pins = source::discover_pins(&mut transport, &current, &args.commit, &api_version)?;
    write_json(&args.pins, &pins.to_json()).map_err(io_fail(&args.pins))?;
    println!(
        "Pinned {} at {} ({}), SHA-256 {}, in {}.",
        pins.file,
        pins.commit,
        pins.commit_date,
        pins.sha256,
        args.pins.display()
    );
    if api_version != current.api_version {
        println!(
            "The API version changed: update config::API_VERSION and the version in Cargo.toml."
        );
    }
    Ok(0)
}

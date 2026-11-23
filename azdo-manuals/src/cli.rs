//! Command line: `azdo-manuals generate --org <org> [--out <dir>] [--stage]`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, CommandFactory, Parser, Subcommand};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::archive::{self, Archive, ArchiveError};
use crate::config::{self, Config};
use crate::generate::{self, Specs};
use crate::http::ureq_transport;
use crate::pat;
use crate::readtest::{self, ReadTest};
use crate::routes;
use crate::sdk;
use crate::sdkops;
use crate::text::{write_atomic, write_json};
use crate::utcp::Utcp;
use crate::writetest::{self, WriteTest};

const PAT_VAR: &str = generate::PAT_VAR;

const HELP_TEMPLATE: &str =
    "{about-with-newline}\n{before-help}{usage-heading} {usage}\n\n{all-args}{after-help}";

const GETTING_STARTED: &str = "\
Getting started (once per user and organization):
  1. azdo-manuals generate --org <your-org> --stage
     Writes the manuals, stages them in utcp (agents do not see them yet) and lists the PAT permissions they need.
  2. Make a PAT with those permissions, plus Analytics: Read, at https://dev.azure.com/<your-org>/_usersSettings/tokens
  3. Give the PAT to utcp as the secret AZURE_DEVOPS_EXT_PAT (generate says how), then check it with: utcp doctor
  4. Run step 1 again with the PAT in AZURE_DEVOPS_EXT_PAT: it adds the endpoints only Microsoft's client libraries
     describe, and the Analytics manual
  5. Test the staged manuals (read-only): azdo-manuals read-test --org <your-org> --project <a project>
  6. Make them active for your agents: utcp promote
generate prints these steps again when it is done. After upgrading azdo-manuals, run step 4 again, check what
changes with utcp diff, then steps 5 and 6.";

const HELP_EXAMPLES: &str = "\
Examples:
  azdo-manuals help                prints this message
  azdo-manuals help generate       prints the help for the generate command
  azdo-manuals generate --help     the same";

const GENERATE_LONG: &str = "\
Write the manuals for an organization and list the PAT permissions they need.

Downloads Microsoft's REST API descriptions on the first run (cached afterwards) and writes one utcp manual per area
of Azure DevOps, plus scopes.md (the PAT permissions) and report.json.

With a PAT in AZURE_DEVOPS_EXT_PAT, generate also reads the organization's live route list and adds the endpoints
that only Microsoft's client libraries describe (several hundred more tools, downloaded from GitHub on the first run),
and writes the Analytics manual from the Analytics service's OData description. Without a PAT the manuals hold what
the REST API descriptions have; generate then says how to get the rest.

With --stage the manuals go to utcp's staging area, where agents do not see them yet: review them with utcp diff,
test them with azdo-manuals read-test, then make them active with utcp promote.

Example (writes, stages, and prints what to do next):
  azdo-manuals generate --org <your-org> --stage

Run it again to update the manuals, for example after upgrading azdo-manuals.";

const READ_TEST_LONG: &str = "\
Call every read tool through utcp, as agents do, to check the manuals and the PAT (read-only).

Uses your utcp (its daemon when it runs) and the PAT utcp already has, from its secrets command or .env, so no PAT
is needed in this shell. Manuals that are staged are called with utcp call --staged, the others as they are active.
The tool definitions come from the folder generate wrote. Ids are chained: a tool that needs a pull request id gets
one from the list of pull requests. Results go to readtest.json in that folder (it names your organization's data).

Example:
  azdo-manuals read-test --org <your-org> --project <a project> --seed repositoryId=<a repository>";

const WRITE_TEST_LONG: &str = "\
Maintainers, sandbox organization only: call every write tool through utcp in a throwaway project.

Runs in its own throwaway utcp setup (a temporary folder; never your manuals, daemon or agent skills) and reads the
PAT for the sandbox organization from the environment variable AZURE_DEVOPS_EXT_PAT.";

const ROUTE_GAPS_LONG: &str = "\
Maintainers: list the organization's live endpoints that no tool covers, and why.

Reads the live route list and the client libraries like generate does (needs the PAT in AZURE_DEVOPS_EXT_PAT), then
sorts every endpoint (route GUID) into: has a tool; left out by a rule (with the reason); or described neither by the
REST API specs nor by a client library. The last group is the list to look through for endpoints worth adding.
Writes route-gaps.json next to the manuals (it names the organization's routes; keep it private).";

/// Generate utcp HTTP manuals for the Azure DevOps REST API from Microsoft's archived specs.
#[derive(Parser)]
#[command(
    name = "azdo-manuals",
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
    /// Write the manuals for an organization and list the PAT permissions they need
    #[command(long_about = GENERATE_LONG)]
    Generate(GenerateArgs),
    /// Check the manuals against the organization's live route list (read-only; needs the PAT)
    ///
    /// Reads the PAT from the environment variable AZURE_DEVOPS_EXT_PAT.
    CheckRoutes(CheckRoutesArgs),
    /// Call every read tool through utcp to check the manuals and the PAT (read-only; utcp supplies the PAT)
    #[command(long_about = READ_TEST_LONG)]
    ReadTest(ReadTestArgs),
    /// Maintainers: record the spec file hashes of an archive revision in a pins file
    Pins(PinsArgs),
    /// Maintainers: record the client library file hashes of a commit in a pins file
    PinsSdk(PinsSdkArgs),
    /// Maintainers: list live endpoints no tool covers, and why (read-only; needs the PAT)
    #[command(long_about = ROUTE_GAPS_LONG)]
    RouteGaps(RouteGapsArgs),
    /// Maintainers, sandbox organization only: call every write tool in a throwaway project (needs the PAT)
    #[command(long_about = WRITE_TEST_LONG)]
    WriteTest(WriteTestArgs),
    /// Print this message, or the help for one command
    Help {
        /// The command to print the help for
        command: Option<String>,
    },
}

#[derive(Args)]
struct GenerateArgs {
    /// Azure DevOps organization name, as in https://dev.azure.com/<org>
    #[arg(long)]
    org: String,
    /// Folder for the manuals; keep it private, they name your organization [default:
    /// ~/.local/share/azdo-manuals/<org>, on Windows %LOCALAPPDATA%\azdo-manuals\manuals\<org>]
    #[arg(long)]
    out: Option<PathBuf>,
    /// Folder for the downloaded API descriptions [default: ~/.cache/azdo-manuals, on Windows
    /// %LOCALAPPDATA%\azdo-manuals\cache]
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Also stage every manual in utcp (utcp stage), so you can review and test them before agents see them
    #[arg(long)]
    stage: bool,
    /// utcp binary for --stage [default: utcp on PATH]
    #[arg(long)]
    utcp: Option<PathBuf>,
    /// Leave out the write tools, for a read-only PAT
    #[arg(long)]
    read_only: bool,
    /// Only what the REST API specs describe, even when AZURE_DEVOPS_EXT_PAT is set
    #[arg(long)]
    spec_only: bool,
    #[arg(long, hide = true)]
    pins: Option<PathBuf>,
}

#[derive(Args)]
struct PinsSdkArgs {
    /// Commit of microsoft/azure-devops-go-api [default: the head of its dev branch]
    #[arg(long)]
    go_commit: Option<String>,
    /// Commit of microsoft/azure-devops-node-api [default: the head of its master branch]
    #[arg(long)]
    node_commit: Option<String>,
    /// Pins file to update (rebuild the binary to use it; it embeds azdo-manuals/pins.json)
    #[arg(long, default_value = "pins.json")]
    pins: PathBuf,
}

#[derive(Args)]
struct RouteGapsArgs {
    /// Azure DevOps organization name, as in https://dev.azure.com/<org>
    #[arg(long)]
    org: String,
    /// Folder for route-gaps.json [default: generate's default folder for --org]
    #[arg(long)]
    out: Option<PathBuf>,
    /// Folder for the downloaded API descriptions [default: as for generate]
    #[arg(long)]
    cache: Option<PathBuf>,
}

#[derive(Args)]
struct PinsArgs {
    #[arg(long, default_value = config::ARCHIVE_REVISION)]
    revision: String,
    #[arg(long, default_value = config::API_VERSION)]
    api_version: String,
    /// Ignore the existing pins file instead of continuing it
    #[arg(long)]
    fresh: bool,
    /// Pins file to write (rebuild the binary to use it; it embeds azdo-manuals/pins.json)
    #[arg(long, default_value = "pins.json")]
    pins: PathBuf,
}

#[derive(Args)]
struct CheckRoutesArgs {
    /// Azure DevOps organization name, as in https://dev.azure.com/<org>
    #[arg(long)]
    org: String,
    /// Folder written by generate [default: generate's default folder for --org]
    #[arg(long)]
    manuals: Option<PathBuf>,
}

#[derive(Args)]
struct ReadTestArgs {
    /// Azure DevOps organization name; finds the folder generate wrote for it
    #[arg(long, required_unless_present = "manuals")]
    org: Option<String>,
    /// Folder written by generate, when it is not the default one (generate --out)
    #[arg(long)]
    manuals: Option<PathBuf>,
    /// Project to read from
    #[arg(long)]
    project: String,
    /// Team for board and iteration tools [default: the project's default team]
    #[arg(long)]
    team: Option<String>,
    /// Fix a parameter by name: name=value (repeatable), e.g. repositoryId=<a repository> to read that one's
    /// pull requests and commits
    #[arg(long)]
    seed: Vec<String>,
    /// Test only this manual (repeatable)
    #[arg(long)]
    only: Vec<String>,
    /// utcp binary [default: utcp on PATH]
    #[arg(long)]
    utcp: Option<PathBuf>,
}

#[derive(Args)]
struct WriteTestArgs {
    /// A sandbox organization of your own, never a work organization
    #[arg(long)]
    org: String,
    /// Folder written by generate (without --read-only)
    #[arg(long)]
    manuals: PathBuf,
    /// Required: the test creates a project and deletes every project named azdo-manuals-test-*
    #[arg(long)]
    create_and_delete_projects: bool,
    /// utcp binary [default: utcp on PATH]
    #[arg(long)]
    utcp: Option<PathBuf>,
}

/// A failure reported as `azdo-manuals: <message>` with the given exit code.
struct Fail(i32, String);

impl From<ArchiveError> for Fail {
    fn from(e: ArchiveError) -> Fail {
        Fail(1, e.0)
    }
}

impl From<generate::GenerateError> for Fail {
    fn from(e: generate::GenerateError) -> Fail {
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
        Cmd::CheckRoutes(a) => cmd_check_routes(a),
        Cmd::ReadTest(a) => cmd_read_test(a),
        Cmd::Pins(a) => cmd_pins(a),
        Cmd::PinsSdk(a) => cmd_pins_sdk(a),
        Cmd::RouteGaps(a) => cmd_route_gaps(a),
        Cmd::WriteTest(a) => cmd_write_test(a),
        Cmd::Help { command } => help(command.as_deref()).exit(),
    };
    match result {
        Ok(code) => code,
        Err(Fail(code, msg)) => {
            eprintln!("azdo-manuals: {msg}");
            code
        }
    }
}

/// `azdo-manuals <command> help` means `azdo-manuals help <command>` (no command takes a positional argument).
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
    let args = ["azdo-manuals"]
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
        format!("unknown command '{name}' (azdo-manuals help lists the commands)"),
    )
}

fn load_specs(
    conf: &Config,
    pins: &Value,
    cache_dir: &Path,
    archive: &mut Archive,
) -> Result<Specs, ArchiveError> {
    let mut specs = Specs::new();
    for area in conf.all_areas() {
        for pin in pins["areas"][&area]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let spec = archive::load_spec(&archive::fetch_file(archive, pin, cache_dir)?)?;
            specs.entry(area.clone()).or_default().push(spec);
        }
    }
    if archive.requests > 0 {
        println!(
            "Downloaded {} file(s) from the archive (cache: {}).",
            archive.requests,
            cache_dir.display()
        );
    }
    Ok(specs)
}

pub fn scopes_markdown(report: &Value, org: &str) -> String {
    let revision = report["source"]["revision"].as_str().unwrap_or("");
    let mut lines = vec![
        format!("# PAT permissions for the azdo manuals ({org})"),
        String::new(),
        format!(
            "REST API {}, specs from archive revision {}.",
            report["api_version"].as_str().unwrap_or(""),
            revision.chars().take(12).collect::<String>()
        ),
        String::new(),
        format!(
            "Create the token at {}: under Scopes pick \"Custom defined\", then \"Show all scopes\", and set these \
             permissions:",
            tokens_url(org)
        ),
        String::new(),
        "| Permission on the token page | Scope | What it allows |".into(),
        "| --- | --- | --- |".into(),
    ];
    for s in report["scopes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let scope = s["scope"].as_str().unwrap_or("");
        let page = pat::permission(scope)
            .map_or_else(|| "(not known)".to_string(), |(n, l)| format!("{n}: {l}"));
        lines.push(format!(
            "| {page} | `{scope}` | {} |",
            s["description"].as_str().unwrap_or("")
        ));
    }
    lines.push(String::new());
    let reads = scope_ids(&report["read_scopes"]);
    if reads != scope_ids(&report["scopes"]) {
        let rows: Vec<String> = pat::rows(reads)
            .into_iter()
            .map(|(n, l)| if l.is_empty() { n } else { format!("{n}: {l}") })
            .collect();
        lines.push(format!(
            "For a PAT that only reads (the write tools then answer 401): {}.",
            rows.join(", ")
        ));
        lines.push(String::new());
    }
    lines.push("| Manual | Tools | Approx. tokens if fully loaded | Permissions |".into());
    lines.push("| --- | --- | --- | --- |".into());
    for (name, m) in report["manuals"].as_object().into_iter().flatten() {
        lines.push(format!(
            "| `{name}` | {} | {} | {} |",
            m["tools"],
            m["approx_tokens"],
            labels(&m["scopes"])
        ));
    }
    lines.join("\n") + "\n"
}

fn tokens_url(org: &str) -> String {
    format!("https://dev.azure.com/{org}/_usersSettings/tokens")
}

fn str_list(list: &Value) -> Vec<&str> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn joined(list: &Value) -> String {
    str_list(list).join(", ")
}

fn labels(scopes: &Value) -> String {
    let labels: Vec<String> = str_list(scopes).into_iter().map(pat::label).collect();
    labels.join(", ")
}

/// The permissions block of `generate`: one line per permission, sorted as on the token page, with the level every
/// tool needs and (unless `reads` is `None`) the level the read tools need.
pub(crate) fn permission_lines<'a>(
    scopes: impl IntoIterator<Item = &'a str>,
    reads: Option<Vec<&'a str>>,
) -> Vec<String> {
    let Some(reads) = reads else {
        let rows = pat::rows(scopes);
        let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        return rows
            .iter()
            .map(|(name, level)| format!("  {name:width$}  {level}").trim_end().to_string())
            .collect();
    };
    let mut rows = vec![(
        "Permission".to_string(),
        "All tools".to_string(),
        "Reads only".to_string(),
    )];
    rows.extend(pat::table(scopes, reads));
    let w1 = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let w2 = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(name, all, read)| {
            format!("  {name:w1$}  {all:w2$}  {read}")
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Scope ids of a report list (`scopes`, `read_scopes`).
fn scope_ids(list: &Value) -> Vec<&str> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["scope"].as_str())
        .collect()
}

/// What `generate` left out and why, one short line each: areas, then operations grouped by reason.
pub(crate) fn left_out_lines(report: &Value, missing: &[(String, String)]) -> Vec<String> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for (area, has) in missing {
        rows.push((
            format!("area {area}"),
            format!(
                "no {} description (has {has})",
                report["api_version"].as_str().unwrap_or("")
            ),
        ));
    }
    for x in report["excluded_areas"].as_array().into_iter().flatten() {
        rows.push((
            format!("area {}", x["area"].as_str().unwrap_or("")),
            x["reason"].as_str().unwrap_or("").to_string(),
        ));
    }
    let groups: [(&str, &str, &str); 4] = [
        (
            "DELETE not selected",
            "DELETE operations",
            "deleting cannot be undone (the small deletes are included)",
        ),
        (
            "HEAD never included",
            "HEAD operations",
            "the GET of the same thing is included",
        ),
        ("write (--read-only)", "writes", "--read-only"),
        (
            "read needs a write scope",
            "reads",
            "only a write permission allows them (--read-only)",
        ),
    ];
    let left_out = report["left_out"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for (prefix, what, why) in groups {
        let n = left_out
            .iter()
            .filter(|x| x["reason"].as_str().is_some_and(|r| r.starts_with(prefix)))
            .count();
        if n > 0 {
            rows.push((format!("{n} {what}"), why.to_string()));
        }
    }
    // Client-library operations left out before generation (no manual), by reason.
    let mut by_reason: BTreeMap<&str, usize> = BTreeMap::new();
    for x in left_out.iter().filter(|x| x["manual"].is_null()) {
        *by_reason
            .entry(x["reason"].as_str().unwrap_or(""))
            .or_default() += 1;
    }
    for (why, n) in by_reason {
        rows.push((format!("{n} library operations"), why.to_string()));
    }
    let width = rows.iter().map(|(a, _)| a.len()).max().unwrap_or(0);
    let mut lines: Vec<String> = rows
        .iter()
        .map(|(what, why)| format!("  {what:width$}  {why}"))
        .collect();
    let broken: Vec<(String, String)> = left_out
        .iter()
        .filter_map(|x| {
            let reason = x["reason"].as_str()?.strip_prefix("skipped: ")?;
            Some((
                reason.to_string(),
                format!(
                    "{} {}",
                    x["area"].as_str().unwrap_or(""),
                    x["operation_id"].as_str().unwrap_or("")
                ),
            ))
        })
        .collect();
    if !broken.is_empty() {
        lines.push(format!("  {} operations that do not work:", broken.len()));
        let mut by_reason: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (reason, op) in &broken {
            by_reason.entry(reason).or_default().push(op);
        }
        for (reason, mut ops) in by_reason {
            ops.sort();
            if ops.len() <= 3 {
                lines.extend(ops.iter().map(|op| format!("    {op}: {reason}")));
            } else {
                lines.push(format!(
                    "    {} operations: {reason} (listed in report.json)",
                    ops.len()
                ));
            }
        }
    }
    lines
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
    given
        .or_else(|| which("utcp"))
        .ok_or_else(|| Fail(2, "no utcp binary (use --utcp)".into()))
}

/// Everything besides the spec, for `generate` and `route-gaps`.
struct Loaded {
    extra: generate::Extra,
    live: crate::live::Live,
    joined: sdkops::Joined,
    coverage: sdkops::Coverage,
}

/// Read the live routes, the client libraries and the Analytics metadata, and join them to the specs.
fn load_extra(
    conf: &Config,
    specs: &Specs,
    pins: &Value,
    cache: &Path,
    org: &str,
    pat: &str,
    api_version: &str,
) -> Result<Loaded, Fail> {
    let mut transport = ureq_transport();
    let live = crate::live::fetch(&mut transport, org, pat).map_err(|e| {
        Fail(
            1,
            format!("{e}; check {PAT_VAR}, or run with --spec-only (the client-library tools then are left out)"),
        )
    })?;
    let mut parsed = Vec::new();
    let mut downloaded = Vec::new();
    for lib in sdk::Sdk::all() {
        let pin = sdk::pin_of(pins, lib)
            .ok_or_else(|| Fail(1, format!("pins.json has no sdk.{} entry", lib.key())))?;
        let (files, fetched) = sdk::load_files(&mut transport, pin, cache)?;
        if fetched {
            downloaded.push(pin["repo"].as_str().unwrap_or("").to_string());
        }
        parsed.push(match lib {
            sdk::Sdk::Go => crate::sdk_go::parse(&files),
            sdk::Sdk::Node => crate::sdk_node::parse(&files),
        });
    }
    if !downloaded.is_empty() {
        println!(
            "Downloaded the client libraries from GitHub ({}; cache: {}).",
            downloaded.join(", "),
            cache.display()
        );
    }
    let libs: Vec<&sdk::Parsed> = parsed.iter().collect();
    let (joined, coverage) = sdkops::join(specs, &libs, &live, conf, api_version);
    let mut extra = generate::Extra {
        specs: joined.specs.clone(),
        live: true,
        left_out: joined.left_out.clone(),
        ..generate::Extra::default()
    };
    let analytics = match crate::analytics::fetch_metadata(&mut transport, org, pat)
        .and_then(|csdl| crate::analytics::entries(&csdl, org))
    {
        Ok(entries) => {
            let n = entries.len();
            extra.entries.extend(
                entries
                    .into_iter()
                    .map(|e| (crate::analytics::MANUAL.to_string(), e)),
            );
            extra.scope_descriptions.insert(
                crate::analytics::SCOPE.into(),
                crate::analytics::SCOPE_DESCRIPTION.into(),
            );
            json!({"odata_version": crate::analytics::ODATA_VERSION, "entity_sets": n})
        }
        Err(e) => {
            println!("Analytics left out: {}", e.replace(org, "<org>"));
            json!({"odata_version": crate::analytics::ODATA_VERSION, "error": e.replace(org, "<org>")})
        }
    };
    let hosts: Map<String, Value> = live
        .hosts
        .iter()
        .map(|(h, r)| {
            let v = match r {
                Ok(n) => json!(n),
                Err(e) => json!(e),
            };
            (h.clone(), v)
        })
        .collect();
    let libraries: Map<String, Value> = sdk::Sdk::all()
        .iter()
        .filter_map(|lib| {
            let pin = sdk::pin_of(pins, *lib)?;
            Some((
                lib.key().to_string(),
                json!({"repo": pin["repo"], "commit": pin["commit"]}),
            ))
        })
        .collect();
    extra.report = json!({
        "client_libraries": libraries,
        "route_hosts": hosts,
        "routes": live.routes.len(),
        "library_operations": joined.specs.values().map(Vec::len).sum::<usize>(),
        "library_methods_not_on_organization": joined.not_on_organization,
        "analytics": analytics,
    });
    Ok(Loaded {
        extra,
        live,
        joined,
        coverage,
    })
}

fn cmd_generate(args: GenerateArgs) -> Result<i32, Fail> {
    let conf = if args.read_only {
        Config::real().read_only()
    } else {
        Config::real()
    };
    if !generate::valid_org(&args.org) {
        return Err(Fail(
            1,
            format!(
                "'{}' is not a valid Azure DevOps organization name",
                args.org
            ),
        ));
    }
    let utcp = if args.stage {
        Some(find_utcp(args.utcp)?)
    } else {
        None
    };
    let pins = archive::load_pins(args.pins.as_deref())?;
    let cache = args.cache.unwrap_or_else(archive::default_cache_dir);
    let mut archive = Archive::new(archive::API, ureq_transport());
    let specs = load_specs(&conf, &pins, &cache, &mut archive)?;
    let source = json!({
        "revision": pins["revision"],
        "revision_date": pins["revision_date"],
        "revision_message": pins["revision_message"],
    });
    let api_version = pins["api_version"].as_str().unwrap_or(config::API_VERSION);
    let pat = std::env::var(PAT_VAR).ok().filter(|p| !p.is_empty());
    let extra = match &pat {
        Some(pat) if !args.spec_only => {
            load_extra(&conf, &specs, &pins, &cache, &args.org, pat, api_version)?.extra
        }
        _ => generate::Extra::default(),
    };
    let (manuals, report) =
        generate::generate_with(&specs, &extra, &args.org, api_version, &source, &conf)?;
    let out_given = args.out.is_some();
    let out = args
        .out
        .unwrap_or_else(|| archive::default_out_dir(&args.org));
    fs::create_dir_all(&out).map_err(io_fail(&out))?;
    for (name, manual) in &manuals {
        let path = out.join(format!("{name}.utcp.json"));
        write_json(&path, manual).map_err(io_fail(&path))?;
    }
    let path = out.join("report.json");
    write_json(&path, &report).map_err(io_fail(&path))?;
    let path = out.join("scopes.md");
    write_atomic(&path, scopes_markdown(&report, &args.org).as_bytes()).map_err(io_fail(&path))?;
    println!("Wrote {} manuals to {}:", manuals.len(), out.display());
    let width = report["manuals"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0);
    for (name, m) in report["manuals"].as_object().into_iter().flatten() {
        println!(
            "  {name:width$}  {:>4} tools  {}",
            m["tools"].to_string(),
            m["hint"].as_str().unwrap_or("")
        );
    }
    let areas = conf.all_areas();
    let missing: Vec<(String, String)> = pins["missing"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(area, _)| areas.contains(area))
        .map(|(area, versions)| (area.clone(), joined(versions)))
        .collect();
    println!("Left out (details in report.json in that folder):");
    for line in left_out_lines(&report, &missing) {
        println!("{line}");
    }
    if extra.live {
        let mut by_source: BTreeMap<String, u64> = BTreeMap::new();
        for m in report["manuals"].as_object().into_iter().flatten() {
            for (k, n) in m.1["sources"].as_object().into_iter().flatten() {
                *by_source.entry(k.clone()).or_default() += n.as_u64().unwrap_or(0);
            }
        }
        let libs = by_source.get("go").unwrap_or(&0) + by_source.get("node").unwrap_or(&0);
        println!(
            "Of the tools, {libs} come from Microsoft's client libraries (endpoints the REST API descriptions leave \
             out) and {} from the Analytics OData description.",
            by_source.get("odata").unwrap_or(&0)
        );
    } else if args.spec_only {
        println!("Only what the REST API descriptions have (--spec-only).");
    } else {
        println!(
            "Only what the REST API descriptions have: with your PAT in {PAT_VAR}, run generate again to add the \
             endpoints only Microsoft's client libraries describe and the Analytics manual (see azdo-manuals help \
             generate)."
        );
    }
    let staged = match utcp {
        None => false,
        Some(utcp) => {
            for (name, m) in report["manuals"].as_object().into_iter().flatten() {
                let file = out.join(format!("{name}.utcp.json"));
                let hint = m["hint"].as_str().unwrap_or("");
                let output = Command::new(&utcp)
                    .args(["stage", name])
                    .arg(&file)
                    .args(["--hint", hint])
                    .output()
                    .map_err(|e| Fail(1, format!("cannot run {}: {e}", utcp.display())))?;
                if !output.status.success() {
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                    return Err(Fail(
                        1,
                        format!("utcp stage {name} failed (output above; utcp 0.2.0 or newer is needed)"),
                    ));
                }
            }
            println!(
                "Staged the {} manuals in utcp. Agents do not see them until you promote them (step 6).",
                manuals.len()
            );
            true
        }
    };
    let needed = scope_ids(&report["scopes"]);
    println!();
    if args.read_only {
        println!("The PAT needs these permissions (also in scopes.md in that folder):");
        for line in permission_lines(needed, None) {
            println!("{line}");
        }
    } else {
        println!("The PAT needs these permissions (also in scopes.md in that folder). With the levels under \"All tools\"");
        println!("agents can use every tool; with those under \"Reads only\" (\"-\": leave it unset) they can only read,");
        println!("and a write answers 401. A PAT never allows more than your own account may do.");
        for line in permission_lines(needed, Some(scope_ids(&report["read_scopes"]))) {
            println!("{line}");
        }
    }
    println!();
    println!("Next:");
    let mut steps = Vec::new();
    if !staged {
        steps.push(format!(
            "Stage the manuals in utcp: azdo-manuals generate --org {} --stage{}",
            args.org,
            if out_given {
                format!(" --out {}", out.display())
            } else {
                String::new()
            }
        ));
    }
    steps.extend(next_steps(&args.org, out_given.then_some(out.as_path())));
    for (i, step) in steps.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
    Ok(0)
}

/// The steps after staging: the PAT, then review, test and promote.
pub(crate) fn next_steps(org: &str, out: Option<&Path>) -> Vec<String> {
    let folder = match out {
        Some(out) => format!("--manuals {}", out.display()),
        None => format!("--org {org}"),
    };
    vec![
        format!(
            "Make a PAT with the permissions above at {}\n     (under Scopes pick \"Custom defined\", then \"Show all \
             scopes\"). If you already have one, check that it has them.",
            tokens_url(org)
        ),
        format!(
            "Give the PAT to utcp as the secret {PAT_VAR}, in one of two ways:\n     \
             - add the line {PAT_VAR}=<the token> to utcp's .env file (utcp doctor: \"Secrets File\")\n     \
             - or, if utcp doctor shows a \"Secrets Command\", store it where that command reads secrets (if the\n       \
             command has an allowlist, add {PAT_VAR} to it)\n     \
             - either way, then run: utcp daemon reload-env"
        ),
        format!("Check it with: utcp doctor   (it should show [SET] {PAT_VAR})"),
        "See what changes for your agents: utcp diff".to_string(),
        format!("Test the staged manuals (read-only): azdo-manuals read-test {folder} --project <a project>"),
        "Make them active for your agents: utcp promote".to_string(),
    ]
}

fn cmd_pins(args: PinsArgs) -> Result<i32, Fail> {
    let conf = Config::real();
    let existing = if args.pins.exists() && !args.fresh {
        Some(archive::load_pins(Some(&args.pins))?)
    } else {
        None
    };
    // The client-library pins (pins-sdk) are kept whatever happens to the spec pins.
    let sdk_pins = if args.pins.exists() {
        archive::load_pins(Some(&args.pins))?.get("sdk").cloned()
    } else {
        None
    };
    let mut archive = Archive::new(archive::API, ureq_transport());
    let path = args.pins.clone();
    let mut save = |result: &Value| {
        let mut result = result.clone();
        if let (Some(sdk), Some(obj)) = (&sdk_pins, result.as_object_mut()) {
            obj.insert("sdk".into(), sdk.clone());
        }
        write_json(&path, &result)
            .map_err(|e| ArchiveError(format!("cannot write {}: {e}", path.display())))
    };
    let mut log = |line: &str| println!("{line}");
    let result = archive::discover_pins(
        &mut archive,
        &args.revision,
        &args.api_version,
        &conf.all_areas(),
        existing,
        &mut log,
        &mut save,
    );
    let remaining = archive.remaining.as_deref().unwrap_or("unknown");
    println!(
        "{} archive request(s), {remaining} left this hour.",
        archive.requests
    );
    save(&result?)?;
    println!("Wrote {}.", args.pins.display());
    Ok(0)
}

fn read_manuals(folder: &Path) -> Result<IndexMap<String, Value>, Fail> {
    let entries = fs::read_dir(folder).map_err(io_fail(folder))?;
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.ends_with(".utcp.json") && n.len() > ".utcp.json".len())
        .collect();
    names.sort();
    let mut manuals = IndexMap::new();
    for file in names {
        let path = folder.join(&file);
        let text = fs::read_to_string(&path).map_err(io_fail(&path))?;
        let manual: Value = serde_json::from_str(&text)
            .map_err(|e| Fail(1, format!("{}: not valid JSON ({e})", path.display())))?;
        manuals.insert(file.trim_end_matches(".utcp.json").to_string(), manual);
    }
    Ok(manuals)
}

fn require_pat() -> Result<String, Fail> {
    match std::env::var(PAT_VAR) {
        Ok(pat) if !pat.is_empty() => Ok(pat),
        _ => Err(Fail(
            2,
            format!("set {PAT_VAR} to a PAT for the organization"),
        )),
    }
}

fn cmd_check_routes(args: CheckRoutesArgs) -> Result<i32, Fail> {
    let pat = require_pat()?;
    let folder = args
        .manuals
        .unwrap_or_else(|| archive::default_out_dir(&args.org));
    let manuals = read_manuals(&folder)?;
    if manuals.is_empty() {
        return Err(Fail(2, format!("no *.utcp.json in {}", folder.display())));
    }
    let mut transport = ureq_transport();
    let mut fetch = |base: &str| routes::fetch_routes(&mut transport, base, &pat);
    let result = routes::check(&manuals, &args.org, &mut fetch);
    let total: usize = manuals
        .values()
        .map(|m| m["tools"].as_array().map_or(0, Vec::len))
        .sum();
    let bases: Vec<String> = result
        .bases
        .iter()
        .map(|(b, n)| format!("{b} ({n} routes)"))
        .collect();
    println!("Route lists: {}", bases.join(", "));
    for (base, e) in &result.base_errors {
        println!("Could not read the route list of {base}: {e}");
    }
    println!(
        "{} of {total} tools match a live route at a supported version.",
        result.matched
    );
    for name in &result.unmatched {
        println!("  no route:    {name}");
    }
    for line in &result.version {
        println!("  version:     {line}");
    }
    for name in &result.unchecked {
        println!("  not checked: {name}");
    }
    if !result.no_route_list.is_empty() {
        println!(
            "Not checked, because their host publishes no route list (read-test calls them instead): {}",
            result.no_route_list.join(", ")
        );
    }
    if !result.odata.is_empty() {
        println!(
            "Not checked: {} Analytics OData tools (OData has no route list; read-test calls them instead).",
            result.odata.len()
        );
    }
    Ok(
        if result.matched + result.no_route_list.len() + result.odata.len() == total {
            0
        } else {
            1
        },
    )
}

fn cmd_read_test(args: ReadTestArgs) -> Result<i32, Fail> {
    let utcp = Utcp::shared(find_utcp(args.utcp)?);
    let folder = match (args.manuals, &args.org) {
        (Some(folder), _) => folder,
        (None, Some(org)) => archive::default_out_dir(org),
        (None, None) => unreachable!("clap requires --org without --manuals"),
    };
    let mut manuals = read_manuals(&folder)?;
    if !args.only.is_empty() {
        manuals.retain(|name, _| args.only.contains(name));
    }
    if manuals.is_empty() {
        return Err(Fail(
            2,
            format!(
                "no manuals to test in {} (run azdo-manuals generate first)",
                folder.display()
            ),
        ));
    }
    let staged_in_utcp = utcp.manual_names(true).map_err(|e| Fail(1, e))?;
    let active_in_utcp = utcp.manual_names(false).map_err(|e| Fail(1, e))?;
    let (staged, rest): (BTreeSet<String>, BTreeSet<String>) = manuals
        .keys()
        .cloned()
        .partition(|name| staged_in_utcp.contains(name));
    let missing: Vec<&String> = rest
        .iter()
        .filter(|n| !active_in_utcp.contains(*n))
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        return Err(Fail(
            2,
            format!(
                "utcp has neither a staged nor an active manual named {}; stage them with: azdo-manuals generate \
                 --org <your-org> --stage",
                names.join(", ")
            ),
        ));
    }
    let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
    if !staged.is_empty() {
        println!("Staged (utcp call --staged): {}", list(&staged));
    }
    if !rest.is_empty() {
        println!("Active: {}", list(&rest));
    }
    let mut seeds = Map::new();
    seeds.insert("project".into(), json!(args.project));
    if let Some(team) = args.team {
        seeds.insert("team".into(), json!(team));
    }
    for s in &args.seed {
        let (key, value) = s.split_once('=').unwrap_or((s, ""));
        seeds.insert(key.into(), json!(value));
    }
    let mut test = ReadTest::new(utcp, manuals, staged, seeds);
    let results = test.run();
    drop(test);
    let path = folder.join("readtest.json");
    write_json(&path, &Value::Object(results.clone())).map_err(io_fail(&path))?;
    let counts: Vec<String> = readtest::summary(&results)
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect();
    println!("Results: {}", counts.join(", "));
    println!(
        "Details (contains organization data, keep private): {}",
        path.display()
    );
    let failed = results
        .values()
        .any(|r| !matches!(r["status"].as_str(), Some("ok" | "not exercised")));
    Ok(if failed { 1 } else { 0 })
}

fn cmd_write_test(args: WriteTestArgs) -> Result<i32, Fail> {
    if !args.create_and_delete_projects {
        return Err(Fail(
            2,
            format!(
                "write-test creates a project in '{}' and deletes every project named {}*; run it only in a sandbox \
                 organization of your own, with --create-and-delete-projects",
                args.org,
                writetest::PROJECT_PREFIX
            ),
        ));
    }
    if !generate::valid_org(&args.org) {
        return Err(Fail(
            1,
            format!(
                "'{}' is not a valid Azure DevOps organization name",
                args.org
            ),
        ));
    }
    let pat = require_pat()?;
    let utcp = find_utcp(args.utcp)?;
    let manuals: Vec<(String, Value)> = read_manuals(&args.manuals)?.into_iter().collect();
    let writes = manuals
        .iter()
        .flat_map(|(_, m)| m["tools"].as_array().cloned().unwrap_or_default())
        .filter(|t| !readtest::is_read(t))
        .count();
    if writes == 0 {
        return Err(Fail(
            2,
            format!(
                "no write tools in {} (generated with --read-only?)",
                args.manuals.display()
            ),
        ));
    }
    let name = format!(
        "{}{}",
        writetest::PROJECT_PREFIX,
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let mut test = WriteTest::new(utcp, manuals, ureq_transport(), &args.org, &pat)
        .map_err(|e| Fail(1, format!("temporary folder: {e}")))?;
    test.register(&args.manuals).map_err(|e| Fail(1, e))?;
    let results = test
        .run(&name)
        .map_err(|e| Fail(1, format!("setup failed: {e}")))?;
    drop(test);
    let path = args.manuals.join("writetest.json");
    write_json(&path, &Value::Object(results.clone())).map_err(io_fail(&path))?;
    let counts: Vec<String> = readtest::summary(&results)
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect();
    println!("Results: {}", counts.join(", "));
    println!(
        "Details (contains organization data, keep private): {}",
        path.display()
    );
    let failed = results.values().any(|r| r["status"] != "ok");
    Ok(if failed { 1 } else { 0 })
}

fn cmd_pins_sdk(args: PinsSdkArgs) -> Result<i32, Fail> {
    let mut pins = archive::load_pins(Some(&args.pins))?;
    let mut transport = ureq_transport();
    let mut section = Map::new();
    for (lib, commit) in [
        (sdk::Sdk::Go, args.go_commit),
        (sdk::Sdk::Node, args.node_commit),
    ] {
        let (repo, branch) = sdk::default_repo(lib);
        let commit = match commit {
            Some(c) => c,
            None => sdk::branch_commit(&mut transport, repo, branch)?,
        };
        let pin = sdk::discover(&mut transport, lib, repo, &commit)?;
        println!(
            "  {repo} {commit}: {} files",
            pin["files"].as_array().map_or(0, Vec::len)
        );
        section.insert(lib.key().into(), pin);
    }
    pins["sdk"] = Value::Object(section);
    write_json(&args.pins, &pins).map_err(io_fail(&args.pins))?;
    println!("Wrote {}.", args.pins.display());
    Ok(0)
}

fn cmd_route_gaps(args: RouteGapsArgs) -> Result<i32, Fail> {
    let pat = require_pat()?;
    if !generate::valid_org(&args.org) {
        return Err(Fail(
            1,
            format!(
                "'{}' is not a valid Azure DevOps organization name",
                args.org
            ),
        ));
    }
    let conf = Config::real();
    let pins = archive::load_pins(None)?;
    let cache = args.cache.unwrap_or_else(archive::default_cache_dir);
    let mut archive = Archive::new(archive::API, ureq_transport());
    let specs = load_specs(&conf, &pins, &cache, &mut archive)?;
    let api_version = pins["api_version"].as_str().unwrap_or(config::API_VERSION);
    let loaded = load_extra(&conf, &specs, &pins, &cache, &args.org, &pat, api_version)?;
    let source = json!({});
    let (_, report) = generate::generate_with(
        &specs,
        &loaded.extra,
        &args.org,
        api_version,
        &source,
        &conf,
    )?;
    let mut reasons: HashMap<(String, String), String> = HashMap::new();
    for x in report["left_out"].as_array().into_iter().flatten() {
        reasons.insert(
            (
                x["area"].as_str().unwrap_or("").to_string(),
                x["operation_id"].as_str().unwrap_or("").to_string(),
            ),
            x["reason"].as_str().unwrap_or("").to_string(),
        );
    }
    // GUID -> operations (area, operationId) that could make a tool.
    let mut ops: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (guid, uses) in &loaded.coverage.ops {
        for u in uses {
            ops.entry(guid.clone())
                .or_default()
                .push((u.area.clone(), u.operation_id.clone()));
        }
    }
    for (area, files) in &loaded.joined.specs {
        for spec in files {
            for item in spec["paths"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(_, i)| i)
            {
                for (_, op) in item.as_object().into_iter().flatten() {
                    let guid = op["x-azdo-manuals"]["guid"].as_str().unwrap_or("");
                    let id = op["operationId"].as_str().unwrap_or("");
                    ops.entry(guid.to_string())
                        .or_default()
                        .push((area.clone(), id.to_string()));
                }
            }
        }
    }
    for x in &loaded.joined.left_out {
        ops.entry(x["guid"].as_str().unwrap_or("").to_string())
            .or_default()
            .push((
                x["area"].as_str().unwrap_or("").to_string(),
                x["operation_id"].as_str().unwrap_or("").to_string(),
            ));
    }
    let plumbing: HashSet<&str> = conf
        .excluded_route_areas
        .iter()
        .map(|(a, _)| a.as_str())
        .collect();
    let by_guid = loaded.live.by_guid();
    let mut guids: Vec<&&str> = by_guid.keys().collect();
    guids.sort();
    let mut rows = Vec::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut undescribed: BTreeMap<String, usize> = BTreeMap::new();
    let mut rule_reasons: BTreeMap<String, usize> = BTreeMap::new();
    for guid in guids {
        let guid: &str = guid;
        let routes = &by_guid[guid];
        let r = routes[0];
        let mine = ops.get(guid).cloned().unwrap_or_default();
        let left: Vec<&String> = mine.iter().filter_map(|k| reasons.get(k)).collect();
        let status = if mine.is_empty() {
            "not described"
        } else if left.len() < mine.len() {
            "tool"
        } else {
            "left out"
        };
        *counts.entry(status).or_default() += 1;
        let area = r.area.to_lowercase();
        if status == "not described" {
            let key = if plumbing.contains(area.as_str()) {
                format!("{area} (excluded)")
            } else {
                area.clone()
            };
            *undescribed.entry(key).or_default() += 1;
        }
        if status == "left out" {
            for reason in &left {
                *rule_reasons.entry(reason.to_string()).or_default() += 1;
            }
        }
        let mut hosts: Vec<&str> = routes.iter().map(|x| x.host.as_str()).collect();
        hosts.sort();
        let libraries: Vec<&str> = loaded
            .joined
            .libraries
            .get(guid)
            .into_iter()
            .flatten()
            .map(|l| l.key())
            .collect();
        let mut why: Vec<&String> = left.clone();
        why.sort();
        why.dedup();
        rows.push(json!({
            "guid": guid, "area": r.area, "resource": r.resource, "template": r.template, "hosts": hosts,
            "released_version": r.released_version, "max_version": r.max_version, "status": status,
            "libraries": libraries, "reasons": why,
        }));
    }
    let out = args
        .out
        .unwrap_or_else(|| archive::default_out_dir(&args.org));
    fs::create_dir_all(&out).map_err(io_fail(&out))?;
    let path = out.join("route-gaps.json");
    write_json(
        &path,
        &json!({"summary": counts, "not_described_by_area": undescribed, "routes": rows}),
    )
    .map_err(io_fail(&path))?;
    let total: usize = counts.values().sum();
    let hosts = loaded.live.hosts.values().filter(|h| h.is_ok()).count();
    println!("{total} live endpoints (route GUIDs) on {hosts} hosts:");
    println!("  {:>5}  have a tool", counts.get("tool").unwrap_or(&0));
    println!(
        "  {:>5}  left out by a rule:",
        counts.get("left out").unwrap_or(&0)
    );
    for (reason, n) in &rule_reasons {
        println!("           {n:>4}  {reason}");
    }
    println!(
        "  {:>5}  described neither by the REST API specs nor by a client library, by route area:",
        counts.get("not described").unwrap_or(&0)
    );
    let mut areas: Vec<(&String, &usize)> = undescribed.iter().collect();
    areas.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (area, n) in areas {
        println!("           {n:>4}  {area}");
    }
    println!(
        "Details (names the organization's routes, keep private): {}",
        path.display()
    );
    Ok(0)
}

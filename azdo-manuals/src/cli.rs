//! Command line: `azdo-manuals generate --org <org> [--out <dir>] [--register]`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Args, Parser, Subcommand};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::archive::{self, Archive, ArchiveError};
use crate::config::{self, Config};
use crate::generate::{self, Specs};
use crate::http::ureq_transport;
use crate::readtest::{self, ReadTest};
use crate::routes;
use crate::text::{write_atomic, write_json};

const PAT_VAR: &str = generate::PAT_VAR;

/// Generate utcp HTTP manuals for the Azure DevOps REST API from Microsoft's archived specs.
#[derive(Parser)]
#[command(name = "azdo-manuals", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download the pinned specs and write the manuals and the scope report
    Generate(GenerateArgs),
    /// Maintainers: record the spec file hashes of an archive revision in a pins file
    Pins(PinsArgs),
    /// Check generated manuals against the organization's live route list (read-only; PAT from
    /// AZURE_DEVOPS_EXT_PAT)
    CheckRoutes(CheckRoutesArgs),
    /// Call every read tool through utcp, chaining ids (read-only; PAT from AZURE_DEVOPS_EXT_PAT)
    ReadTest(ReadTestArgs),
}

#[derive(Args)]
struct GenerateArgs {
    /// Azure DevOps organization name (dev.azure.com/<org>)
    #[arg(long)]
    org: String,
    /// Folder for the manuals; keep it private, they name your org [default: ~/.local/share/azdo-manuals/<org>,
    /// on Windows %LOCALAPPDATA%\azdo-manuals\manuals\<org>]
    #[arg(long)]
    out: Option<PathBuf>,
    /// Spec cache folder [default: ~/.cache/azdo-manuals, on Windows %LOCALAPPDATA%\azdo-manuals\cache]
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Also register (or re-register) every manual with utcp, with its hint
    #[arg(long)]
    register: bool,
    /// utcp binary for --register [default: utcp on PATH]
    #[arg(long)]
    utcp: Option<PathBuf>,
    #[arg(long, hide = true)]
    pins: Option<PathBuf>,
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
    #[arg(long)]
    org: String,
    /// Folder written by generate
    #[arg(long)]
    manuals: PathBuf,
}

#[derive(Args)]
struct ReadTestArgs {
    /// Folder written by generate
    #[arg(long)]
    manuals: PathBuf,
    /// Project to read from
    #[arg(long)]
    project: String,
    /// Team for board and iteration tools
    #[arg(long)]
    team: Option<String>,
    /// Fix a path parameter: name=value (repeatable)
    #[arg(long)]
    seed: Vec<String>,
    /// Test only this manual (repeatable)
    #[arg(long)]
    only: Vec<String>,
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
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Generate(a) => cmd_generate(a),
        Cmd::Pins(a) => cmd_pins(a),
        Cmd::CheckRoutes(a) => cmd_check_routes(a),
        Cmd::ReadTest(a) => cmd_read_test(a),
    };
    match result {
        Ok(code) => code,
        Err(Fail(code, msg)) => {
            eprintln!("azdo-manuals: {msg}");
            code
        }
    }
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
        format!("# PAT scopes for the azdo manuals ({org})"),
        String::new(),
        format!(
            "REST API {}, specs from archive revision {}.",
            report["api_version"].as_str().unwrap_or(""),
            revision.chars().take(12).collect::<String>()
        ),
        format!("Create the token at https://dev.azure.com/{org}/_usersSettings/tokens with these scopes:"),
        String::new(),
        "| Scope | What it allows |".into(),
        "| --- | --- |".into(),
    ];
    for s in report["scopes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        lines.push(format!(
            "| `{}` | {} |",
            s["scope"].as_str().unwrap_or(""),
            s["description"].as_str().unwrap_or("")
        ));
    }
    lines.push(String::new());
    lines.push("| Manual | Tools | Approx. tokens if fully loaded | Scopes |".into());
    lines.push("| --- | --- | --- | --- |".into());
    for (name, m) in report["manuals"].as_object().into_iter().flatten() {
        lines.push(format!(
            "| `{name}` | {} | {} | {} |",
            m["tools"],
            m["approx_tokens"],
            joined(&m["scopes"])
        ));
    }
    lines.join("\n") + "\n"
}

fn joined(list: &Value) -> String {
    let items: Vec<&str> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    items.join(", ")
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

fn cmd_generate(args: GenerateArgs) -> Result<i32, Fail> {
    let conf = Config::real();
    if !generate::valid_org(&args.org) {
        return Err(Fail(
            1,
            format!(
                "'{}' is not a valid Azure DevOps organization name",
                args.org
            ),
        ));
    }
    let utcp = if args.register {
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
    let (manuals, report) = generate::generate(&specs, &args.org, api_version, &source, &conf)?;
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
    for (name, m) in report["manuals"].as_object().into_iter().flatten() {
        println!(
            "  {name:18} {:>4} tools  ~{:>6} tokens  {}",
            m["tools"].to_string(),
            m["approx_tokens"].to_string(),
            joined(&m["scopes"])
        );
    }
    let needed: Vec<&str> = report["scopes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["scope"].as_str())
        .collect();
    println!("PAT scopes needed: {}", needed.join(", "));
    for (area, versions) in pins["missing"].as_object().into_iter().flatten() {
        println!(
            "Not included: area '{area}' has no {api_version} spec (has {}).",
            joined(versions)
        );
    }
    match utcp {
        None => println!(
            "Register with: utcp register <name> {}{}<name>.utcp.json --hint '<hint>' (see README), or run again \
             with --register.",
            out.display(),
            std::path::MAIN_SEPARATOR
        ),
        Some(utcp) => {
            for (name, m) in report["manuals"].as_object().into_iter().flatten() {
                let file = out.join(format!("{name}.utcp.json"));
                let hint = m["hint"].as_str().unwrap_or("");
                let status = Command::new(&utcp)
                    .args(["register", name])
                    .arg(&file)
                    .args(["--hint", hint])
                    .status()
                    .map_err(|e| Fail(1, format!("cannot run {}: {e}", utcp.display())))?;
                if !status.success() {
                    return Err(Fail(1, format!("utcp register {name} failed")));
                }
            }
            println!("Registered {} manuals with utcp. Calls need {PAT_VAR} (see scopes.md).", manuals.len());
        }
    }
    Ok(0)
}

fn cmd_pins(args: PinsArgs) -> Result<i32, Fail> {
    let conf = Config::real();
    let existing = if args.pins.exists() && !args.fresh {
        Some(archive::load_pins(Some(&args.pins))?)
    } else {
        None
    };
    let mut archive = Archive::new(archive::API, ureq_transport());
    let path = args.pins.clone();
    let mut save = |result: &Value| {
        write_json(&path, result)
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
    let manuals = read_manuals(&args.manuals)?;
    if manuals.is_empty() {
        return Err(Fail(
            2,
            format!("no *.utcp.json in {}", args.manuals.display()),
        ));
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
    Ok(if result.matched == total { 0 } else { 1 })
}

fn cmd_read_test(args: ReadTestArgs) -> Result<i32, Fail> {
    require_pat()?;
    let utcp = find_utcp(args.utcp)?;
    let mut manuals = read_manuals(&args.manuals)?;
    if !args.only.is_empty() {
        manuals.retain(|name, _| args.only.contains(name));
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
    let mut test = ReadTest::new(utcp, manuals, seeds)
        .map_err(|e| Fail(1, format!("temporary folder: {e}")))?;
    test.register(&args.manuals).map_err(|e| Fail(1, e))?;
    let results = test.run();
    drop(test);
    let path = args.manuals.join("readtest.json");
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

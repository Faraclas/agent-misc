use crate::catalog::UtcpRegistryConfig;
use crate::paths::{write_atomic, UtcpPaths};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SKILL_TEMPLATE: &str = include_str!("../../../assets/SKILL.template.md");
const ADMIN_SKILL_TEMPLATE: &str = include_str!("../../../assets/SKILL-admin.template.md");
/// `docs/operating.md`, installed next to the admin skill so agents read the guide matching this binary.
pub const OPERATING_GUIDE: &str = include_str!("../../../docs/operating.md");

/// Skill for finding and calling tools.
pub const TOOLS_SKILL: &str = "utcp-tools";
/// Skill for changing the setup (register, secrets, daemon, servers, debugging).
pub const ADMIN_SKILL: &str = "utcp-admin";
/// File in each skills folder's `utcp-tools/` naming the registry (`.utcp_config.json`) whose manuals the skills
/// list. Registering in another setup (a test, `UTCP_CONFIG_DIR`) then leaves these skills alone.
pub const OWNER_FILE: &str = ".utcp-registry";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillTargetPlatform {
    LinuxPosix,
    WindowsPowerShell,
}

impl SkillTargetPlatform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::WindowsPowerShell
        } else {
            Self::LinuxPosix
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdeSkillTargetStatus {
    pub client: String,
    pub path: String,
    pub exists: bool,
    pub up_to_date: bool,
}

fn format_registered_manuals_hint(manual_labels: &[String]) -> String {
    if manual_labels.is_empty() {
        "run `utcp list` to check registered manuals".to_string()
    } else {
        format!("registered manuals: {}", manual_labels.join(", "))
    }
}

/// Render `SKILL.template.md` tailored to the target OS shell, paths, and currently registered manuals
/// (one label per manual, `name` or `name (hint)` from [`registered_manual_labels`], so agents immediately
/// know `d2`, `context7 (library docs)`, `github`, etc. are ready in `utcp` without wasting turns on
/// `which` / `--help` probing, while still keeping zero tool schemas in idle context).
pub fn render_skill_markdown(platform: SkillTargetPlatform, manual_labels: &[String]) -> String {
    let hint = format_registered_manuals_hint(manual_labels);
    match platform {
        SkillTargetPlatform::LinuxPosix => {
            let call_examples = "\
The arguments are one JSON object; pass long or quoted ones on stdin:

```sh
utcp call <manual.tool> '{\"param\": \"value\"}'
utcp call <manual.tool> --args-stdin <<'EOF'
{\"param\": \"value with 'quotes'\"}
EOF
```";

            let batch_example = "\
```sh
utcp batch <<'EOF'
[{\"tool\": \"manual.tool_a\", \"args\": {\"id\": 1}}, {\"id\": \"b\", \"tool\": \"manual.tool_b\", \"args\": {}}]
EOF
```";

            let body = SKILL_TEMPLATE
                .replace("{{REGISTERED_MANUALS_HINT}}", &hint)
                .replace("{{CALL_EXAMPLES}}", call_examples)
                .replace("{{BATCH_EXAMPLE}}", batch_example);
            apply_platform(&body, platform)
        }
        SkillTargetPlatform::WindowsPowerShell => {
            let call_examples = "\
The arguments are one JSON object; pass them on stdin so PowerShell keeps the quotes:

```powershell
@'
{\"param\": \"value\"}
'@ | utcp.exe call <manual.tool> --args-stdin
```";

            let batch_example = "\
```powershell
@'
[{\"tool\": \"manual.tool_a\", \"args\": {\"id\": 1}}, {\"id\": \"b\", \"tool\": \"manual.tool_b\", \"args\": {}}]
'@ | utcp.exe batch
```";

            let body = SKILL_TEMPLATE
                .replace("```sh", "```powershell")
                .replace("{{REGISTERED_MANUALS_HINT}}", &hint)
                .replace("{{CALL_EXAMPLES}}", call_examples)
                .replace("{{BATCH_EXAMPLE}}", batch_example);
            apply_platform(&body, platform)
        }
    }
}

/// Fill the placeholders both skill templates share: program name, `.env` path, daemon endpoint,
/// and the shell of code blocks.
fn apply_platform(template: &str, platform: SkillTargetPlatform) -> String {
    match platform {
        SkillTargetPlatform::LinuxPosix => template
            .replace("{{BIN_NAME}}", "utcp")
            .replace("{{ENV_PATH}}", "~/.config/utcp/.env")
            .replace("{{SOCKET_PATH}}", "$XDG_RUNTIME_DIR/utcp.sock"),
        SkillTargetPlatform::WindowsPowerShell => template
            .replace("```sh", "```powershell")
            .replace("{{BIN_NAME}}", "utcp.exe")
            .replace("{{ENV_PATH}}", r"%APPDATA%\utcp\config\.env")
            .replace("{{SOCKET_PATH}}", r"\\.\pipe\utcp-<USERNAME>"),
    }
}

/// Render the `utcp-admin` skill (changing the setup) for the target OS.
pub fn render_admin_skill_markdown(platform: SkillTargetPlatform) -> String {
    apply_platform(ADMIN_SKILL_TEMPLATE, platform)
}

/// Every file `sync-ides` writes into one client's skills folder: its path relative to that
/// folder, and its contents.
pub fn skill_files(
    platform: SkillTargetPlatform,
    manual_labels: &[String],
) -> Vec<(PathBuf, String)> {
    vec![
        (
            Path::new(TOOLS_SKILL).join("SKILL.md"),
            render_skill_markdown(platform, manual_labels),
        ),
        (
            Path::new(ADMIN_SKILL).join("SKILL.md"),
            render_admin_skill_markdown(platform),
        ),
        (
            Path::new(ADMIN_SKILL).join("operating.md"),
            OPERATING_GUIDE.to_string(),
        ),
    ]
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Skill label for one manual: `name`, or `name (hint)` when it has a hint.
pub fn manual_label(name: &str, hint: Option<&str>) -> String {
    match hint {
        Some(h) if !h.is_empty() => format!("{} ({})", name, h),
        _ => name.to_string(),
    }
}

/// Labels of the active manuals, sorted by name, for the skill description (see [`grouped_manual_labels`]),
/// shortened by [`fit_manual_labels`] so the description stays within [`MAX_DESCRIPTION_CHARS`].
pub fn registered_manual_labels(paths: &UtcpPaths) -> Vec<String> {
    UtcpRegistryConfig::load(paths)
        .map(|cfg| {
            let entries: Vec<(String, Option<String>)> = cfg
                .manuals
                .iter()
                .map(|(name, e)| (name.clone(), e.hint.clone()))
                .collect();
            fit_manual_labels(&entries, SkillTargetPlatform::current())
        })
        .unwrap_or_default()
}

/// Longest skill description the Agent Skills format allows; clients warn about or cut a longer one.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// The front matter `description` of a rendered skill as a client reads it: the folded (`>-`) lines joined
/// with single spaces.
pub fn skill_description(md: &str) -> String {
    let front = md
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(front, _)| front)
        .unwrap_or_default();
    let mut lines = front.lines().skip_while(|l| !l.starts_with("description:"));
    let first = lines
        .next()
        .map(|l| l.trim_start_matches("description:").trim())
        .unwrap_or_default();
    let mut parts: Vec<&str> = Vec::new();
    if !first.is_empty() && first != ">-" && first != ">" {
        parts.push(first);
    }
    parts.extend(
        lines
            .take_while(|l| l.starts_with(' ') || l.is_empty())
            .map(str::trim)
            .filter(|l| !l.is_empty()),
    );
    parts.join(" ")
}

/// [`grouped_manual_labels`], shortened until the `utcp-tools` description fits in
/// [`MAX_DESCRIPTION_CHARS`]: the longest label that can still shrink takes its next form (see
/// [`manual_label_forms`]) until the description fits. When even the shortest forms are too long, the list
/// ends with `and N more (see utcp list)`.
pub fn fit_manual_labels(
    entries: &[(String, Option<String>)],
    platform: SkillTargetPlatform,
) -> Vec<String> {
    let fits = |labels: &[String]| {
        skill_description(&render_skill_markdown(platform, labels))
            .chars()
            .count()
            <= MAX_DESCRIPTION_CHARS
    };
    let forms = manual_label_forms(entries);
    let mut step = vec![0usize; forms.len()];
    loop {
        let labels: Vec<String> = forms
            .iter()
            .zip(&step)
            .map(|(f, &s)| f[s].clone())
            .collect();
        if fits(&labels) {
            return labels;
        }
        let longest = (0..forms.len())
            .filter(|&i| step[i] + 1 < forms[i].len())
            .max_by_key(|&i| (labels[i].chars().count(), std::cmp::Reverse(i)));
        match longest {
            Some(i) => step[i] += 1,
            None => {
                for keep in (0..labels.len()).rev() {
                    let mut cut = labels[..keep].to_vec();
                    let more = labels.len() - keep;
                    cut.push(format!("and {more} more (see `utcp list`)"));
                    if keep == 0 || fits(&cut) {
                        return cut;
                    }
                }
                return labels;
            }
        }
    }
}

/// Labels for the skill description, which every agent loads on every task. Manuals named
/// `<prefix>_<rest>` that share a prefix with another manual become one label: the words their hints all
/// start or end with, written once, then the member names. `azdo_admin (Azure DevOps permissions, org ni)` and
/// `azdo_git (Azure DevOps repos and PRs, org ni)` become `azdo_* (Azure DevOps, org ni: admin, git)`; each
/// member's own words are in `utcp list`. Members whose hints share no words keep their hints. Other manuals
/// get [`manual_label`]. `entries` must be sorted by name.
pub fn grouped_manual_labels(entries: &[(String, Option<String>)]) -> Vec<String> {
    manual_label_forms(entries)
        .into_iter()
        .map(|mut forms| forms.swap_remove(0))
        .collect()
}

/// For each label of [`grouped_manual_labels`], its forms from longest to shortest. A manual: `name (hint)`,
/// then `name`. A group with shared words: the shared words with the member names
/// (`azdo_* (Azure DevOps, org ni: admin, git)`), then the shared words alone (`azdo_* (Azure DevOps, org ni)`),
/// then `azdo_*`. A group without: each member with its hint, then the member names, then the prefix. The
/// member names follow the shared words because the names alone say more than the prefix does, and the
/// shared words say what the group is.
pub fn manual_label_forms(entries: &[(String, Option<String>)]) -> Vec<Vec<String>> {
    let prefix_of = |name: &str| name.split_once('_').map(|(p, _)| p.to_string());
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (name, _) in entries {
        if let Some(p) = prefix_of(name) {
            *counts.entry(p).or_default() += 1;
        }
    }
    let mut labels = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, hint) in entries {
        let group = prefix_of(name).filter(|p| counts.get(p).copied().unwrap_or(0) > 1);
        let Some(prefix) = group else {
            let mut forms = vec![manual_label(name, hint.as_deref())];
            forms.push(name.clone());
            forms.dedup();
            labels.push(forms);
            continue;
        };
        if !done.insert(prefix.clone()) {
            continue;
        }
        let members: Vec<(&str, Option<&str>)> = entries
            .iter()
            .filter(|(n, _)| prefix_of(n).as_deref() == Some(prefix.as_str()))
            .map(|(n, h)| {
                (
                    &n[prefix.len() + 1..],
                    h.as_deref().filter(|h| !h.trim().is_empty()),
                )
            })
            .collect();
        labels.push(group_label_forms(&prefix, &members));
    }
    labels
}

fn group_label_forms(prefix: &str, members: &[(&str, Option<&str>)]) -> Vec<String> {
    let words: Vec<Vec<&str>> = members
        .iter()
        .filter_map(|(_, h)| h.map(|h| h.split_whitespace().collect()))
        .collect();
    // Shared words only when every member has a hint; otherwise they would describe the ones without.
    let (lead, tail) = if words.len() == members.len() {
        let min = words.iter().map(Vec::len).min().unwrap_or(0);
        let lead = (0..min)
            .take_while(|&i| words.iter().all(|w| w[i] == words[0][i]))
            .count();
        let tail = (0..min - lead)
            .take_while(|&i| {
                let last = &words[0][words[0].len() - 1 - i];
                words.iter().all(|w| &w[w.len() - 1 - i] == last)
            })
            .count();
        (lead, tail)
    } else {
        (0, 0)
    };
    let trim = |s: String| s.trim_end_matches([',', ';', ':']).to_string();
    let shared = match (&words.first(), lead, tail) {
        (Some(w), l, t) if l + t > 0 => {
            let head = trim(w[..l].join(" "));
            let end = w[w.len() - t..].join(" ");
            [head, end]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => String::new(),
    };
    let bare: Vec<&str> = members.iter().map(|(rest, _)| *rest).collect();
    let mut forms = Vec::new();
    if shared.is_empty() {
        let parts: Vec<String> = members
            .iter()
            .map(|(rest, hint)| manual_label(rest, *hint))
            .collect();
        forms.push(format!("{prefix}_* ({})", parts.join(", ")));
        forms.push(format!("{prefix}_* ({})", bare.join(", ")));
    } else {
        forms.push(format!("{prefix}_* ({shared}: {})", bare.join(", ")));
        forms.push(format!("{prefix}_* ({shared})"));
    }
    forms.push(format!("{prefix}_*"));
    forms.dedup();
    forms
}

/// Enumerate the global and optional workspace skills root folders across Zed (`~/.agents/skills`),
/// VS Code and Copilot CLI (`~/.copilot/skills`), Antigravity CLI (`agy`), and Hermes
/// (`~/.hermes/skills`). Each folder gets the files from [`skill_files`]. VS Code's default Copilot
/// harness does not read `~/.agents/skills`; Copilot CLI reads both and lists a skill found in both
/// only once. The old Gemini CLI path (`~/.gemini/skills`) is not a target: Antigravity replaced
/// Gemini CLI and does not read it.
pub fn resolve_skill_targets(
    include_global: bool,
    workspace_dir: Option<&Path>,
) -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();
    if include_global {
        if let Some(home) = home_dir() {
            // One component per `join`, so Windows prints `\` throughout instead of `...\.agents/skills`.
            targets.push(("Zed".to_string(), home.join(".agents").join("skills")));
            targets.push((
                "VS Code + Copilot CLI".to_string(),
                home.join(".copilot").join("skills"),
            ));
            targets.push((
                "Antigravity (agy)".to_string(),
                home.join(".gemini").join("antigravity-cli").join("skills"),
            ));

            let hermes_skills_root = home.join(".hermes").join("skills");
            if hermes_skills_root.is_dir() {
                targets.push((
                    "Hermes Agent".to_string(),
                    hermes_skills_root.join("software-development"),
                ));
            }
        }
    }
    if let Some(ws) = workspace_dir {
        // Agents that open `ws` as a project (Zed, Copilot CLI, agy) also read its `.agents/skills/`.
        // VS Code's Copilot harness documents `.github/skills/` as its project folder; a root
        // `.agents/skills/` appears to load too, but that is observed, not documented.
        let root = ws.join(".agents").join("skills");
        // In the home folder this is Zed's global folder; list it once.
        let already_listed = targets
            .iter()
            .any(|(_, global)| normalized(global) == normalized(&root));
        if !already_listed {
            targets.push(("This folder (project skills)".to_string(), root));
        }
    }
    targets
}

/// `path` with its longest existing ancestor canonicalized, so two spellings of one folder
/// compare equal even before the folder itself exists.
fn normalized(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            normalized(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

/// Status of one skills root folder: `exists` when the tools skill is installed there,
/// `up_to_date` when every file from [`skill_files`] matches.
fn target_status(client: String, root: &Path, files: &[(PathBuf, String)]) -> IdeSkillTargetStatus {
    let exists = root.join(TOOLS_SKILL).join("SKILL.md").exists();
    let up_to_date = exists
        && files.iter().all(|(rel, expected)| {
            std::fs::read_to_string(root.join(rel))
                .map(|c| &c == expected)
                .unwrap_or(false)
        });
    IdeSkillTargetStatus {
        client,
        path: root.to_string_lossy().into_owned(),
        exists,
        up_to_date,
    }
}

fn write_skill_files(paths: &UtcpPaths, root: &Path, files: &[(PathBuf, String)]) -> Result<()> {
    for (rel, contents) in files {
        write_atomic(&root.join(rel), contents.as_bytes())?;
    }
    write_atomic(
        &root.join(TOOLS_SKILL).join(OWNER_FILE),
        owner_marker(paths).as_bytes(),
    )
}

fn owner_marker(paths: &UtcpPaths) -> String {
    format!("{}\n", paths.config_file.to_string_lossy())
}

/// Whether `paths` is the default setup: no `--config`, `UTCP_CONFIG` or `UTCP_CONFIG_DIR` moved the registry.
fn is_default_setup(paths: &UtcpPaths) -> bool {
    directories::ProjectDirs::from("", "", "utcp")
        .is_some_and(|p| p.config_dir().join(".utcp_config.json") == paths.config_file)
}

/// Whether the skills in `root` list the manuals of `paths`' registry: their [`OWNER_FILE`] names it or, for skills
/// written before that file existed, `paths` is the default setup.
fn owned_by(paths: &UtcpPaths, root: &Path) -> bool {
    match std::fs::read_to_string(root.join(TOOLS_SKILL).join(OWNER_FILE)) {
        Ok(owner) => owner.trim_end() == owner_marker(paths).trim_end(),
        Err(_) => is_default_setup(paths),
    }
}

/// Inspect the installed skills across target clients.
pub fn inspect_skill_targets(
    paths: &UtcpPaths,
    include_global: bool,
    workspace_dir: Option<&Path>,
) -> Vec<IdeSkillTargetStatus> {
    let files = skill_files(
        SkillTargetPlatform::current(),
        &registered_manual_labels(paths),
    );
    resolve_skill_targets(include_global, workspace_dir)
        .into_iter()
        .map(|(client, root)| target_status(client, &root, &files))
        .collect()
}

/// Write the platform-native skills using regular atomic file writes (never requiring Windows Admin
/// symlink rights).
pub fn sync_ide_skills(
    paths: &UtcpPaths,
    include_global: bool,
    workspace_dir: Option<&Path>,
) -> Result<Vec<IdeSkillTargetStatus>> {
    let files = skill_files(
        SkillTargetPlatform::current(),
        &registered_manual_labels(paths),
    );
    let targets = resolve_skill_targets(include_global, workspace_dir);
    let mut statuses = Vec::with_capacity(targets.len());

    for (client, root) in targets {
        write_skill_files(paths, &root, &files)?;
        statuses.push(IdeSkillTargetStatus {
            client,
            path: root.to_string_lossy().into_owned(),
            exists: true,
            up_to_date: true,
        });
    }
    Ok(statuses)
}

/// Refresh the skills in every global folder that already has the tools skill whenever manuals are
/// registered or deregistered, so the 1-line `registered manuals: ...` hint stays current. Only folders whose skills
/// this registry wrote ([`OWNER_FILE`]) are refreshed; `sync-ides` takes a folder over explicitly.
pub fn refresh_installed_global_skills(paths: &UtcpPaths) {
    let files = skill_files(
        SkillTargetPlatform::current(),
        &registered_manual_labels(paths),
    );
    for (_, root) in resolve_skill_targets(true, None) {
        if root.join(TOOLS_SKILL).join("SKILL.md").exists() && owned_by(paths, &root) {
            let _ = write_skill_files(paths, &root, &files);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_targets_cover_each_client_and_skip_gemini_cli() {
        let targets = resolve_skill_targets(true, None);
        let paths: Vec<String> = targets
            .iter()
            .map(|(_, p)| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(
            paths.iter().all(|p| !p.contains("/.gemini/skills/")),
            "Gemini CLI was replaced by Antigravity; ~/.gemini/skills is not a target: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with(".gemini/antigravity-cli/skills")),
            "agy target present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("/.agents/skills")),
            "Zed target present: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("/.copilot/skills")),
            "VS Code's Copilot harness reads ~/.copilot/skills, not ~/.agents/skills: {paths:?}"
        );
        assert!(
            targets.iter().all(|(c, _)| !c.contains("Gemini CLI")),
            "no Gemini CLI row"
        );
    }

    /// `utcp doctor` run from the home folder listed `~/.agents/skills` twice: as Zed and as
    /// "This folder". The project row is dropped when it is one of the global folders.
    #[test]
    fn workspace_target_in_home_is_not_listed_twice() {
        let Some(home) = home_dir() else { return };
        let global = resolve_skill_targets(true, None);
        assert_eq!(resolve_skill_targets(true, Some(&home)), global);
        assert_eq!(
            resolve_skill_targets(true, Some(&home.join("."))),
            global,
            "another spelling of the home folder"
        );

        let elsewhere = std::env::temp_dir().join("utcp-skills-ws-test");
        let with_ws = resolve_skill_targets(true, Some(&elsewhere));
        assert_eq!(with_ws.len(), global.len() + 1);
        assert_eq!(
            with_ws.last().map(|(_, p)| p.clone()),
            Some(elsewhere.join(".agents/skills"))
        );
        assert_eq!(resolve_skill_targets(false, Some(&home)).len(), 1);
    }

    /// An agent following only the skill must be able to import an MCP server correctly: one
    /// `--arg` per argument, `${VAR}` placeholders for secrets, where values go, and a check.
    #[test]
    fn rendered_skill_teaches_mcp_imports_and_secrets() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_admin_skill_markdown(platform);
            assert!(md.contains("--command <program> --arg <a> --arg <b>"));
            assert!(md.contains("takes the program only"));
            assert!(md.contains("each `env` entry to `-e`"));
            assert!(md.contains("write `${VAR}` placeholders in `-H` and `-e`"));
            assert!(md.contains("list <name>"));
        }
        let linux = render_admin_skill_markdown(SkillTargetPlatform::LinuxPosix);
        assert!(linux.contains("`VAR=value` to\n  `~/.config/utcp/.env`"));
    }

    fn entries(list: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        list.iter()
            .map(|(n, h)| (n.to_string(), h.map(str::to_string)))
            .collect()
    }

    /// Nine `azdo_*` manuals repeated "Azure DevOps ..., org ni" nine times in every agent's prompt.
    #[test]
    fn manuals_sharing_a_prefix_get_one_label_with_the_shared_hint_words_once() {
        let labels = grouped_manual_labels(&entries(&[
            (
                "azdo_admin",
                Some("Azure DevOps permissions, connections, hooks, org ni"),
            ),
            ("azdo_git", Some("Azure DevOps repos and PRs, org ni")),
            (
                "azdo_wiki_search",
                Some("Azure DevOps wiki and search, org ni"),
            ),
            ("context7", Some("library and framework docs")),
            ("d2", None),
            ("github", Some("GitHub REST API")),
            ("my_tool", Some("one of a kind")),
        ]));
        assert_eq!(
            labels,
            vec![
                "azdo_* (Azure DevOps, org ni: admin, git, wiki_search)",
                "context7 (library and framework docs)",
                "d2",
                "github (GitHub REST API)",
                "my_tool (one of a kind)",
            ]
        );
    }

    /// The manuals registered on 25 Nov 2026: nine `azdo_*` and seven `github_*` with hints made the
    /// description 1,219 characters, and Zed warned that it is over the 1,024 the Agent Skills format allows.
    fn many_hinted_manuals() -> Vec<(String, Option<String>)> {
        entries(&[
            (
                "azdo_admin",
                Some("Azure DevOps permissions, connections, hooks, org ni"),
            ),
            (
                "azdo_analytics",
                Some("Azure DevOps Analytics OData queries, org ni"),
            ),
            (
                "azdo_artifacts",
                Some("Azure DevOps feeds, packages, symbols, org ni"),
            ),
            (
                "azdo_core",
                Some("Azure DevOps projects, teams, users, org ni"),
            ),
            ("azdo_git", Some("Azure DevOps repos and PRs, org ni")),
            (
                "azdo_pipelines",
                Some("Azure DevOps builds and pipelines, org ni"),
            ),
            (
                "azdo_test",
                Some("Azure DevOps test plans and results, org ni"),
            ),
            (
                "azdo_wiki_search",
                Some("Azure DevOps wiki and search, org ni"),
            ),
            (
                "azdo_work",
                Some("Azure DevOps work items, boards, dashboards, org ni"),
            ),
            ("context7", Some("library and framework docs")),
            ("d2", Some("diagrams from text")),
            ("github", None),
            (
                "github_actions",
                Some("GitHub Actions, checks, environments, packages, Pages"),
            ),
            (
                "github_copilot",
                Some("GitHub Copilot agent tasks, automations, settings"),
            ),
            (
                "github_issues_pulls",
                Some("GitHub issues, pull requests, reviews, labels, projects"),
            ),
            (
                "github_repos",
                Some("GitHub repos, files, branches, commits, git, releases"),
            ),
            (
                "github_search",
                Some("GitHub search: code, issues, PRs, repos, commits, users"),
            ),
            (
                "github_security",
                Some("GitHub code and secret scanning, Dependabot, advisories"),
            ),
            (
                "github_users",
                Some("GitHub users, notifications, stars, orgs, billing, meta"),
            ),
        ])
    }

    fn description_len(platform: SkillTargetPlatform, labels: &[String]) -> usize {
        skill_description(&render_skill_markdown(platform, labels))
            .chars()
            .count()
    }

    /// Group members are listed by name only, so the 25 Nov 2026 set now fits untouched.
    #[test]
    fn the_registered_manuals_of_25_nov_fit_without_shortening() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let all = many_hinted_manuals();
            let labels = fit_manual_labels(&all, platform);
            assert_eq!(labels, grouped_manual_labels(&all));
            let len = description_len(platform, &labels);
            assert!(len <= 700, "{len}: {labels:?}");
        }
    }

    #[test]
    fn the_description_is_shortened_to_the_limit_longest_label_first() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let mut all = many_hinted_manuals();
            for i in 0..8 {
                all.push((
                    format!("svc{i:02}"),
                    Some(format!(
                        "service number {i} for tests, with a hint of medium length"
                    )),
                ));
            }
            assert!(
                description_len(platform, &grouped_manual_labels(&all)) > MAX_DESCRIPTION_CHARS,
                "the case this guards against"
            );
            let labels = fit_manual_labels(&all, platform);
            let len = description_len(platform, &labels);
            assert!(len <= MAX_DESCRIPTION_CHARS, "{len}: {labels:?}");
            // Short labels keep their hints; the longest labels give up their words first.
            assert!(
                labels.contains(&"context7 (library and framework docs)".to_string()),
                "{labels:?}"
            );
            assert!(
                labels.contains(&"d2 (diagrams from text)".to_string()),
                "{labels:?}"
            );
            assert!(labels.contains(&"github".to_string()), "{labels:?}");
            assert_eq!(labels.len(), 13, "no manual is cut: {labels:?}");
            assert!(
                labels.contains(&"github_* (GitHub)".to_string()),
                "{labels:?}"
            );
        }
    }

    #[test]
    fn labels_that_fit_are_left_alone() {
        let few = entries(&[
            ("context7", Some("library and framework docs")),
            ("d2", None),
        ]);
        assert_eq!(
            fit_manual_labels(&few, SkillTargetPlatform::LinuxPosix),
            grouped_manual_labels(&few)
        );
        assert_eq!(
            fit_manual_labels(&[], SkillTargetPlatform::LinuxPosix),
            Vec::<String>::new()
        );
    }

    #[test]
    fn when_bare_names_are_too_long_the_list_ends_with_how_many_more() {
        let names: Vec<String> = (0..120).map(|i| format!("manual{i:03}")).collect();
        let many: Vec<(String, Option<String>)> = names
            .iter()
            .map(|n| (n.clone(), Some("some hint".into())))
            .collect();
        let labels = fit_manual_labels(&many, SkillTargetPlatform::LinuxPosix);
        let len = description_len(SkillTargetPlatform::LinuxPosix, &labels);
        assert!(len <= MAX_DESCRIPTION_CHARS, "{len}");
        let last = labels.last().unwrap();
        assert!(
            last.starts_with("and ") && last.ends_with(" more (see `utcp list`)"),
            "{last}"
        );
        let shown = labels.len() - 1;
        assert_eq!(last, &format!("and {} more (see `utcp list`)", 120 - shown));
        assert!(
            labels[..shown].iter().all(|l| !l.contains('(')),
            "bare names: {labels:?}"
        );
    }

    #[test]
    fn skill_description_reads_the_folded_front_matter() {
        let md = "---\nname: x\ndescription: >-\n  one two\n  three\n---\n\nbody\n";
        assert_eq!(skill_description(md), "one two three");
        let md = "---\nname: x\ndescription: plain text\nother: y\n---\nbody\n";
        assert_eq!(skill_description(md), "plain text");
    }

    #[test]
    fn group_forms_go_from_member_words_to_the_bare_prefix() {
        let forms = manual_label_forms(&entries(&[
            ("azdo_git", Some("Azure DevOps repos and PRs, org ni")),
            ("azdo_work", Some("Azure DevOps work items, org ni")),
            ("d2", None),
        ]));
        assert_eq!(
            forms,
            vec![
                vec![
                    "azdo_* (Azure DevOps, org ni: git, work)".to_string(),
                    "azdo_* (Azure DevOps, org ni)".to_string(),
                    "azdo_*".to_string(),
                ],
                vec!["d2".to_string()],
            ]
        );
    }

    #[test]
    fn a_group_with_a_manual_without_hint_or_nothing_in_common_shares_no_words() {
        let labels = grouped_manual_labels(&entries(&[
            ("gh_issues", Some("GitHub issues")),
            ("gh_users", None),
        ]));
        assert_eq!(labels, vec!["gh_* (issues (GitHub issues), users)"]);
        let labels = grouped_manual_labels(&entries(&[
            ("x_a", Some("alpha docs")),
            ("x_b", Some("beta api")),
        ]));
        assert_eq!(labels, vec!["x_* (a (alpha docs), b (beta api))"]);
        // A hint that is all shared words leaves the member bare.
        let labels =
            grouped_manual_labels(&entries(&[("x_a", Some("Foo")), ("x_b", Some("Foo bar"))]));
        assert_eq!(labels, vec!["x_* (Foo: a, b)"]);
    }

    #[test]
    fn hints_render_next_to_manual_names_in_the_description() {
        assert_eq!(manual_label("d2", None), "d2");
        assert_eq!(manual_label("d2", Some("")), "d2");
        assert_eq!(
            manual_label("context7", Some("library docs")),
            "context7 (library docs)"
        );
        let labels = vec![
            manual_label("context7", Some("library and framework docs")),
            manual_label("d2", Some("diagrams")),
            manual_label("github", None),
        ];
        let md = render_skill_markdown(SkillTargetPlatform::LinuxPosix, &labels);
        let front = md.split("\n---\n").next().unwrap();
        assert!(
            front.contains(
                "(registered manuals: context7 (library and framework docs), d2 (diagrams), github)"
            ),
            "{front}"
        );
        let admin = render_admin_skill_markdown(SkillTargetPlatform::LinuxPosix);
        assert!(admin.contains("set-hint <name>"), "skill teaches set-hint");
        assert!(
            admin.contains("--hint '<few words>'"),
            "skill teaches --hint"
        );
    }

    /// The description is all an agent sees before it picks a skill. A VS Code agent asked to call
    /// `mocktest.echo` looked for a `mocktest` MCP server in its own tools, found none and gave up, so the
    /// description must say where these tools are not and how to reach them.
    #[test]
    fn description_says_the_tools_are_reached_through_the_cli_not_the_agents_tool_list() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_skill_markdown(platform, &["mocktest (test server)".to_string()]);
            let front = md.split("\n---\n").next().unwrap().replace("\n  ", " ");
            for needle in [
                "(registered manuals: mocktest (test server))",
                "not in your own tool list or MCP servers",
                "call <manual.tool>",
                "whenever a request names one of these manuals",
            ] {
                assert!(front.contains(needle), "{needle:?} missing: {front}");
            }
            assert!(!front.contains("without MCP servers"), "{front}");
        }
    }

    /// Hand edits to an installed skill are lost at the next write, so each file says so. The note
    /// sits in the body, which loads only when the skill is used, not in the always-loaded front matter.
    #[test]
    fn each_skill_says_it_is_written_by_utcp_below_the_front_matter() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            for md in [
                render_skill_markdown(platform, &[]),
                render_admin_skill_markdown(platform),
            ] {
                let (front, body) = md
                    .strip_prefix("---\n")
                    .and_then(|rest| rest.split_once("\n---\n"))
                    .expect("front matter");
                assert!(!front.contains("Written by utcp"), "{front}");
                let note = body.trim_start();
                assert!(
                    note.starts_with("<!-- Written by utcp"),
                    "{platform:?}: {note}"
                );
                assert!(!note.contains("{{"), "unfilled placeholder: {note}");
            }
        }
    }

    /// Agents can find out why calls fail and fix a hung local MCP server themselves.
    #[test]
    fn rendered_skill_teaches_failure_lookup_and_server_management() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_skill_markdown(platform, &[]);
            assert!(md.contains("log recent --failed"), "{platform:?}");
            assert!(md.contains("Nothing was sent"), "{platform:?}");
            assert!(md.contains("use the utcp-admin skill"), "{platform:?}");
            let admin = render_admin_skill_markdown(platform);
            for command in [
                "log recent --failed",
                "daemon servers",
                "daemon restart-server <name>",
                "daemon stop-server <name>",
            ] {
                assert!(
                    admin.contains(command),
                    "{platform:?} skill lacks `{command}`"
                );
            }
            assert!(
                admin.contains("ask the user before `"),
                "deregister needs the user's OK"
            );
        }
    }

    /// Agents learn to filter list answers, to trust notes, and to page on.
    #[test]
    fn rendered_skill_teaches_where() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_skill_markdown(platform, &[]);
            for needle in [
                "`--where 'path=a|b'`",
                "a date alone is that whole UTC day",
                "`note` in `batch`",
                "which keys aren't in the schema",
                "fix the value rather than conclude there",
                "plus those in `next_page`",
                "`fields`, `where` and `out`",
                "see one with `--path`",
            ] {
                assert!(md.contains(needle), "{platform:?} lacks {needle:?}");
            }
        }
    }

    /// The body stays in an agent's context for the rest of the thread once loaded, so it stays short;
    /// details live in `utcp <command> --help`.
    #[test]
    fn rendered_skill_body_stays_short() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_skill_markdown(platform, &[]);
            let body = md
                .strip_prefix("---\n")
                .and_then(|rest| rest.split_once("\n---\n"))
                .expect("front matter")
                .1;
            let len = body.chars().count();
            assert!(len <= 4_500, "{platform:?}: {len} characters");
        }
    }

    #[test]
    fn admin_skill_renders_on_both_platforms() {
        for platform in [
            SkillTargetPlatform::LinuxPosix,
            SkillTargetPlatform::WindowsPowerShell,
        ] {
            let md = render_admin_skill_markdown(platform);
            assert!(!md.contains("{{"), "unrendered placeholder in {platform:?}");
            assert!(md.starts_with("---\nname: utcp-admin\n"), "{platform:?}");
            assert!(md.contains("`operating.md`, next to this file"));
            assert!(
                md.contains("Ask the user before"),
                "deregister and daemon restarts need an OK"
            );
            assert!(md.contains("daemon restart-server <name>"));
        }
        let windows = render_admin_skill_markdown(SkillTargetPlatform::WindowsPowerShell);
        assert!(windows.contains("utcp.exe register") && !windows.contains("```sh"));
        let linux = render_admin_skill_markdown(SkillTargetPlatform::LinuxPosix);
        assert!(linux.contains("`~/.config/utcp/.env`"));
    }

    fn test_paths(base: &Path) -> UtcpPaths {
        UtcpPaths {
            config_dir: base.join("config"),
            cache_dir: base.join("cache"),
            state_dir: base.join("state"),
            media_dir: base.join("media"),
            config_file: base.join("config/.utcp_config.json"),
            settings_file: base.join("config/settings.toml"),
            env_file: base.join("config/.env"),
            lock_file: base.join("config/.utcp.lock"),
            manuals_dir: base.join("cache/manuals"),
            search_index_file: base.join("cache/search_index.bin"),
            logging_sentinel: base.join("state/logging.enabled"),
            activity_log_file: base.join("state/activity.jsonl"),
            activity_log_bak: base.join("state/activity.jsonl.bak"),
            socket_endpoint: base.join("cache/utcp.sock").to_string_lossy().into_owned(),
            daemon_pid_file: base.join("state/daemon.pid"),
            separate_setup: true,
        }
    }

    /// Sync writes both skills and the guide; a missing admin file makes the folder outdated.
    #[test]
    fn sync_writes_both_skills_and_status_checks_every_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = test_paths(&tmp.path().join("utcp"));
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents").join("skills");

        let before = inspect_skill_targets(&paths, false, Some(&ws));
        assert_eq!(before.len(), 1);
        assert!(!before[0].exists && !before[0].up_to_date);

        let synced = sync_ide_skills(&paths, false, Some(&ws)).unwrap();
        assert_eq!(synced[0].path, root.to_string_lossy());
        for rel in [
            "utcp-tools/SKILL.md",
            "utcp-admin/SKILL.md",
            "utcp-admin/operating.md",
        ] {
            assert!(root.join(rel).is_file(), "{rel} written");
        }
        assert_eq!(
            std::fs::read_to_string(root.join("utcp-admin/operating.md")).unwrap(),
            OPERATING_GUIDE
        );
        let after = inspect_skill_targets(&paths, false, Some(&ws));
        assert!(after[0].exists && after[0].up_to_date);

        std::fs::remove_file(root.join("utcp-admin/SKILL.md")).unwrap();
        let outdated = inspect_skill_targets(&paths, false, Some(&ws));
        assert!(outdated[0].exists && !outdated[0].up_to_date);
    }
}

//! What goes into which manual, what is left out and why, and how tools are named.
//!
//! Every operation of GitHub's description becomes a tool unless a rule in `Config::rules` leaves it out with its
//! reason. The rules record the coverage review: enterprise and organization administration, areas not used, and a
//! reviewed list of dangerous operations (access grants, repository settings, security settings). Everything else
//! is included, and the token's scopes decide what an agent may do with it. Every included category must have a
//! manual (`categories`), so a category GitHub adds later fails the generator until it is reviewed.

use std::collections::HashMap;

use regex::Regex;

pub const API_VERSION: &str = "2026-03-10";
pub const PAT_VAR: &str = "GITHUB_MANUALS_PAT";

#[derive(Clone, Debug)]
pub struct ManualConf {
    pub name: String,
    pub title: String,
    pub hint: String,
}

/// Leaves out an operation when the method matches `methods` (a regex over the upper-case method), the path matches
/// `path` and not `except`, and `category` (if set) is the operation's `x-github` category. `keep` lists
/// (METHOD, path) pairs the rule does not apply to.
#[derive(Clone, Debug)]
pub struct Rule {
    pub methods: Regex,
    pub path: Regex,
    pub except: Option<Regex>,
    pub category: Option<String>,
    pub keep: Vec<(String, String)>,
    /// Shown in `generate`'s summary and README; operations are grouped by it.
    pub reason: String,
}

impl Rule {
    pub fn matches(&self, method: &str, path: &str, category: &str) -> bool {
        if self.keep.iter().any(|(m, p)| m == method && p == path) {
            return false;
        }
        if let Some(c) = &self.category {
            if c != category {
                return false;
            }
        }
        if self.except.as_ref().is_some_and(|e| e.is_match(path)) {
            return false;
        }
        self.methods.is_match(method) && self.path.is_match(path)
    }
}

/// Where an area's noun goes in a tool name: inserted after the verb unless one of `markers` is already a word of it.
#[derive(Clone, Debug)]
pub struct TagNoun {
    pub noun: String,
    pub markers: Vec<String>,
}

/// Everything the generator decides by configuration. `Config::real()` is the real one; tests build their own.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub manuals: Vec<ManualConf>,
    /// `x-github` category -> manual.
    pub categories: HashMap<String, String>,
    /// (category, subcategory) -> manual, overriding `categories`.
    pub subcategories: HashMap<(String, String), String>,
    /// Checked in order; the first match leaves the operation out. Deprecated operations are always left out.
    pub rules: Vec<Rule>,
    /// operationId tag (before the `/`) -> the noun a bare verb gets (`repos/get` -> `get_repo`).
    pub nouns: HashMap<String, String>,
    /// operationId tag -> noun inserted after the verb when missing (`pulls/list-files` ->
    /// `list_pull_request_files`).
    pub tag_nouns: HashMap<String, TagNoun>,
    /// operationId tag -> the word that goes after the verb when two tools of one manual still share a name.
    pub clash_words: HashMap<String, String>,
    /// The tag whose tools keep the plain name in a clash, per manual (`repos` in `github_repos`).
    pub home_tags: HashMap<String, String>,
    /// operationId -> tool name, for names the rules get wrong.
    pub names: HashMap<String, String>,
    /// Writes that worked in live tests ([`TESTED_WRITES`]). The others are marked "(untested)" and tagged `untested`.
    pub tested_writes: Vec<String>,
    /// Path parameter name -> its description, for GitHub's placeholder descriptions ("path parameter"). A
    /// placeholder without an entry fails `generate`.
    pub param_descriptions: HashMap<String, String>,
    /// (operationId, path parameter, note) for a path parameter an agent may leave out; utcp then drops that part of
    /// the URL (`/contents/{path}` -> `/contents`, the repository root). The note follows the description.
    pub optional_path: Vec<(String, String, String)>,
    /// (operationId, argument) -> its description, for arguments GitHub leaves undescribed. An undescribed argument
    /// without an entry, or an entry for an argument GitHub describes, fails `generate`.
    pub input_descriptions: HashMap<(String, String), String>,
    /// `x-github` category -> what follows a summary that other tools share ("for Dependabot"). A shared summary
    /// without one fails `generate`.
    pub summary_qualifiers: HashMap<String, String>,
    /// (operationId, GitHub's text, replacement): fixes to a tool description where GitHub is wrong or talks about
    /// HTTP details utcp takes care of. The text is matched in GitHub's description as plain text with each paragraph
    /// on one line; a fix whose text is not there fails `generate`, so it cannot go stale.
    pub sentence_fixes: Vec<(String, String, String)>,
    /// operationId -> the summary a tool description starts with, instead of GitHub's ([`SUMMARIES`]). An entry for
    /// an operation the description does not have, or equal to GitHub's summary, fails `generate`.
    pub summaries: HashMap<String, String>,
    /// operationId -> sentences added to the end of a tool description and kept when it is shortened, such as a
    /// pointer to a variant.
    pub notes: HashMap<String, String>,
    /// Extra tools that call an operation with another `Accept` media type.
    pub variants: Vec<Variant>,
    /// Leave out every write.
    pub read_only: bool,
}

/// A second tool for an operation that asks GitHub for another media type, such as a file's raw content instead of
/// JSON with base64. It has the operation's inputs.
#[derive(Clone, Debug)]
pub struct Variant {
    pub op_id: String,
    /// The tool name, in the operation's manual.
    pub name: String,
    pub accept: String,
    /// The whole tool description.
    pub description: String,
    /// The answer is always a file (`response_type` `file`); otherwise utcp prints JSON or text and saves binary.
    pub file: bool,
    /// The output description.
    pub output: String,
}

fn re(s: &str) -> Regex {
    Regex::new(s).expect("valid regex in config")
}

pub fn rule(methods: &str, path: &str, reason: &str) -> Rule {
    Rule {
        methods: re(&format!("^(?:{methods})$")),
        path: re(path),
        except: None,
        category: None,
        keep: Vec::new(),
        reason: reason.into(),
    }
}

pub fn category_rule(category: &str, reason: &str) -> Rule {
    Rule {
        category: Some(category.into()),
        ..rule(".*", "", reason)
    }
}

fn keeping(mut r: Rule, keep: &[(&str, &str)]) -> Rule {
    r.keep = keep
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    r
}

pub fn manual(name: &str, title: &str, hint: &str) -> ManualConf {
    ManualConf {
        name: name.into(),
        title: title.into(),
        hint: hint.into(),
    }
}

fn pairs(items: &[(&str, &str)]) -> HashMap<String, String> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const R: &str = r"^/repos/\{owner\}/\{repo\}";

/// The reasons, shared by the rules and the README.
pub mod why {
    pub const ENTERPRISE: &str = "enterprise administration";
    pub const ORG: &str = "organization administration (not covered yet)";
    pub const ORG_BILLING: &str = "organization billing";
    pub const AREA_NOT_USED: &str = "area not used: Codespaces, Copilot Spaces, gists, Classroom";
    pub const MODERATION: &str =
        "moderation: interaction limits, blocking, revoking others' credentials";
    pub const MIGRATIONS: &str = "migrations: can lock repositories";
    pub const ORG_SECURITY: &str = "organization security administration";
    pub const APP_TOKEN: &str = "needs a GitHub App's own token, or grants an app access";
    pub const TEAMS: &str = "teams: organization administration";
    pub const COPILOT_ORG: &str = "Copilot organization administration";
    pub const STUB: &str = "Marketplace test stubs";
    pub const DELETE_REPO: &str = "deletes or transfers a repository";
    pub const REPO_SETTINGS: &str =
        "repository settings: visibility, archiving, renaming, security (one operation)";
    pub const ACCESS: &str = "grants access: collaborators, invitations, deploy keys";
    pub const ACCOUNT: &str = "account security: SSH, GPG and signing keys, emails, profile";
    pub const WEBHOOKS: &str = "webhooks: send repository events to any URL";
    pub const PROTECTION: &str = "branch protection and rulesets: the guards that stop an agent";
    pub const ACTIONS_SECURITY: &str =
        "Actions security settings: allowed actions, token permissions, OIDC";
    pub const SECRETS: &str = "secret writes: the value must be encrypted first (gh secret set)";
    pub const VARIABLES: &str = "variable writes: they switch off the Copilot agent firewall";
    pub const DISABLE_SECURITY: &str = "turns a security feature off (turning it on is included)";
    pub const PUSH_BYPASS: &str = "lets a detected secret be pushed";
    pub const PATTERNS: &str = "changes or deletes secret scanning patterns";
}

impl Config {
    pub fn manual_names(&self) -> Vec<&str> {
        self.manuals.iter().map(|m| m.name.as_str()).collect()
    }

    pub fn read_only(mut self) -> Config {
        self.read_only = true;
        self
    }

    /// The manual of an included operation, or `None` when its category has none.
    pub fn manual_of(&self, category: &str, subcategory: &str) -> Option<&str> {
        self.subcategories
            .get(&(category.to_string(), subcategory.to_string()))
            .or_else(|| self.categories.get(category))
            .map(String::as_str)
    }

    pub fn real() -> Config {
        let manuals = vec![
            manual(
                "github_repos",
                "GitHub repositories",
                "GitHub repos, files, branches, commits, git, releases",
            ),
            manual(
                "github_issues_pulls",
                "GitHub issues and pull requests",
                "GitHub issues, pull requests, reviews, labels, projects",
            ),
            manual(
                "github_actions",
                "GitHub Actions, checks, deployments, packages and Pages",
                "GitHub Actions, checks, environments, packages, Pages",
            ),
            manual(
                "github_security",
                "GitHub security",
                "GitHub code and secret scanning, Dependabot, advisories",
            ),
            manual(
                "github_copilot",
                "GitHub Copilot",
                "GitHub Copilot agent tasks, automations, settings",
            ),
            manual(
                "github_search",
                "GitHub search",
                "GitHub search: code, issues, PRs, repos, commits, users",
            ),
            manual(
                "github_users",
                "GitHub users, activity and account",
                "GitHub users, notifications, stars, orgs, billing, meta",
            ),
        ];
        let by_manual: [(&str, &[&str]); 7] = [
            (
                "github_repos",
                &[
                    "repos",
                    "branches",
                    "commits",
                    "git",
                    "releases",
                    "collaborators",
                    "deploy-keys",
                    "metrics",
                    "licenses",
                    "gitignore",
                    "codes-of-conduct",
                ],
            ),
            (
                "github_issues_pulls",
                &["issues", "pulls", "reactions", "projects"],
            ),
            (
                "github_actions",
                &["actions", "checks", "deployments", "packages", "pages"],
            ),
            (
                "github_security",
                &[
                    "code-scanning",
                    "security-advisories",
                    "dependabot",
                    "secret-scanning",
                    "code-quality",
                    "dependency-graph",
                    "code-security",
                ],
            ),
            (
                "github_copilot",
                &["agent-tasks", "agent-automations", "agents", "copilot"],
            ),
            ("github_search", &["search"]),
            (
                "github_users",
                &[
                    "users",
                    "activity",
                    "orgs",
                    "teams",
                    "apps",
                    "billing",
                    "meta",
                    "rate-limit",
                    "markdown",
                    "emojis",
                ],
            ),
        ];
        let mut categories = HashMap::new();
        for (m, cats) in by_manual {
            for c in cats {
                categories.insert(c.to_string(), m.to_string());
            }
        }
        let subcategories: HashMap<(String, String), String> = [
            ("users", "issue-fields", "github_issues_pulls"),
            ("users", "issue-types", "github_issues_pulls"),
            ("repos", "issue-types", "github_issues_pulls"),
            ("repos", "planning-statuses", "github_issues_pulls"),
            ("repos", "attestations", "github_security"),
            ("users", "attestations", "github_security"),
            ("pulls", "agent-merge", "github_copilot"),
            ("pulls", "copilot-code-review", "github_copilot"),
        ]
        .into_iter()
        .map(|(c, s, m)| ((c.to_string(), s.to_string()), m.to_string()))
        .collect();

        use why::*;
        let rules = vec![
            rule(".*", r"^/enterprises/", ENTERPRISE),
            keeping(
                rule(".*", r"^/orgs/", ORG),
                &[("GET", "/orgs/{org}"), ("GET", "/orgs/{org}/repos")],
            ),
            rule(".*", r"^/organizations/", ORG_BILLING),
            category_rule("codespaces", AREA_NOT_USED),
            category_rule("copilot-spaces", AREA_NOT_USED),
            category_rule("gists", AREA_NOT_USED),
            category_rule("classroom", AREA_NOT_USED),
            category_rule("interactions", MODERATION),
            category_rule("credentials", MODERATION),
            category_rule("migrations", MIGRATIONS),
            category_rule("campaigns", ORG_SECURITY),
            category_rule("private-registries", ORG_SECURITY),
            category_rule("enterprise-teams", ENTERPRISE),
            category_rule("hosted-compute", ENTERPRISE),
            category_rule("announcement-banners", ENTERPRISE),
            rule(".*", r"/stubbed(/|$)", STUB),
            keeping(
                category_rule("apps", APP_TOKEN),
                &[
                    ("GET", "/user/installations"),
                    ("GET", "/user/installations/{installation_id}/repositories"),
                    ("GET", "/apps/{app_slug}"),
                    ("GET", "/user/marketplace_purchases"),
                ],
            ),
            keeping(category_rule("teams", TEAMS), &[("GET", "/user/teams")]),
            Rule {
                except: Some(re(r"^/repos/")),
                ..category_rule("copilot", COPILOT_ORG)
            },
            rule("DELETE", &format!("{R}$"), DELETE_REPO),
            rule("POST", &format!("{R}/transfer$"), DELETE_REPO),
            rule("PATCH", &format!("{R}$"), REPO_SETTINGS),
            rule("PUT|DELETE", &format!("{R}/collaborators/"), ACCESS),
            rule("PATCH|DELETE", &format!("{R}/invitations/"), ACCESS),
            rule("POST|DELETE", &format!("{R}/keys"), ACCESS),
            rule(
                "POST|DELETE|PATCH",
                r"^/user/(keys|gpg_keys|ssh_signing_keys|emails|social_accounts|email/visibility)",
                ACCOUNT,
            ),
            rule("PATCH", r"^/user$", ACCOUNT),
            rule("PUT|DELETE", r"^/user/blocks", MODERATION),
            rule("POST|PATCH|DELETE", &format!("{R}/hooks"), WEBHOOKS),
            rule("POST|PUT|PATCH|DELETE", &format!("{R}/branches/[^/]+/protection"), PROTECTION),
            rule("POST|PUT|DELETE", &format!("{R}/rulesets"), PROTECTION),
            rule("PUT|POST|DELETE", &format!("{R}/actions/(permissions|oidc)"), ACTIONS_SECURITY),
            rule("POST|PUT|DELETE", &format!("{R}/actions/policies"), ACTIONS_SECURITY),
            rule("PUT|DELETE", r"/secrets/", SECRETS),
            rule("POST|PATCH|DELETE", r"/variables", VARIABLES),
            rule(
                "DELETE",
                &format!(
                    "{R}/(vulnerability-alerts|automated-security-fixes|private-vulnerability-reporting|\
                     immutable-releases)$"
                ),
                DISABLE_SECURITY,
            ),
            rule("POST", &format!("{R}/secret-scanning/push-protection-bypasses"), PUSH_BYPASS),
            rule("PATCH|DELETE", &format!("{R}/secret-scanning/custom-patterns"), PATTERNS),
            rule("PUT|DELETE", r"^/user/installations/[^/]+/repositories/", APP_TOKEN),
        ];

        let nouns = pairs(&[
            ("repos", "repo"),
            ("issues", "issue"),
            ("pulls", "pull_request"),
            ("orgs", "org"),
            ("users", "user"),
            ("licenses", "license"),
            ("gitignore", "gitignore_template"),
            ("codes-of-conduct", "code_of_conduct"),
            ("releases", "release"),
            ("packages", "package"),
            ("projects", "project"),
            ("teams", "team"),
            ("apps", "app"),
            ("meta", "meta"),
            ("markdown", "markdown"),
            ("emojis", "emojis"),
            ("rate-limit", "rate_limit"),
            ("pull-request-stacks", "pull_request_stack"),
            ("checks", "check"),
            ("git", "git"),
            ("actions", "actions"),
            ("reactions", "reaction"),
            ("code-scanning", "code_scanning"),
            ("dependabot", "dependabot"),
            ("billing", "billing"),
        ]);
        let tag_noun = |noun: &str, markers: &[&str]| TagNoun {
            noun: noun.into(),
            markers: markers.iter().map(|m| m.to_string()).collect(),
        };
        let tag_nouns: HashMap<String, TagNoun> = [
            ("issues", tag_noun("issue", &["issue", "issues"])),
            ("pulls", tag_noun("pull_request", &["pull", "pulls"])),
            (
                "reactions",
                tag_noun("reaction", &["reaction", "reactions"]),
            ),
            ("checks", tag_noun("check", &["check", "checks"])),
            ("git", tag_noun("git", &["git"])),
            ("gitignore", tag_noun("gitignore", &["gitignore"])),
            ("dependabot", tag_noun("dependabot", &["dependabot"])),
            ("agents", tag_noun("copilot_agent", &["agent"])),
            ("code-quality", tag_noun("code_quality", &["quality"])),
            (
                "code-scanning",
                tag_noun(
                    "code_scanning",
                    &["scanning", "codeql", "sarif", "autofix", "variant"],
                ),
            ),
            (
                "secret-scanning",
                tag_noun("secret_scanning", &["secret", "scanning"]),
            ),
            ("code-security", tag_noun("code_security", &["security"])),
            (
                "security-advisories",
                tag_noun("advisory", &["advisory", "advisories"]),
            ),
            ("projects", tag_noun("project", &["project", "projects"])),
            ("apps", tag_noun("app", &["app", "apps"])),
            ("markdown", tag_noun("markdown", &["markdown"])),
            (
                "users",
                tag_noun("user", &["user", "users", "username", "person"]),
            ),
            (
                "pull-request-stacks",
                tag_noun("pull_request_stack", &["stack", "stacks"]),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let clash_words = pairs(&[
            ("code-scanning", "code_scanning"),
            ("dependabot", "dependabot"),
            ("secret-scanning", "secret_scanning"),
            ("git", "git"),
            ("repos", "repo"),
            ("users", "user"),
        ]);
        let home_tags = pairs(&[("github_repos", "repos")]);
        let names = pairs(&[
            // Bare names the rules cannot fix.
            ("codes-of-conduct/get-conduct-code", "get_code_of_conduct"),
            (
                "codes-of-conduct/get-all-codes-of-conduct",
                "list_codes_of_conduct",
            ),
            ("gitignore/get-all-templates", "list_gitignore_templates"),
            ("licenses/get-all-commonly-used", "list_licenses"),
            ("meta/get-all-versions", "list_api_versions"),
            ("meta/root", "get_api_root"),
            ("dependency-graph/diff-range", "compare_dependencies"),
            (
                "dependency-graph/create-repository-snapshot",
                "create_dependency_snapshot",
            ),
            ("repos/get-all-environments", "list_environments"),
            ("repos/delete-an-environment", "delete_environment"),
            (
                "repos/get-all-deployment-protection-rules",
                "list_deployment_protection_rules",
            ),
            (
                "repos/update-information-about-pages-site",
                "update_pages_site",
            ),
            ("repos/merge", "merge_branch"),
            ("repos/list-public", "list_public_repos"),
            ("repos/create-using-template", "create_repo_using_template"),
            ("repos/codeowners-errors", "list_codeowners_errors"),
            (
                "repos/custom-properties-for-repos-get-repository-values",
                "get_custom_property_values",
            ),
            (
                "repos/custom-properties-for-repos-create-or-update-repository-values",
                "create_or_update_custom_property_values",
            ),
            (
                "secret-scanning/bulk-create-repo-custom-patterns",
                "create_secret_scanning_custom_patterns",
            ),
            (
                "secret-scanning/list-repo-custom-patterns",
                "list_secret_scanning_custom_patterns",
            ),
            ("pulls/check-if-merged", "check_pull_request_merged"),
            ("pull-request-stacks/list", "list_pull_request_stacks"),
            ("pull-request-stacks/unstack", "unstack_pull_request_stack"),
            // `list_issues` is taken by no tool: the old hand-written manual used it for a repository's issues.
            ("issues/list", "list_assigned_issues"),
            (
                "issues/check-user-can-be-assigned",
                "check_user_can_be_assigned",
            ),
            ("issues/remove-all-labels", "remove_all_issue_labels"),
            // Checks: the bare operations are about check runs.
            ("checks/create", "create_check_run"),
            ("checks/get", "get_check_run"),
            ("checks/update", "update_check_run"),
            ("checks/list-for-ref", "list_check_runs_for_ref"),
            ("checks/list-for-suite", "list_check_runs_for_suite"),
            ("checks/list-annotations", "list_check_run_annotations"),
            ("checks/rerequest-run", "rerequest_check_run"),
            (
                "packages/get-all-package-versions-for-package-owned-by-authenticated-user",
                "list_package_versions_for_authenticated_user",
            ),
            (
                "packages/get-all-package-versions-for-package-owned-by-user",
                "list_package_versions_for_user",
            ),
            ("agent-tasks/create-task-in-repo", "create_agent_task"),
            ("agent-tasks/get-task-by-id", "get_agent_task"),
            (
                "agent-tasks/get-task-by-repo-and-id",
                "get_agent_task_for_repo",
            ),
            ("agent-tasks/list-tasks", "list_agent_tasks"),
            (
                "agent-tasks/list-tasks-for-repo",
                "list_agent_tasks_for_repo",
            ),
            ("users/get-by-username", "get_user_by_username"),
            (
                "users/check-person-is-followed-by-authenticated",
                "check_person_is_followed_by_authenticated_user",
            ),
            (
                "activity/star-repo-for-authenticated-user",
                "star_repo_for_authenticated_user",
            ),
            (
                "activity/unstar-repo-for-authenticated-user",
                "unstar_repo_for_authenticated_user",
            ),
        ]);
        Config {
            manuals,
            categories,
            subcategories,
            rules,
            nouns,
            tag_nouns,
            clash_words,
            home_tags,
            names,
            tested_writes: TESTED_WRITES.iter().map(|s| s.to_string()).collect(),
            param_descriptions: pairs(&[
                (
                    "path",
                    "The path of a file or directory in the repository, such as docs/README.md.",
                ),
                ("deployment_id", "The ID of the deployment."),
                (
                    "account_id",
                    "The user's numeric ID (the id field of a user).",
                ),
                ("tag", "The tag name of the release, such as v1.0.0."),
            ]),
            optional_path: [
                (
                    "repos/get-content",
                    "path",
                    "Leave it out for the repository root.",
                ),
                (
                    "repos/download-tarball-archive",
                    "ref",
                    "Leave it out for the default branch.",
                ),
                (
                    "repos/download-zipball-archive",
                    "ref",
                    "Leave it out for the default branch.",
                ),
            ]
            .iter()
            .map(|(id, p, n)| (id.to_string(), p.to_string(), n.to_string()))
            .collect(),
            input_descriptions: INPUT_DESCRIPTIONS
                .iter()
                .map(|(id, name, d)| ((id.to_string(), name.to_string()), d.to_string()))
                .collect(),
            summary_qualifiers: pairs(&[
                ("actions", "for GitHub Actions"),
                ("agents", "for the Copilot coding agent"),
                ("dependabot", "for Dependabot"),
                ("repos", "for a repository"),
                ("users", "for a user"),
            ]),
            sentence_fixes: SENTENCE_FIXES
                .iter()
                .map(|(id, from, to)| (id.to_string(), from.to_string(), to.to_string()))
                .collect(),
            notes: pairs(NOTES),
            summaries: pairs(SUMMARIES),
            variants: variants(),
            read_only: false,
        }
    }
}

/// Descriptions for the arguments GitHub leaves undescribed: (operationId, argument, description). The meanings of
/// `list_assigned_issues`' booleans and `list_licenses`' `featured` were checked live (2026-11-25).
pub const INPUT_DESCRIPTIONS: &[(&str, &str, &str)] = &[
    (
        "actions/download-artifact",
        "archive_format",
        "The archive format: zip, the only one.",
    ),
    (
        "apps/get-by-slug",
        "app_slug",
        "The app's slug, the last part of its github.com/apps/ URL, such as github-actions.",
    ),
    (
        "checks/list-for-ref",
        "app_id",
        "Only the check runs of the GitHub App with this ID.",
    ),
    (
        "codes-of-conduct/get-conduct-code",
        "key",
        "The code of conduct's key, such as contributor_covenant (list_codes_of_conduct lists them).",
    ),
    (
        "dependency-graph/create-repository-snapshot",
        "job",
        "The CI job that took the snapshot.",
    ),
    ("git/get-blob", "file_sha", "The SHA of the blob."),
    (
        "git/get-tag",
        "tag_sha",
        "The SHA of the annotated tag object, not of the commit it points to.",
    ),
    (
        "gitignore/get-template",
        "name",
        "The template's name, such as Python or Rust (list_gitignore_templates lists them).",
    ),
    (
        "issues/check-user-can-be-assigned",
        "assignee",
        "The login of the user to check.",
    ),
    (
        "issues/check-user-can-be-assigned-to-issue",
        "assignee",
        "The login of the user to check.",
    ),
    ("issues/delete-label", "name", "The label's name."),
    ("issues/get-event", "event_id", "The ID of the issue event."),
    ("issues/get-label", "name", "The label's name."),
    (
        "issues/list",
        "collab",
        "Not documented by GitHub; false returns no issues. Leave it out.",
    ),
    (
        "issues/list",
        "orgs",
        "Not documented by GitHub; false returns no issues. Leave it out.",
    ),
    (
        "issues/list",
        "owned",
        "Not documented by GitHub; false returns no issues. Leave it out.",
    ),
    (
        "issues/list",
        "pulls",
        "false leaves out pull requests, which are listed by default.",
    ),
    (
        "issues/remove-label",
        "name",
        "The name of the label to remove.",
    ),
    (
        "issues/update-label",
        "name",
        "The label's current name. To rename it, pass new_name.",
    ),
    (
        "licenses/get",
        "license",
        "The license's key, such as mit or apache-2.0 (list_licenses lists them).",
    ),
    (
        "licenses/get-all-commonly-used",
        "featured",
        "true lists only the featured licenses, false only the others.",
    ),
    (
        "pulls/dismiss-review",
        "event",
        "DISMISS, the only value. It can be left out.",
    ),
    (
        "pulls/list-review-comments-for-repo",
        "sort",
        "What to sort the review comments by.",
    ),
    (
        "repos/create-commit-status",
        "sha",
        "The SHA of the commit the status is for.",
    ),
    (
        "repos/download-tarball-archive",
        "ref",
        "The branch, tag or commit SHA to archive, such as main or v1.0.0.",
    ),
    (
        "repos/download-zipball-archive",
        "ref",
        "The branch, tag or commit SHA to archive, such as main or v1.0.0.",
    ),
    (
        "repos/get-deployment-status",
        "status_id",
        "The ID of the deployment status.",
    ),
    (
        "repos/get-pages-build",
        "build_id",
        "The ID of the GitHub Pages build.",
    ),
    (
        "repos/get-webhook-delivery",
        "delivery_id",
        "The ID of the webhook delivery.",
    ),
    (
        "repos/update-release-asset",
        "state",
        "Not documented by GitHub (assets show uploaded). Leave it out.",
    ),
    (
        "repos/upload-release-asset",
        "label",
        "An alternate short description of the asset. Used in place of the filename.",
    ),
    (
        "repos/upload-release-asset",
        "name",
        "The asset's file name, such as app-linux.tar.gz.",
    ),
    (
        "users/check-following-for-user",
        "target_user",
        "The login of the user who may be followed.",
    ),
];

const USER_EMAIL: &str = "The email key in the following response is the publicly visible email address from your \
    GitHub profile page. When setting up your profile, you can select a primary email address to be public which \
    provides an email entry for this endpoint. If you do not set a public email address for email, then it will have \
    a value of null. You only see publicly visible email addresses when authenticated with GitHub. For more \
    information, see Authentication.\n\nThe Emails API enables you to list all of your email addresses, and toggle a \
    primary email to be visible publicly. For more information, see Emails API.";
const USER_EMAIL_FIX: &str =
    "The answer's email is the address the user made public on their profile, or null. \
    github_users.list_emails_for_authenticated_user lists all of your own addresses.";

/// Fixes to GitHub's descriptions: (operationId, GitHub's text, replacement). Checked live (2026-11-25):
/// matching-refs without a ref answers 404, and the archives without a ref redirect to the default branch.
pub const SENTENCE_FIXES: &[(&str, &str, &str)] = &[
    // GitHub named the tools by their page titles.
    (
        "agent-automations/create-repo-automation-task",
        "User tokens can fetch it with the \"Get a task by repo\" Agent tasks API. GitHub App installation tokens can \
         list tasks for the automation in the repository and match the returned task_id.",
        "User tokens can get the task with github_copilot.get_agent_task_for_repo; GitHub App installation tokens \
         can find it in github_copilot.list_repo_automation_tasks.",
    ),
    // "The following response" is GitHub's example, and the Emails API's toggle is left out.
    ("users/get-by-id", USER_EMAIL, USER_EMAIL_FIX),
    ("users/get-by-username", USER_EMAIL, USER_EMAIL_FIX),
    // A long list of categories with links to documentation, cut by the length limit before the note on rate.
    (
        "rate-limit/get",
        "Some categories of endpoints have custom rate limits that are separate from the rate limit governing the \
         other REST API endpoints. For this reason, the API response categorizes your rate limit. Under resources, \
         you'll see objects relating to different categories:\n\n\
         The core object provides your rate limit status for all non-search-related resources in the REST API.\n\n\
         The search object provides your rate limit status for the REST API for searching (excluding code \
         searches). For more information, see \"Search.\".\n\n\
         The code_search object provides your rate limit status for the REST API for searching code. For more \
         information, see github_search.search_code.\n\n\
         The graphql object provides your rate limit status for the GraphQL API. For more information, see \
         \"Resource limitations.\".\n\n\
         The integration_manifest object provides your rate limit status for the POST \
         /app-manifests/{code}/conversions operation. For more information, see \"Creating a GitHub App from a \
         manifest.\".\n\n\
         The dependency_snapshots object provides your rate limit status for submitting snapshots to the dependency \
         graph. For more information, see \"Dependency graph.\".\n\n\
         The dependency_sbom object provides your rate limit status for requesting SBOMs from the dependency graph. \
         For more information, see \"Dependency graph.\".\n\n\
         The actions_runner_registration object provides your rate limit status for registering self-hosted runners \
         in GitHub Actions. For more information, see \"Self-hosted runners.\".\n\n\
         The source_import object is no longer in use for any API endpoints, and it will be removed in the next API \
         version. For more information about API versions, see \"API Versions.\".\n\n\
         The rate object is closing down. If you're writing new API client code or updating existing code, you \
         should use the core object instead of the rate object. The core object contains the same information that \
         is present in the rate object.",
        "Some categories have their own rate limits, so the answer has one object per category under resources: \
         core (the REST API except search), search (search except code), code_search, graphql, \
         integration_manifest (creating a GitHub App from a manifest), dependency_snapshots and dependency_sbom \
         (the dependency graph), and actions_runner_registration (registering self-hosted runners). The top-level \
         rate object is closing down; use resources.core, which has the same values.",
    ),
    // GitHub copied the repository archive's sentence; the summary says what the tool does.
    (
        "actions/download-artifact",
        "Gets a redirect URL to download an archive for a repository.",
        "",
    ),
    // The ref input says it (optional_path).
    (
        "repos/download-tarball-archive",
        "If you omit :ref, the repository’s default branch (usually main) will be used.",
        "",
    ),
    (
        "repos/download-zipball-archive",
        "If you omit :ref, the repository’s default branch (usually main) will be used.",
        "",
    ),
    // Only a pointer to the pull request tool, whose own description says it.
    (
        "git/get-ref",
        "You need to explicitly request a pull request (github_issues_pulls.get_pull_request) to trigger a test \
         merge commit, which checks the mergeability of pull requests.",
        "",
    ),
    (
        "git/list-matching-refs",
        "You need to explicitly request a pull request (github_issues_pulls.get_pull_request) to trigger a test \
         merge commit, which checks the mergeability of pull requests.",
        "",
    ),
    // About GitHub's documentation pages, not the API.
    (
        "meta/get",
        "The values shown in the documentation's response are example values. You must always query the API \
         directly to get the latest values.",
        "",
    ),
    (
        "meta/get",
        " You should refer to the specific documentation for each feature to determine if IPv6 is supported.",
        "",
    ),
    (
        "repos/create-deployment-protection-rule",
        ", as well as the guide to creating custom deployment protection rules.",
        ".",
    ),
    (
        "git/list-matching-refs",
        "When you use this endpoint without providing a :ref, it will return an array of all the references from \
         your Git database, including notes and stashes if they exist on the server. Anything in the namespace is \
         returned, not just heads and tags.",
        "",
    ),
    (
        "repos/get-commit",
        "If there are more than 300 files in the commit diff and the default JSON media type is requested, the \
         response will include pagination link headers for the remaining files, up to a limit of 3000 files.",
        "If the commit changes more than 300 files, the others come in further pages (call again with next_page), \
         up to 3000 files.",
    ),
    // How to download with HTTP; download_release_asset does it.
    (
        "repos/get-release-asset",
        "To download the asset's binary content:",
        "",
    ),
    (
        "repos/get-release-asset",
        "If within a browser, fetch the location specified in the browser_download_url key provided in the \
         response.",
        "",
    ),
    (
        "repos/get-release-asset",
        "The API will either redirect the client to the location, or stream it directly if possible.",
        "",
    ),
    (
        "repos/get-release-asset",
        "API clients should handle both a 200 or 302 response.",
        "",
    ),
    // How to upload with HTTP; utcp does it from release_id and the file.
    (
        "repos/upload-release-asset",
        "The endpoint you call to upload release assets is specific to your release.",
        "",
    ),
    (
        "repos/upload-release-asset",
        "Everything else about the endpoint is the same as the rest of the API.",
        "",
    ),
    // GitHub wrote the field's name with a space.
    (
        "repos/list-webhooks",
        "last response may return null if there have not been any deliveries within 30 days.",
        "A webhook's last_response is null if there have been no deliveries within 30 days.",
    ),
    // The manuals have no GraphQL tool.
    (
        "search/users",
        " As an alternative, you can use the GraphQL API. The GraphQL API requires authentication and will return \
         private users, including Enterprise Managed Users (EMUs), that you are authorized to view.",
        "",
    ),
    // Only the default branch is searched; master is no longer the usual name.
    (
        "search/code",
        " In most cases, this will be the master branch.",
        "",
    ),
    (
        "search/code",
        "Considerations for code search:\n\nDue to the complexity of searching code, there are a few restrictions \
         on how searches are performed:",
        "Code search has limits:",
    ),
    // GitHub's typos.
    (
        "actions/generate-runner-jitconfig-for-repo",
        "need therepo scope",
        "need the repo scope",
    ),
    ("checks/rerequest-run", "apps recieving", "apps receiving"),
    ("repos/list-pages-builds", "Lists builts of", "Lists builds of"),
    // Paging by Link header, which utcp turns into next_page.
    (
        "actions/list-concurrency-groups-for-workflow-run",
        "The after cursor paginates forward only and does not emit a rel=\"prev\" Link; use before to page backward \
         from a forward page's next cursor.",
        "next_page holds the after cursor of the next page; to page backward, pass a page's after cursor as before.",
    ),
    // Without a Link header, utcp prints no next_page, which otherwise means the last page.
    (
        "issues/list-issue-artifacts",
        "The response is a bare array and does not include pagination metadata such as a Link header, a total \
         count, or the service's has_next_page signal.",
        "The answer has no next_page even when more pages exist: ask for the next page until one has fewer than \
         per_page artifacts.",
    ),
    // The visibility tool is left out (account security).
    (
        "users/list-public-emails-for-authenticated-user",
        ", which you can set with the Set primary email visibility for the authenticated user endpoint.",
        ".",
    ),
    (
        "issues/set-issue-field-values",
        "use the POST endpoint instead.",
        "use github_issues_pulls.add_issue_field_values instead.",
    ),
    // GitHub wrote the ref as a CLI flag, and its limits as a table that is too long to keep.
    (
        "code-scanning/upload-sarif",
        "There are two places where you can upload code scanning results.\n\n",
        "",
    ),
    (
        "code-scanning/upload-sarif",
        "for example --ref refs/pull/42/merge or --ref refs/pull/42/head",
        "for example with the ref refs/pull/42/merge or refs/pull/42/head",
    ),
    (
        "code-scanning/upload-sarif",
        "for example --ref refs/heads/my-branch",
        "for example with the ref refs/heads/my-branch",
    ),
    (
        "code-scanning/upload-sarif",
        "SARIF upload supports a maximum number of entries per the following data objects, and an analysis will be \
         rejected if any of these objects is above its maximum value. For some objects, there are additional values \
         over which the entries will be ignored while keeping the most important entries whenever applicable.",
        "An analysis is rejected if it has more than 20 runs per file, 25,000 results or rules per run, 100 tool \
         extensions per run, 10,000 thread flow locations or 1,000 locations per result, or 20 tags per rule. Of \
         those, only the top 5,000 results per run, 1,000 thread flow locations and 100 locations per result, and 10 \
         tags per rule are kept.",
    ),
    // The API, not what an agent does with the tool.
    (
        "pulls/merge-async",
        "This is the required API for merging stacked pull requests.",
        "Stacked pull requests can only be merged with this tool.",
    ),
    (
        "git/create-tree",
        "The tree creation API accepts nested entries.",
        "An entry's path can name a file in a subfolder (src/lib.rs).",
    ),
    // What a successful answer's status code means: utcp prints the answer (null when it has no content) and shows a
    // status only when the call fails, and it follows redirects. Checked live (2026-11-25) with utcp 0.2.5:
    // check-if-merged answers null for a merged pull request and fails with 404 otherwise.
    (
        "repos/get-pages-health-check",
        "The first request to this endpoint returns a 202 Accepted status and starts an asynchronous background task \
         to get the results for the domain. After the background task completes, subsequent requests to this \
         endpoint return a 200 OK status with the health check results in the response.",
        "The first call starts a background check and its answer is empty; call again after a few seconds for the \
         results.",
    ),
    (
        "pulls/check-if-merged",
        "The HTTP status of the response indicates whether or not the pull request has been merged; the response \
         body is empty.",
        "If it has been merged, the answer is null; if not, the call fails with 404 Not Found.",
    ),
    (
        "issues/check-user-can-be-assigned",
        "If the assignee can be assigned to issues in the repository, a 204 header with no content is \
         returned.\n\nOtherwise a 404 status code is returned.",
        "If the assignee can be assigned to issues in the repository, the answer is null; otherwise the call fails \
         with 404 Not Found.",
    ),
    (
        "issues/check-user-can-be-assigned-to-issue",
        "If the assignee can be assigned to this issue, a 204 status code with no content is returned.\n\nOtherwise \
         a 404 status code is returned.",
        "If the assignee can be assigned to this issue, the answer is null; otherwise the call fails with 404 Not \
         Found.",
    ),
    (
        "users/check-blocked",
        "Returns a 204 if the given user is blocked by the authenticated user. Returns a 404 if the given user is not \
         blocked by the authenticated user, or if the given user account has been identified as spam by GitHub.",
        "If the authenticated user has blocked the user, the answer is null. The call fails with 404 Not Found if the \
         user is not blocked, or if GitHub has identified the account as spam.",
    ),
    // Webhooks are left out of the manuals.
    (
        "issues/get",
        "The API returns a 301 Moved Permanently status if the issue was transferred to another repository. If the \
         issue was transferred to or deleted from a repository where the authenticated user lacks read access, the \
         API returns a 404 Not Found status. If the issue was deleted from a repository where the authenticated user \
         has read access, the API returns a 410 Gone status. To receive webhook events for transferred and deleted \
         issues, subscribe to the issues webhook.",
        "If the issue was transferred to another repository, the answer is the issue there. If it was transferred to \
         or deleted from a repository where the authenticated user lacks read access, the call fails with 404 Not \
         Found; if it was deleted from one where the user has read access, with 410 Gone.",
    ),
    (
        "pulls/merge-async",
        "A new asynchronous merge request returns a 202 response with a UUID that can be used to fetch the result of \
         the merge (github_issues_pulls.get_pull_request_merge_async_result). If another asynchronous merge request \
         is already pending for this pull request, a 409 response returns that request's UUID and merge options \
         instead.\n\nIf the pull request is already merged or already in a merge queue, a 200 response is returned \
         immediately. A merged result includes the merge commit OID. An enqueued result means the pull request was \
         added to the merge queue, not that it has merged.\n\nIf the pull request cannot be merged (e.g. because it \
         is closed, or still a draft) this result will be returned immediately with a 400 response status.",
        "The answer's status is pending while the merge runs in the background: pass its details.uuid to \
         github_issues_pulls.get_pull_request_merge_async_result for the result. It is merged (with the merge commit \
         OID) or enqueued when the pull request was already merged or in a merge queue; enqueued means it was added \
         to the merge queue, not that it has merged. If another asynchronous merge request is already pending, the \
         call fails with 409 Conflict and the error shows that request's UUID and merge options. If the pull request \
         cannot be merged (it is closed, or still a draft), the call fails with 400 Bad Request.",
    ),
    (
        "pulls/get-merge-async-result",
        "Once the asynchronous request completes, its status is one of:\n\nmerged: The pull request was merged into \
         the base branch. The response includes the merge commit OID.\n\nenqueued: The pull request was added to a \
         merge queue.\n\nfailed: The request failed. The response includes a message describing the failure.",
        "Once the asynchronous request completes, its status is merged (the pull request was merged into the base \
         branch, and the answer includes the merge commit OID), enqueued (it was added to a merge queue) or failed \
         (the answer includes a message describing the failure).",
    ),
    (
        "pull-request-stacks/unstack",
        "When pull requests remain in the stack, the updated stack is returned with a 200. When no pull requests \
         remain, the stack is dissolved and a 204 is returned.",
        "The answer is the updated stack, or null when no pull requests remain and the stack is dissolved.",
    ),
    (
        "code-scanning/commit-autofix",
        "If an autofix is committed as a result of this request, then this endpoint will return a 201 Created \
         response.",
        "The answer has the new commit's sha and its target_ref.",
    ),
    (
        "code-scanning/create-autofix",
        "If a new autofix is to be created as a result of this request or is currently being generated, then this \
         endpoint will return a 202 Accepted response.\n\nIf an autofix already exists for a given alert, then this \
         endpoint will return a 200 OK response.",
        "If an autofix already exists for the alert, the answer is that autofix. The answer's status is pending \
         while the autofix is generated; github_security.get_autofix shows when it is success, error or outdated, \
         and github_security.commit_autofix commits it.",
    ),
    (
        "dependency-graph/fetch-sbom-report",
        "When the SBOM is ready, the response is a 302 redirect to a temporary download URL for the SBOM in SPDX \
         JSON format. The generated SBOM report may be retained for up to one week from the original request. The \
         temporary download URL returned by this endpoint expires separately, and its expiry is set when the fetch \
         request is made.",
        "When the SBOM is ready, utcp saves it, in SPDX JSON format; until then the answer has no content (0 \
         bytes), so call again after a few seconds. The generated SBOM report may be retained for up to one week \
         from the original request.",
    ),
    (
        "orgs/list-for-authenticated-user",
        "Requests using a fine-grained access token will receive a 200 Success response with an empty list.",
        "With a fine-grained access token, the answer is an empty list.",
    ),
    (
        "activity/mark-notifications-as-read",
        "If the number of notifications is too large to complete in one request, you will receive a 202 Accepted \
         status and GitHub will run an asynchronous process to mark notifications as \"read.\"",
        "The answer is null, or a message when there are too many notifications to mark at once and GitHub marks \
         them in the background.",
    ),
    (
        "activity/mark-repo-notifications-as-read",
        "If the number of notifications is too large to complete in one request, you will receive a 202 Accepted \
         status and GitHub will run an asynchronous process to mark notifications as \"read.\"",
        "The answer is null, or a message when there are too many notifications to mark at once and GitHub marks \
         them in the background.",
    ),
    // A list flattened into fragments ("0-23: Hour of day.").
    (
        "repos/get-punch-card-stats",
        "Each array contains the day number, hour number, and number of commits:\n\n\
         0-6: Sunday - Saturday.\n\n\
         0-23: Hour of day.\n\n\
         Number of commits.",
        "The answer is a list of [day, hour, commits]: the day from 0 (Sunday) to 6 (Saturday), the hour from 0 to \
         23, and the number of commits.",
    ),
    // Restates the summary.
    (
        "activity/check-repo-is-starred-by-authenticated-user",
        "Whether the authenticated user has starred the repository.",
        "",
    ),
    (
        "repos/get-code-frequency-stats",
        "If the repository contains 10,000 or more commits, a 422 status code will be returned.",
        "With more, the call fails with 422.",
    ),
    // A list flattened into fragments ("w - Start of the week").
    (
        "repos/get-contributors-stats",
        "Returns the total number of commits authored by the contributor. In addition, the response includes a \
         Weekly Hash (weeks array) with the following information:\n\n\
         w - Start of the week, given as a Unix timestamp.\n\n\
         a - Number of additions.\n\n\
         d - Number of deletions.\n\n\
         c - Number of commits.",
        "Each item is a contributor: author, total (their number of commits) and weeks, a list of {w: the start \
         of the week as a Unix timestamp, a: additions, d: deletions, c: commits}.",
    ),
];

/// Sentences added to a tool description (operationId, sentences), kept when it is shortened.
pub const NOTES: &[(&str, &str)] = &[
    (
        "code-scanning/upload-sarif",
        "To check the upload's status, pass the answer's id as sarif_id to github_security.get_sarif.",
    ),
    (
        "code-scanning/get-codeql-database",
        "To download the database, use github_security.download_codeql_database.",
    ),
    (
        "pulls/get",
        "For the changed files and their patches, use github_issues_pulls.list_pull_request_files.",
    ),
    (
        "repos/get-content",
        "For a file's content as it is, instead of base64, use github_repos.get_raw_content.",
    ),
    (
        "repos/get-release-asset",
        "To download the asset, use github_repos.download_release_asset.",
    ),
    // GitHub says "the authenticated user"; agents search for "current user", "me", "who am I", "my user", "my
    // repositories" and "my notifications".
    (
        "users/get-authenticated",
        "The authenticated user is the account the token belongs to: who am I, my user, the current user, me.",
    ),
    (
        "repos/list-for-authenticated-user",
        "These are the current user's repositories (my repositories): their own and the ones shared with them.",
    ),
    (
        "activity/list-notifications-for-authenticated-user",
        "These are the current user's notifications (my notifications).",
    ),
    (
        "dependency-graph/generate-sbom-report",
        "To get the SBOM, pass the last part of the answer's sbom_url as sbom_uuid to \
         github_security.fetch_sbom_report. A 404 Not Found means the repository's dependency graph is off \
         (Settings, Code security).",
    ),
    (
        "dependency-graph/fetch-sbom-report",
        "github_security.generate_sbom_report starts the SBOM; its answer's sbom_url ends with the sbom_uuid.",
    ),
    (
        "dependency-graph/diff-range",
        "A 403 Forbidden means the repository's dependency graph is off (Settings, Code security).",
    ),
    // Statistics GitHub computes on the first request (checked live 2026-11-25: {} first, the data seconds later).
    ("repos/get-code-frequency-stats", STATS_NOTE),
    ("repos/get-commit-activity-stats", STATS_NOTE),
    ("repos/get-contributors-stats", STATS_NOTE),
    (
        "code-scanning/update-default-setup",
        "When the change starts a run, the answer has its run_id and run_url; otherwise it is {}.",
    ),
    (
        "code-quality/update-setup",
        "When the change starts a run, the answer has its run_id and run_url; otherwise it is {}.",
    ),
];

const STATS_NOTE: &str =
    "When GitHub has not computed the statistics yet, the answer is {} and GitHub starts \
    computing them; call again after a few seconds. An empty repository answers null.";

/// Summaries that replace GitHub's (operationId, summary) as the first sentence of a tool description, the one
/// `utcp search` shows, in the words people use for the task. Tool names do not change.
pub const SUMMARIES: &[(&str, &str)] = &[
    // GitHub's "Get repository content" hides that it reads a file ("file contents", "read a file").
    (
        "repos/get-content",
        "Get the contents of a file or directory in a repository",
    ),
    // People open an issue; utcp search matches whole words, so "Opens" later on would not be found.
    ("issues/create", "Create (open) an issue"),
];

/// Tools that ask for another media type than JSON. Checked live (2026-11-25) with utcp 0.2.5.
fn variants() -> Vec<Variant> {
    let v =
        |op_id: &str, name: &str, accept: &str, description: &str, output: Option<&str>| Variant {
            op_id: op_id.into(),
            name: name.into(),
            accept: accept.into(),
            description: description.into(),
            file: output.is_none(),
            output: output.unwrap_or_default().into(),
        };
    vec![
        v(
            "repos/get-content",
            "get_raw_content",
            "application/vnd.github.raw+json",
            "Get a file's raw content. Prints a text file as it is, instead of base64 inside JSON as \
             github_repos.get_content does; a JSON file prints as JSON, and a binary file is saved. GitHub \
             sends no file name, so give one with --out (for example --out logo.png); otherwise the file is \
             saved as get_raw_content.json whatever it holds. For a directory, prints its entries as \
             github_repos.get_content does. Files up to 100 MB.",
            Some(
                "The file's content: text, JSON for a JSON file, or for a binary file utcp's file reference \
                 {\"type\": \"file\", \"path\", \"mime_type\", \"bytes\", \"filename\"}, whose mime_type is GitHub's \
                 application/vnd.github.raw+json, not the file's type. For a directory, the list of its entries.",
            ),
        ),
        v(
            "repos/get-release-asset",
            "download_release_asset",
            "application/octet-stream",
            "Download a release asset. Saves the asset's file; github_repos.list_release_assets lists a \
             release's assets with their IDs.",
            None,
        ),
        v(
            "code-scanning/get-codeql-database",
            "download_codeql_database",
            "application/zip",
            "Download a CodeQL database for a repository. Saves a language's CodeQL database as a zip file; \
             github_security.list_codeql_databases lists the languages that have one. Classic tokens need the \
             repo scope with private or public repositories, or the public_repo scope with only public \
             repositories.",
            None,
        ),
    ]
}

/// Writes that worked in live tests against a throwaway repository of the token's owner (github-manuals 0.1.0, API
/// 2026-03-10). The others are marked "(untested)" and tagged `untested`.
pub const TESTED_WRITES: &[&str] = &[
    "actions/create-registration-token-for-repo",
    "actions/create-remove-token-for-repo",
    "actions/create-workflow-dispatch",
    "actions/delete-artifact",
    "actions/delete-workflow-run",
    "actions/delete-workflow-run-logs",
    "actions/disable-workflow",
    "actions/enable-workflow",
    "activity/delete-repo-subscription",
    "activity/mark-repo-notifications-as-read",
    "activity/set-repo-subscription",
    "activity/star-repo-for-authenticated-user",
    "activity/unstar-repo-for-authenticated-user",
    "agent-merge/delete-agent-merge-enrollment",
    "agent-merge/put-agent-merge-enrollment",
    "agent-tasks/create-task-in-repo",
    "checks/set-suites-preferences",
    "copilot/update-repository-code-review-settings",
    "dependency-graph/create-repository-snapshot",
    "git/create-blob",
    "git/create-commit",
    "git/create-ref",
    "git/create-tag",
    "git/create-tree",
    "git/delete-ref",
    "git/update-ref",
    "issues/add-assignees",
    "issues/add-blocked-by-dependency",
    "issues/add-labels",
    "issues/add-relates-to",
    "issues/add-sub-issue",
    "issues/create",
    "issues/create-comment",
    "issues/create-label",
    "issues/create-milestone",
    "issues/delete-comment",
    "issues/delete-label",
    "issues/delete-milestone",
    "issues/lock",
    "issues/pin-comment",
    "issues/remove-all-labels",
    "issues/remove-assignees",
    "issues/remove-dependency-blocked-by",
    "issues/remove-label",
    "issues/remove-relates-to",
    "issues/remove-sub-issue",
    "issues/reprioritize-sub-issue",
    "issues/set-labels",
    "issues/unlock",
    "issues/unpin-comment",
    "issues/update",
    "issues/update-comment",
    "issues/update-label",
    "issues/update-milestone",
    "markdown/render",
    "markdown/render-raw",
    "pulls/create",
    "pulls/create-reply-for-review-comment",
    "pulls/create-review",
    "pulls/create-review-comment",
    "pulls/delete-pending-review",
    "pulls/delete-review-comment",
    "pulls/merge",
    "pulls/request-copilot-code-review",
    "pulls/submit-review",
    "pulls/update",
    "pulls/update-branch",
    "pulls/update-review",
    "pulls/update-review-comment",
    "reactions/create-for-commit-comment",
    "reactions/create-for-issue",
    "reactions/create-for-issue-comment",
    "reactions/create-for-pull-request-review-comment",
    "reactions/create-for-release",
    "reactions/delete-for-commit-comment",
    "reactions/delete-for-issue",
    "reactions/delete-for-issue-comment",
    "reactions/delete-for-pull-request-comment",
    "reactions/delete-for-release",
    "repos/create-autolink",
    "repos/create-commit-comment",
    "repos/create-commit-status",
    "repos/create-deployment",
    "repos/create-deployment-branch-policy",
    "repos/create-deployment-status",
    "repos/create-dispatch-event",
    "repos/create-or-update-environment",
    "repos/create-or-update-file-contents",
    "repos/create-pages-site",
    "repos/create-release",
    "repos/delete-an-environment",
    "repos/delete-autolink",
    "repos/delete-commit-comment",
    "repos/delete-deployment",
    "repos/delete-deployment-branch-policy",
    "repos/delete-file",
    "repos/delete-pages-site",
    "repos/delete-release",
    "repos/delete-release-asset",
    "repos/enable-automated-security-fixes",
    "repos/enable-private-vulnerability-reporting",
    "repos/enable-vulnerability-alerts",
    "repos/generate-release-notes",
    "repos/merge",
    "repos/rename-branch",
    "repos/replace-all-topics",
    "repos/request-pages-build",
    "repos/update-commit-comment",
    "repos/update-deployment-branch-policy",
    "repos/update-information-about-pages-site",
    "repos/update-release",
    "repos/update-release-asset",
    "repos/upload-release-asset",
    "security-advisories/create-repository-advisory",
    "security-advisories/create-repository-advisory-comment",
    "security-advisories/update-repository-advisory",
    "security-advisories/update-repository-advisory-comment",
];

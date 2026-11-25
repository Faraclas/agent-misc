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
    /// (operationId, path parameter) pairs an agent may leave out; utcp then drops that part of the URL
    /// (`/contents/{path}` -> `/contents`, the repository root).
    pub optional_path: Vec<(String, String)>,
    /// Leave out every write.
    pub read_only: bool,
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
            optional_path: vec![("repos/get-content".into(), "path".into())],
            read_only: false,
        }
    }
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

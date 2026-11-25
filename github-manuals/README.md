# github-manuals

Generates [utcp](../utcp-rs/) HTTP manuals for the GitHub REST API, so agents call GitHub through utcp
(`search` -> `schema` -> `call`) instead of loading an MCP server's tool list into every conversation.

- **Source:** GitHub's own OpenAPI description, [`github/rest-api-description`](https://github.com/github/rest-api-description)
  (MIT), file `descriptions/api.github.com/api.github.com.2026-03-10.json` at a pinned commit. The generator downloads
  it from `raw.githubusercontent.com` and checks it against the SHA-256 in [`pins.json`](./pins.json), which is built
  into the binary. Nothing from GitHub is stored in this repository.
- **Output:** seven manuals with 605 tools for REST API version `2026-03-10` (every tool sends
  `X-GitHub-Api-Version: 2026-03-10`), and a `report.json` listing every tool and every operation left out with the
  reason. The manuals name no user or organization: one set works for any account.
- **Requirements:** one static binary, no runtime; utcp 0.2.4 or newer (earlier versions cannot send the bodies
  of some tools, silently return only the first page of a list, or cannot promote the manuals by the `'github_*'`
  pattern; `generate --stage` refuses them). Network
  access to `raw.githubusercontent.com` for the first run (one 14 MB download); later runs use the cache and take
  about a second.
- **Docs:** this README (installation, first setup, what is left out), the [operating guide](./docs/operating.md)
  (the token, upgrades, troubleshooting) and the [specification](./docs/github-manuals-spec.md) (how the manuals are
  built).

## Installation

- **Linux (x86_64):** the project's GitHub Releases (tags `github-manuals-v*`) have
  `github-manuals-x86_64-unknown-linux-musl.tar.gz`, a static binary that runs on any distribution, provided as-is.
  Copy `github-manuals` to `~/.local/bin/`.
- **Windows:** the release zip `github-manuals-x86_64-pc-windows-msvc.zip` holds `github-manuals.exe`. Put it in a
  folder on your `PATH`, for example next to `utcp.exe` in `%LOCALAPPDATA%\Programs\utcp`.
- **From source:**

  ```sh
  cd github-manuals
  cargo build --release
  cargo test
  install -Dm755 target/release/github-manuals ~/.local/bin/github-manuals
  ```

## The manuals

| Manual | Tools | GitHub areas (`x-github` category) | Hint for `--hint` |
| --- | --- | --- | --- |
| `github_repos` | 133 | repos, branches, commits, git, releases, collaborators, deploy keys, metrics, licenses, gitignore, codes of conduct | GitHub repos, files, branches, commits, git, releases |
| `github_issues_pulls` | 138 | issues, pulls, reactions, projects; issue types, issue fields, planning statuses | GitHub issues, pull requests, reviews, labels, projects |
| `github_actions` | 144 | actions, checks, deployments and environments, packages, pages | GitHub Actions, checks, environments, packages, Pages |
| `github_security` | 66 | code scanning, Dependabot, secret scanning, security advisories, code quality, dependency graph, code security, attestations | GitHub code and secret scanning, Dependabot, advisories |
| `github_copilot` | 31 | Copilot agent tasks and automations, agent secrets and variables (read), Copilot repository settings, agent merge, Copilot code review | GitHub Copilot agent tasks, automations, settings |
| `github_search` | 7 | search | GitHub search: code, issues, PRs, repos, commits, users |
| `github_users` | 86 | users, activity (notifications, stars, watching, events), your orgs and teams, apps you installed, your billing usage, meta, rate limit, markdown, emojis | GitHub users, notifications, stars, orgs, billing, meta |

Issues and pull requests share a manual because a pull request's conversation comments are issue comments.

Tool names follow GitHub's operation names in snake_case: `repos/create-or-update-file-contents` is
`github_repos.create_or_update_file_contents`. A bare verb gets its noun (`repos/get` is `get_repo`), and the area is
added where the name would be unclear (`pulls/list-files` is `list_pull_request_files`, `dependabot/get-alert` is
`get_dependabot_alert`). Arguments are GitHub's own: path parameters, query parameters and the fields of the request
body side by side (`create_issue` takes `owner`, `repo`, `title`, `body`, `labels`, ...). Writes not yet covered by a
live write test start their description with `(untested)`. In 0.1.0, 117 of the 222 writes worked against a throwaway
repository. Most of the others need what one personal repository does not have: an organization, a GitHub App token
(check runs), a self-hosted runner, packages, security alerts, forks or templates, a paid plan, or workflow runs that
GitHub starts (the test runs stayed queued). A few were not tried because they reach beyond the test repository:
following users, marking all notifications read, requesting a CVE.

## Usage

```sh
github-manuals generate --stage
```

This writes the manuals and `report.json` to `~/.local/share/github-manuals` (Windows:
`%LOCALAPPDATA%\github-manuals\manuals`), stages them in utcp, prints how many operations were left out for each
reason, and prints the next steps:

1. Give utcp the token as the secret `GITHUB_MANUALS_PAT` (see [The token](#the-token)), then check it with
   `utcp doctor`.
2. See what changes for your agents: `utcp diff 'github_*'`. Try a tool:
   `utcp call --staged github_users.get_authenticated_user`.
3. Make them active: `utcp promote 'github_*'`.

The pattern names only these seven manuals, so manuals that others staged stay staged. `--read-only` leaves out every
write tool. After upgrading github-manuals, run `generate --stage` again and check `utcp diff 'github_*'` before
promoting.

## The token

The manuals send `Authorization: Bearer ${GITHUB_MANUALS_PAT}`. Use a token of its own under that name, not
`GITHUB_TOKEN` (which many tools read for other purposes).

**The token decides what agents may do.** The manuals include every operation that is safe to offer (see the next
section for what they leave out); what an agent can actually do with them is what the token allows.

- **A classic personal access token** is what these manuals are made for. It can write to public repositories you
  do not belong to (opening issues and pull requests upstream), and it works for Packages, Checks, Projects owned by
  your account, notifications and Copilot agent tasks.
- **A fine-grained token** can be limited to some repositories and permissions, but it cannot write to public
  repositories where you are not a member, and GitHub does not accept it for Packages, the Checks API, user-owned
  Projects, notifications, watching, or Copilot agent tasks and automations. Those tools then answer 403.

Classic scopes that GitHub's descriptions name for the included tools: `repo` (or `public_repo` for public
repositories only), `workflow`, `security_events`, `read:packages`, `write:packages`, `delete:packages`,
`read:org`, `user` (covers `read:user`, `user:email`, `user:follow`), `notifications`, `project`, `read:public_key`,
`read:gpg_key`, `read:ssh_signing_key`, `read:repo_hook`. The descriptions name no scope for about 385 of the 605
tools, so a call that answers 403 or 404 may need another scope. Leaving out `delete_repo` is recommended (the
manuals cannot delete a repository anyway).

## What is left out, and why

`generate` prints the count per reason; `report.json` lists every operation. Of the 1,281 operations in the
description, 605 are tools. The others:

| Left out | Why |
| --- | --- |
| Organization administration (`/orgs/...`, except "get an organization" and "list organization repositories"), org billing | Not covered yet. Repository operations on organization-owned repositories (`/repos/...`) are included. |
| Enterprise administration (`/enterprises/...`, enterprise teams, hosted compute, announcement banners) | Not an enterprise admin's tool set. |
| Deprecated operations | GitHub has scheduled their removal. |
| Codespaces, Copilot Spaces, gists, GitHub Classroom | Not used. |
| Interaction limits, blocking users, revoking someone's leaked credentials | Moderation, not agent work. |
| Migrations | A migration can lock repositories. |
| GitHub App operations that need the app's own token (JWT), and adding or removing repositories of an app installation | A personal token cannot call them, or they grant an app access. |
| Teams, except "list your teams"; Copilot organization settings | Organization administration. |
| Deleting or transferring a repository | Cannot be undone. Delete repositories yourself. |
| Updating repository settings (`PATCH /repos/{owner}/{repo}`) | One operation changes everyday fields and dangerous ones together: visibility (publishing a private repository), archiving, renaming, security settings. utcp passes any argument through, so the dangerous fields cannot be removed from the tool. |
| Adding or removing collaborators, changing invitations, deploy keys | They grant access to a repository. Listing them is included. |
| Your account's SSH, GPG and signing keys, emails, social accounts, profile | Account security. Listing them is included. |
| Creating, changing, testing or deleting webhooks | A webhook sends repository events, and can send secrets, to any URL. Listing webhooks and deliveries is included. |
| Changing branch protection and rulesets | They are the guards that stop an agent's mistakes. Reading them is included. |
| Changing Actions permissions, policies and OIDC settings | Security settings: allowed actions, the default token permissions, fork pull request approval. Reading them is included. |
| Setting or deleting secrets (Actions, Dependabot, environments, Copilot agent) | A secret value has to be encrypted with the repository's public key first, which a plain HTTP call cannot do; use `gh secret set`. Overwriting loses the old value. Listing secret names is included. |
| Creating, changing or deleting variables (Actions, environments, Copilot agent) | The variables in use switch off or widen the Copilot agent's firewall; a write could open the agent's network unnoticed. Reading them is included. |
| Turning off vulnerability alerts, automated security fixes, private vulnerability reporting, immutable releases | Turning a security feature off. Turning them on is included. |
| Push protection bypass; changing or deleting secret scanning patterns | They let a detected secret through. Creating patterns is included. |

### What leaving out repository settings gives up

Without `PATCH /repos/{owner}/{repo}`, agents cannot change on an existing repository: its name, description,
homepage, feature toggles (issues, projects, wiki, discussions), merge settings (allowed merge methods, auto-merge,
delete branch on merge, commit message defaults), or the default branch (switching it to another existing branch;
renaming the default branch with `rename_branch` is included). Creating a repository sets most of these, and
`replace_all_topics` sets topics. Change the rest in the repository's settings page.

### What leaving out is not

Leaving an operation out keeps agents from finding it through `utcp search` and from doing it by accident. It is not
a security boundary: the same token works with `gh api` and `curl`, and git with your credentials can push or delete
branches. Limit the token itself (scopes, a fine-grained token, or a separate account) for that.

### Deletes

Everyday deletes are included: branches and tags (refs), releases and release assets, comments, labels, milestones,
reactions, workflow runs and logs, artifacts, caches, package versions, environments, deployments. Every DELETE tool's
description starts with "Delete only what the user asked for or what you created." Deleting a release asset breaks
anything that downloads it (an ebuild, an install script).

### Copilot agent automations

`github_copilot` includes creating and editing automations, which run the Copilot agent on events such as a new issue
or a pull request comment. An automation with `require_actor_write_permission: false` and an event trigger on a public
repository runs for anyone who opens an issue or writes a comment: a stranger's text becomes the agent's prompt, with
the automation's write access. Keep the default (`true`) on public repositories.

## Coming from the hand-written `github` manual

The earlier hand-written manual (`github`, 23 tools, `GITHUB_TOKEN`, API version 2022-11-28) maps to these tools.
utcp has no aliases, so agents use the new names.

| Old tool | New tool |
| --- | --- |
| `add_issue_comment` | `github_issues_pulls.create_issue_comment` |
| `create_issue` | `github_issues_pulls.create_issue` |
| `create_pull_request` | `github_issues_pulls.create_pull_request` |
| `get_commit` | `github_repos.get_commit` |
| `get_file_contents` | `github_repos.get_content` |
| `get_issue` | `github_issues_pulls.get_issue` |
| `get_job_logs` | `github_actions.download_job_logs_for_workflow_run` (saves the log as a file) |
| `get_latest_release` | `github_repos.get_latest_release` |
| `get_me` | `github_users.get_authenticated_user` |
| `get_pull_request` | `github_issues_pulls.get_pull_request` |
| `get_repo` | `github_repos.get_repo` |
| `get_workflow_run` | `github_actions.get_workflow_run` |
| `list_branches` | `github_repos.list_branches` |
| `list_commits` | `github_repos.list_commits` |
| `list_issues` | `github_issues_pulls.list_issues_for_repo` (`list_assigned_issues` lists issues assigned to you) |
| `list_pull_requests` | `github_issues_pulls.list_pull_requests` |
| `list_releases` | `github_repos.list_releases` |
| `list_workflow_run_jobs` | `github_actions.list_jobs_for_workflow_run` |
| `list_workflow_runs` | `github_actions.list_workflow_runs_for_repo` |
| `list_workflows` | `github_actions.list_repo_workflows` |
| `search_code` | `github_search.search_code` |
| `search_issues` | `github_search.search_issues_and_pull_requests` |
| `search_repositories` | `github_search.search_repos` |

Remove the old manual once the new ones are promoted and tried: `utcp deregister github`.

## Maintainers

- `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` (offline; set
  `UTCP_BIN` to a utcp binary to include the load test).
- Moving to a newer commit of the description: `github-manuals pins --commit <sha>` rewrites `pins.json`; rebuild,
  regenerate, review `utcp diff`. A new API version also changes `config::API_VERSION`.
- Contributor rules: [`AGENTS.md`](./AGENTS.md).

## License

MIT (see [`LICENSE`](../LICENSE)). The generated manuals copy operation and parameter descriptions from GitHub's
MIT-licensed description; see [`NOTICE`](./NOTICE).

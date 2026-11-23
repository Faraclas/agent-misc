//! PAT scope ids (`vso.code_write`) as named on the Azure DevOps "New Token" page ("Code", "Read & write"), so people
//! can pick the permissions there without knowing the ids.

/// Scope id, permission name, access level. Names and levels as the New Token page shows them (2026-11). Entries
/// marked "not checked" follow Microsoft's scope documentation but were not yet compared with the page.
const PERMISSIONS: &[(&str, &str, &str)] = &[
    ("vso.advsec", "Advanced Security", "Read"),
    ("vso.advsec_write", "Advanced Security", "Read & write"),
    (
        "vso.advsec_manage",
        "Advanced Security",
        "Read, write, & manage",
    ),
    ("vso.agentpools", "Agent Pools", "Read"),
    ("vso.agentpools_manage", "Agent Pools", "Read & manage"),
    ("vso.analytics", "Analytics", "Read"),
    // not checked
    ("vso.auditlog", "Auditing", "Read Audit Log"),
    // not checked
    (
        "vso.auditstreams_manage",
        "Auditing",
        "Manage Audit Streams",
    ),
    ("vso.build", "Build", "Read"),
    ("vso.build_execute", "Build", "Read & execute"),
    ("vso.code", "Code", "Read"),
    ("vso.code_write", "Code", "Read & write"),
    ("vso.code_manage", "Code", "Read, write, & manage"),
    ("vso.code_full", "Code", "Full"),
    ("vso.code_status", "Code", "Status"),
    ("vso.dashboards", "Team Dashboard", "Read"),
    ("vso.dashboards_manage", "Team Dashboard", "Read & manage"),
    ("vso.environment_manage", "Environment", "Read & manage"),
    // not checked
    ("vso.extension", "Extensions", "Read"),
    // not checked
    ("vso.extension_manage", "Extensions", "Read & manage"),
    ("vso.graph", "Graph", "Read"),
    ("vso.graph_manage", "Graph", "Read & manage"),
    ("vso.identity", "Identity", "Read"),
    ("vso.identity_manage", "Identity", "Read & manage"),
    (
        "vso.machinegroup_manage",
        "Deployment Groups",
        "Read & manage",
    ),
    // not checked
    (
        "vso.memberentitlementmanagement",
        "Member Entitlement Management",
        "Read",
    ),
    // not checked
    (
        "vso.memberentitlementmanagement_write",
        "Member Entitlement Management",
        "Read & write",
    ),
    // not checked
    ("vso.notification", "Notifications", "Read"),
    // not checked
    ("vso.notification_write", "Notifications", "Write"),
    // not checked
    ("vso.notification_manage", "Notifications", "Manage"),
    // not checked
    (
        "vso.notification_diagnostics",
        "Notifications",
        "Diagnostics",
    ),
    ("vso.packaging", "Packaging", "Read"),
    ("vso.packaging_write", "Packaging", "Read & write"),
    ("vso.packaging_manage", "Packaging", "Read, write, & manage"),
    // not checked
    ("vso.pipelineresources_use", "Pipeline Resources", "Use"),
    // not checked
    (
        "vso.pipelineresources_manage",
        "Pipeline Resources",
        "Use and manage",
    ),
    ("vso.profile", "User Profile", "Read"),
    ("vso.profile_write", "User Profile", "Read & write"),
    ("vso.project", "Project and Team", "Read"),
    ("vso.project_write", "Project and Team", "Read & write"),
    (
        "vso.project_manage",
        "Project and Team",
        "Read, write, & manage",
    ),
    ("vso.release", "Release", "Read"),
    ("vso.release_execute", "Release", "Read, write, & execute"),
    (
        "vso.release_manage",
        "Release",
        "Read, write, execute, & manage",
    ),
    ("vso.securefiles_read", "Secure Files", "Read"),
    ("vso.securefiles_write", "Secure Files", "Read & create"),
    (
        "vso.securefiles_manage",
        "Secure Files",
        "Read, create, & manage",
    ),
    // not checked
    ("vso.security_manage", "Security", "Manage"),
    ("vso.serviceendpoint", "Service Connections", "Read"),
    (
        "vso.serviceendpoint_query",
        "Service Connections",
        "Read & query",
    ),
    (
        "vso.serviceendpoint_manage",
        "Service Connections",
        "Read, query, & manage",
    ),
    // not checked
    ("vso.symbols", "Symbols", "Read"),
    // not checked
    ("vso.symbols_write", "Symbols", "Read & write"),
    // not checked
    ("vso.symbols_manage", "Symbols", "Read, write, & manage"),
    ("vso.taskgroups_read", "Task Groups", "Read"),
    ("vso.taskgroups_write", "Task Groups", "Read & create"),
    (
        "vso.taskgroups_manage",
        "Task Groups",
        "Read, create, & manage",
    ),
    ("vso.test", "Test Management", "Read"),
    ("vso.test_write", "Test Management", "Read & write"),
    ("vso.threads_full", "Pull Request Threads", "Read & write"),
    ("vso.variablegroups_read", "Variable Groups", "Read"),
    (
        "vso.variablegroups_write",
        "Variable Groups",
        "Read & create",
    ),
    (
        "vso.variablegroups_manage",
        "Variable Groups",
        "Read, create, & manage",
    ),
    ("vso.wiki", "Wiki", "Read"),
    ("vso.wiki_write", "Wiki", "Read & write"),
    ("vso.work", "Work Items", "Read"),
    ("vso.work_write", "Work Items", "Read & write"),
    ("vso.work_full", "Work Items", "Read, write, & manage"),
];

/// The New Token page's permission name and access level for a scope id, if known.
pub fn permission(scope: &str) -> Option<(&'static str, &'static str)> {
    PERMISSIONS
        .iter()
        .find(|(id, _, _)| *id == scope)
        .map(|(_, name, level)| (*name, *level))
}

/// `Code: Read & write`, or the scope id itself when the page name is not known.
pub fn label(scope: &str) -> String {
    match permission(scope) {
        Some((name, level)) => format!("{name}: {level}"),
        None => scope.to_string(),
    }
}

/// (name, level) rows for a list of scope ids, sorted by name as on the New Token page. An unknown scope id is shown
/// as its own name with an empty level. Scopes with the same name (Code: Read & write, Code: Status) share a row,
/// their levels joined with " + ".
pub fn rows<'a>(scopes: impl IntoIterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut merged: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for s in scopes {
        let (name, level) = match permission(s) {
            Some((name, level)) => (name.to_string(), level.to_string()),
            None => (s.to_string(), String::new()),
        };
        let levels = merged.entry(name).or_default();
        if !level.is_empty() && !levels.contains(&level) {
            levels.push(level);
        }
    }
    merged
        .into_iter()
        .map(|(name, levels)| (name, levels.join(" + ")))
        .collect()
}

/// (name, level for every tool, level for the reads) rows; "-" when the reads need no such permission.
pub fn table<'a>(
    all: impl IntoIterator<Item = &'a str>,
    reads: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, String, String)> {
    let reads: std::collections::BTreeMap<String, String> = rows(reads).into_iter().collect();
    let mut all: std::collections::BTreeMap<String, String> = rows(all).into_iter().collect();
    for name in reads.keys() {
        all.entry(name.clone()).or_insert_with(|| "-".to_string());
    }
    all.into_iter()
        .map(|(name, level)| {
            let read = reads.get(&name).cloned().unwrap_or_else(|| "-".to_string());
            (name, level, read)
        })
        .collect()
}

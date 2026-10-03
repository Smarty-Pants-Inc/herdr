//! Pure client-side grouping. Keys describe presentation, never endpoint routing.
//! Cache this projection on snapshot/config changes rather than rebuilding in render.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{ClientEndpointId, ClientEndpointStatus, ClientShellEndpoint, ClientShellWorkspace};
use crate::config::SidebarGroupingConfig;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub(super) enum GroupKey {
    Org(String),
    Project(String, String),
    OrgAgents(String),
    Ungrouped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GroupedRow {
    Heading {
        key: GroupKey,
        label: String,
        depth: u16,
    },
    Workspace {
        endpoint: usize,
        index: usize,
        depth: u16,
        label: String,
        host: String,
    },
}

#[derive(Clone, Copy)]
struct Identity<'a> {
    id: &'a str,
    label: &'a str,
}

#[derive(Clone, Copy)]
enum Metadata<'a> {
    Missing,
    Invalid,
    Org(Identity<'a>),
    Project {
        org: Identity<'a>,
        project: Identity<'a>,
        lane: Option<&'a str>,
        purpose: Option<&'a str>,
        issue: Option<&'a str>,
        root: bool,
    },
}

fn token_keys(config: &SidebarGroupingConfig) -> [&str; 10] {
    [
        &config.org_id,
        &config.org_label,
        &config.project_id,
        &config.project_label,
        &config.lane_id,
        &config.lane_purpose,
        &config.issue,
        &config.host,
        &config.role,
        &config.lead,
    ]
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn identity<'a>(id: Option<&'a str>, label: Option<&'a str>) -> Result<Option<Identity<'a>>, ()> {
    match (id, label) {
        (None, None) => Ok(None),
        (Some(id), Some(label)) if valid_slug(id) && !label.trim().is_empty() => {
            Ok(Some(Identity { id, label }))
        }
        _ => Err(()),
    }
}

fn metadata<'a>(workspace: &'a ClientShellWorkspace, keys: &[&str; 10]) -> Metadata<'a> {
    let mut values = [None; 10];
    for (key, value) in &workspace.tokens {
        for (slot, expected) in keys.iter().enumerate() {
            if key != expected {
                continue;
            }
            if value.chars().count() > 80
                || value.chars().any(char::is_control)
                || values[slot].is_some_and(|previous| previous != value.as_str())
            {
                return Metadata::Invalid;
            }
            values[slot] = Some(value.as_str());
        }
    }
    // Empty values are cleared metadata, but still participated in duplicate checks.
    let values = values.map(|value| value.filter(|value| !value.is_empty()));
    if values.iter().all(Option::is_none) {
        return Metadata::Missing;
    }
    let Ok(Some(org)) = identity(values[0], values[1]) else {
        return Metadata::Invalid;
    };
    let Ok(project) = identity(values[2], values[3]) else {
        return Metadata::Invalid;
    };
    let lane = values[4];
    if lane.is_some_and(|lane| !valid_slug(lane)) || (values[5].is_some() && lane.is_none()) {
        return Metadata::Invalid;
    }
    if values[8] == Some("org-agent") {
        return if project.is_none() && lane.is_none() {
            Metadata::Org(org)
        } else {
            Metadata::Invalid
        };
    }
    let Some(project) = project else {
        return Metadata::Invalid;
    };
    let root = match values[8] {
        None => {
            lane.is_none()
                && workspace
                    .worktree
                    .as_ref()
                    .is_none_or(|worktree| !worktree.is_linked_worktree)
        }
        Some("project-agent") if lane.is_none() => true,
        Some("worktree-agent") if lane.is_some() => false,
        _ => return Metadata::Invalid,
    };
    Metadata::Project {
        org,
        project,
        lane,
        purpose: values[5],
        issue: values[6],
        root,
    }
}

#[derive(Default)]
struct HeadingLabel {
    first: Option<String>,
    conflicting: bool,
}

impl HeadingLabel {
    fn observe(&mut self, label: &str) {
        match &self.first {
            Some(first) if first != label => self.conflicting = true,
            None => self.first = Some(label.to_owned()),
            _ => {}
        }
    }

    fn resolve(self, id: &str) -> String {
        if self.conflicting {
            id.to_owned()
        } else {
            self.first.unwrap_or_else(|| id.to_owned())
        }
    }
}

struct Leaf<'a> {
    endpoint: usize,
    index: usize,
    depth: u16,
    label: String,
    host: String,
    root: bool,
    lane: &'a str,
    workspace: &'a ClientShellWorkspace,
    endpoint_id: &'a ClientEndpointId,
}

#[derive(Default)]
struct ProjectRows<'a> {
    label: HeadingLabel,
    leaves: Vec<Leaf<'a>>,
}

#[derive(Default)]
struct OrgRows<'a> {
    label: HeadingLabel,
    agents: Vec<Leaf<'a>>,
    projects: BTreeMap<String, ProjectRows<'a>>,
}

fn host_label(endpoint: &ClientShellEndpoint) -> String {
    if endpoint.endpoint_id.is_local() {
        return "Local".to_owned();
    }
    let Some(target) = endpoint.connection_target.as_deref() else {
        return "Remote".to_owned();
    };
    let uri_authority = target.strip_prefix("ssh://");
    let authority = uri_authority.unwrap_or(target);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // Only peel a syntactically valid numeric port. Paths, lookalike domains,
    // malformed ports, and IPv6 literals must not become a guessed fleet alias.
    let hostname = host_port
        .rsplit_once(':')
        .filter(|(_, port)| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        })
        .map_or(host_port, |(host, _)| host);
    let normalized = hostname.to_ascii_lowercase();
    let alias = normalized
        .strip_suffix(".smartypants.ai")
        .unwrap_or(&normalized);
    match alias {
        "ryzen1" => "Ryzen 1".to_owned(),
        "ryzen2" => "Ryzen 2".to_owned(),
        "ryzen3" => "Ryzen 3".to_owned(),
        "ryzen4" => "Ryzen 4".to_owned(),
        "m5" => "m5".to_owned(),
        // Preserve an unknown URI exactly; a plain target still omits user@.
        _ => uri_authority.map_or(host_port, |_| target).to_owned(),
    }
}

fn append_leaves(rows: &mut Vec<GroupedRow>, mut leaves: Vec<Leaf<'_>>) {
    leaves.sort_by(|left, right| {
        (
            !left.root,
            &left.label,
            left.lane,
            &left.workspace.label,
            left.endpoint_id,
            &left.workspace.workspace_id,
            left.endpoint,
            left.index,
        )
            .cmp(&(
                !right.root,
                &right.label,
                right.lane,
                &right.workspace.label,
                right.endpoint_id,
                &right.workspace.workspace_id,
                right.endpoint,
                right.index,
            ))
    });
    rows.extend(leaves.into_iter().map(|leaf| GroupedRow::Workspace {
        endpoint: leaf.endpoint,
        index: leaf.index,
        depth: leaf.depth,
        label: leaf.label,
        host: leaf.host,
    }));
}

/// Project each enabled endpoint's cached snapshot exactly once. Method support is
/// deliberately irrelevant: existing snapshot tokens are already readable data.
pub(super) fn project(
    endpoints: &[ClientShellEndpoint],
    config: &SidebarGroupingConfig,
) -> Vec<GroupedRow> {
    let keys = token_keys(config);
    // A reused config key cannot represent two independent identity fields safely.
    let grouping = config.enabled
        && keys
            .iter()
            .enumerate()
            .all(|(index, key)| !key.is_empty() && !keys[..index].contains(key));
    let mut orgs = BTreeMap::<String, OrgRows<'_>>::new();
    let mut ungrouped = Vec::new();
    for (endpoint_index, endpoint) in endpoints.iter().enumerate() {
        if endpoint.status == ClientEndpointStatus::Disabled {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot.as_deref() else {
            continue;
        };
        let host = host_label(endpoint);
        let parsed = snapshot
            .workspaces
            .iter()
            .map(|workspace| {
                if grouping {
                    metadata(workspace, &keys)
                } else {
                    Metadata::Invalid
                }
            })
            .collect::<Vec<_>>();
        // Count all nonlinked roots, including unstamped/invalid roots: choosing
        // just the valid root would conceal an ambiguous local repository group.
        let mut roots = HashMap::<&str, Option<usize>>::new();
        for (index, workspace) in snapshot.workspaces.iter().enumerate() {
            if let Some(worktree) = workspace
                .worktree
                .as_ref()
                .filter(|worktree| !worktree.is_linked_worktree && !worktree.key.is_empty())
            {
                roots
                    .entry(&worktree.key)
                    .and_modify(|root| *root = None)
                    .or_insert(Some(index));
            }
        }
        for (index, workspace) in snapshot.workspaces.iter().enumerate() {
            let mut assignment = parsed[index];
            if matches!(assignment, Metadata::Missing) {
                if let Some(root) = workspace
                    .worktree
                    .as_ref()
                    .filter(|worktree| worktree.is_linked_worktree)
                    .and_then(|worktree| roots.get(worktree.key.as_str()))
                    .copied()
                    .flatten()
                {
                    if let Metadata::Project {
                        org,
                        project,
                        root: true,
                        ..
                    } = parsed[root]
                    {
                        // Inherit only identity. A root's purpose/issue/role is not
                        // the child's assignment and must not replace its label.
                        assignment = Metadata::Project {
                            org,
                            project,
                            lane: None,
                            purpose: None,
                            issue: None,
                            root: false,
                        };
                    }
                }
            }
            let (depth, label, root, lane) = match assignment {
                Metadata::Project {
                    purpose,
                    issue,
                    root,
                    lane,
                    ..
                } => {
                    let label = if root {
                        workspace.label.as_str()
                    } else {
                        purpose.unwrap_or(&workspace.label)
                    };
                    let label = match issue {
                        Some(issue) => format!("{label} {issue}"),
                        None => label.to_owned(),
                    };
                    (2, label, root, lane.unwrap_or_default())
                }
                Metadata::Org(_) => (2, workspace.label.clone(), true, ""),
                _ => (1, workspace.label.clone(), false, ""),
            };
            let leaf = Leaf {
                endpoint: endpoint_index,
                index,
                depth,
                label,
                host: host.clone(),
                root,
                lane,
                workspace,
                endpoint_id: &endpoint.endpoint_id,
            };
            match assignment {
                Metadata::Org(org) => {
                    let group = orgs.entry(org.id.to_owned()).or_default();
                    group.label.observe(org.label);
                    group.agents.push(leaf);
                }
                Metadata::Project { org, project, .. } => {
                    let group = orgs.entry(org.id.to_owned()).or_default();
                    group.label.observe(org.label);
                    let project_rows = group.projects.entry(project.id.to_owned()).or_default();
                    project_rows.label.observe(project.label);
                    project_rows.leaves.push(leaf);
                }
                _ => ungrouped.push(leaf),
            }
        }
    }
    let mut rows = Vec::new();
    for (org_id, org) in orgs {
        rows.push(GroupedRow::Heading {
            key: GroupKey::Org(org_id.clone()),
            label: org.label.resolve(&org_id),
            depth: 0,
        });
        if !org.agents.is_empty() {
            rows.push(GroupedRow::Heading {
                key: GroupKey::OrgAgents(org_id.clone()),
                label: "Org agents".to_owned(),
                depth: 1,
            });
            append_leaves(&mut rows, org.agents);
        }
        for (project_id, project) in org.projects {
            rows.push(GroupedRow::Heading {
                key: GroupKey::Project(org_id.clone(), project_id.clone()),
                label: project.label.resolve(&project_id),
                depth: 1,
            });
            append_leaves(&mut rows, project.leaves);
        }
    }
    if !ungrouped.is_empty() {
        rows.push(GroupedRow::Heading {
            key: GroupKey::Ungrouped,
            label: "Ungrouped".to_owned(),
            depth: 0,
        });
        append_leaves(&mut rows, ungrouped);
    }
    rows
}

/// Retain a collapsed heading itself, but hide all deeper descendants (including
/// nested headings) until a sibling/ancestor row ends that subtree.
pub(super) fn visible_rows(rows: &[GroupedRow], collapsed: &HashSet<GroupKey>) -> Vec<GroupedRow> {
    let mut hidden_below = None;
    let mut visible = Vec::with_capacity(rows.len());
    for row in rows {
        let depth = match row {
            GroupedRow::Heading { depth, .. } | GroupedRow::Workspace { depth, .. } => *depth,
        };
        if hidden_below.is_some_and(|heading_depth| depth > heading_depth) {
            continue;
        }
        hidden_below = None;
        if let GroupedRow::Heading { key, .. } = row {
            if collapsed.contains(key) {
                hidden_below = Some(depth);
            }
        }
        visible.push(row.clone());
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;
    use crate::protocol::ClientShellWorktree;

    fn config() -> SidebarGroupingConfig {
        SidebarGroupingConfig {
            enabled: true,
            ..SidebarGroupingConfig::default()
        }
    }

    fn workspace(id: &str, label: &str, tokens: &[(&str, &str)]) -> ClientShellWorkspace {
        let mut workspace = super::super::tests::snapshot().workspaces.remove(0);
        workspace.workspace_id = id.to_owned();
        workspace.label = label.to_owned();
        workspace.tokens = tokens
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        workspace
    }

    fn stamped(id: &str, label: &str) -> ClientShellWorkspace {
        workspace(
            id,
            label,
            &[
                ("smarty_org_id", "smarty-pants"),
                ("smarty_org_label", "Smarty Pants"),
                ("smarty_project_id", "herdr"),
                ("smarty_project_label", "Herdr"),
            ],
        )
    }

    fn set(workspace: &mut ClientShellWorkspace, key: &str, value: &str) {
        workspace.tokens.retain(|(existing, _)| existing != key);
        workspace.tokens.push((key.to_owned(), value.to_owned()));
    }

    fn membership(workspace: &mut ClientShellWorkspace, key: &str, linked: bool) {
        workspace.worktree = Some(ClientShellWorktree {
            key: key.to_owned(),
            label: "repository".to_owned(),
            is_linked_worktree: linked,
        });
    }

    fn endpoint(
        number: u8,
        target: Option<&str>,
        workspaces: Vec<ClientShellWorkspace>,
    ) -> ClientShellEndpoint {
        let mut endpoint = super::super::local_endpoint();
        if let Some(target) = target {
            endpoint.endpoint_id =
                ClientEndpointId::Ssh(ProfileId::parse(format!("{number:032x}")).unwrap());
            endpoint.connection_target = Some(target.to_owned());
        }
        let mut snapshot = super::super::tests::snapshot();
        snapshot.workspaces = workspaces;
        endpoint.snapshot = Some(Box::new(snapshot));
        endpoint
    }

    fn heading<'a>(rows: &'a [GroupedRow], key: &GroupKey) -> Option<&'a str> {
        rows.iter().find_map(|row| match row {
            GroupedRow::Heading {
                key: candidate,
                label,
                ..
            } if candidate == key => Some(label.as_str()),
            _ => None,
        })
    }

    fn leaves(rows: &[GroupedRow]) -> Vec<(usize, usize, &str, &str)> {
        rows.iter()
            .filter_map(|row| match row {
                GroupedRow::Workspace {
                    endpoint,
                    index,
                    label,
                    host,
                    ..
                } => Some((*endpoint, *index, label.as_str(), host.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn missing_partial_oversized_and_conflicting_metadata_stays_ungrouped() {
        let mut invalid = vec![workspace("missing", "missing", &[])];
        for key in [
            "smarty_org_id",
            "smarty_org_label",
            "smarty_project_id",
            "smarty_project_label",
        ] {
            let mut item = stamped(key, key);
            item.tokens.retain(|(candidate, _)| candidate != key);
            invalid.push(item);
        }
        for key in [
            "smarty_org_id",
            "smarty_project_id",
            "smarty_lane_id",
            "smarty_org_label",
            "smarty_lane_purpose",
            "smarty_issue",
            "smarty_host",
            "smarty_role",
            "smarty_lead",
        ] {
            let mut item = stamped(key, key);
            set(&mut item, key, &"a".repeat(81));
            invalid.push(item);
        }
        for key in [
            "smarty_org_id",
            "smarty_project_id",
            "smarty_org_label",
            "smarty_project_label",
        ] {
            let mut item = stamped(key, key);
            item.tokens.push((key.to_owned(), "different".to_owned()));
            invalid.push(item);
        }
        for (key, value) in [
            ("smarty_org_label", "\u{1b}[31mUnsafe"),
            ("smarty_project_label", "\nUnsafe"),
            ("smarty_org_id", "Org"),
            ("smarty_project_id", "project.name"),
            ("smarty_lane_id", "_lane"),
            ("smarty_org_label", "   "),
            ("smarty_role", "unknown"),
            ("smarty_role", "org-agent"),
            ("smarty_lane_purpose", "purpose without lane"),
        ] {
            let mut item = stamped(key, key);
            set(&mut item, key, value);
            invalid.push(item);
        }
        let expected = invalid.len();
        let rows = project(&[endpoint(1, Some("ryzen1"), invalid)], &config());
        assert_eq!(heading(&rows, &GroupKey::Ungrouped), Some("Ungrouped"));
        assert_eq!(rows.len(), expected + 1);
        assert_eq!(leaves(&rows).len(), expected);
        assert!(rows
            .iter()
            .skip(1)
            .all(|row| matches!(row, GroupedRow::Workspace { depth: 1, .. })));
    }

    #[test]
    fn identity_validation_is_exact_ascii_bounded_and_does_not_normalize() {
        for valid in ["a", "0", "org-1_lane", &"a".repeat(80)] {
            assert!(valid_slug(valid), "{valid:?}");
        }
        for invalid in [
            "",
            "A",
            "aA",
            "a!",
            "a/other",
            "_a",
            "-a",
            " a",
            "a ",
            "é",
            "a\n",
            &"a".repeat(81),
        ] {
            assert!(!valid_slug(invalid), "{invalid:?}");
        }
        let mut item = stamped("unicode-label", "root");
        set(&mut item, "smarty_org_label", &"é".repeat(80));
        let rows = project(&[endpoint(1, None, vec![item.clone()])], &config());
        assert_eq!(
            heading(&rows, &GroupKey::Org("smarty-pants".into())),
            Some("é".repeat(80).as_str())
        );
        set(&mut item, "smarty_org_label", &"é".repeat(81));
        let rows = project(&[endpoint(1, None, vec![item])], &config());
        assert_eq!(heading(&rows, &GroupKey::Ungrouped), Some("Ungrouped"));
    }

    #[test]
    fn rootless_lane_groups_from_tokens_and_shows_purpose_and_issue_without_method_gating() {
        let mut lane = stamped("ws_1", "assignment");
        set(&mut lane, "smarty_lane_id", "herdr-4283");
        set(&mut lane, "smarty_lane_purpose", "Unified sidebar");
        set(
            &mut lane,
            "smarty_issue",
            "Smarty-Pants-Inc/smarty-dev#4283",
        );
        set(&mut lane, "smarty_role", "worktree-agent");
        set(&mut lane, "smarty_host", "ryzen1");
        membership(&mut lane, "/remote/repo", true);
        let mut remote = endpoint(1, Some("paul@ryzen3.smartypants.ai"), vec![lane]);
        remote.methods = Some(HashSet::new());
        let rows = project(&[remote], &config());
        assert_eq!(
            heading(
                &rows,
                &GroupKey::Project("smarty-pants".into(), "herdr".into())
            ),
            Some("Herdr")
        );
        assert_eq!(
            leaves(&rows),
            vec![(
                0,
                0,
                "Unified sidebar Smarty-Pants-Inc/smarty-dev#4283",
                "Ryzen 3"
            )]
        );
        assert!(heading(&rows, &GroupKey::Ungrouped).is_none());
    }

    #[test]
    fn duplicate_workspace_ids_and_labels_on_hosts_remain_distinct_leaves() {
        let root = stamped("ws_1", "lead");
        let endpoints = [
            endpoint(1, Some("ryzen1"), vec![root.clone()]),
            endpoint(2, Some("ryzen4"), vec![root]),
        ];
        let rows = project(&endpoints, &config());
        assert_eq!(rows.len(), 4);
        assert_eq!(
            leaves(&rows),
            vec![(0, 0, "lead", "Ryzen 1"), (1, 0, "lead", "Ryzen 4")]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(
                    row,
                    GroupedRow::Heading {
                        key: GroupKey::Project(_, _),
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn conflicting_readable_labels_fall_back_to_ids_independent_of_endpoint_order() {
        let first = stamped("ws_1", "lead");
        let mut second = first.clone();
        set(&mut second, "smarty_org_label", "Other org label");
        set(&mut second, "smarty_project_label", "Other project label");
        let mut endpoints = vec![
            endpoint(1, Some("ryzen1"), vec![first]),
            endpoint(2, Some("ryzen2"), vec![second]),
        ];
        for _ in 0..2 {
            let rows = project(&endpoints, &config());
            assert_eq!(
                heading(&rows, &GroupKey::Org("smarty-pants".into())),
                Some("smarty-pants")
            );
            assert_eq!(
                heading(
                    &rows,
                    &GroupKey::Project("smarty-pants".into(), "herdr".into())
                ),
                Some("herdr")
            );
            assert_eq!(leaves(&rows).len(), 2);
            endpoints.reverse();
        }
    }

    #[test]
    fn equal_readable_labels_never_merge_different_org_or_project_ids() {
        let first = stamped("ws_1", "same");
        let mut second = first.clone();
        set(&mut second, "smarty_project_id", "other-project");
        let mut third = first.clone();
        set(&mut third, "smarty_org_id", "other-org");
        let rows = project(
            &[endpoint(1, Some("ryzen1"), vec![first, second, third])],
            &config(),
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(
                    row,
                    GroupedRow::Heading {
                        key: GroupKey::Org(_),
                        ..
                    }
                ))
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(
                    row,
                    GroupedRow::Heading {
                        key: GroupKey::Project(_, _),
                        ..
                    }
                ))
                .count(),
            3
        );
        assert_eq!(leaves(&rows).len(), 3);
    }

    #[test]
    fn org_sessions_on_one_host_share_org_agents_heading_not_leaf_identity() {
        let org = workspace(
            "ws_1",
            "org",
            &[
                ("smarty_org_id", "smarty-pants"),
                ("smarty_org_label", "Smarty Pants"),
                ("smarty_role", "org-agent"),
            ],
        );
        let mut paul = endpoint(1, Some("paul@ryzen1"), vec![org.clone()]);
        paul.label = "Paul org session".into();
        let mut kate = endpoint(2, Some("paul@ryzen1"), vec![org, stamped("lead", "lead")]);
        kate.label = "Kate org session".into();
        let rows = project(&[paul, kate], &config());
        assert_eq!(
            heading(&rows, &GroupKey::OrgAgents("smarty-pants".into())),
            Some("Org agents")
        );
        assert_eq!(
            leaves(&rows),
            vec![
                (0, 0, "org", "Ryzen 1"),
                (1, 0, "org", "Ryzen 1"),
                (1, 1, "lead", "Ryzen 1")
            ]
        );
        assert!(matches!(
            &rows[1],
            GroupedRow::Heading {
                key: GroupKey::OrgAgents(_),
                depth: 1,
                ..
            }
        ));
        assert!(heading(&rows, &GroupKey::Ungrouped).is_none());
    }

    #[test]
    fn roots_precede_lanes_and_lanes_sort_by_display_label_with_stable_ties() {
        let mut lane = stamped("lane", "A lane");
        set(&mut lane, "smarty_lane_id", "lane-a");
        set(&mut lane, "smarty_lane_purpose", "A purpose");
        let mut other_lane = lane.clone();
        other_lane.workspace_id = "other-lane".into();
        set(&mut other_lane, "smarty_lane_id", "lane-b");
        let mut lead = stamped("lead", "Z lead");
        set(&mut lead, "smarty_role", "project-agent");
        let root = stamped("root", "Y root");
        let rows = project(
            &[endpoint(
                1,
                Some("ryzen1"),
                vec![other_lane, lead, lane, root],
            )],
            &config(),
        );
        assert_eq!(
            leaves(&rows),
            vec![
                (0, 3, "Y root", "Ryzen 1"),
                (0, 1, "Z lead", "Ryzen 1"),
                (0, 2, "A purpose", "Ryzen 1"),
                (0, 0, "A purpose", "Ryzen 1")
            ]
        );
    }

    #[test]
    fn unstamped_linked_worktree_inherits_only_unique_same_endpoint_root_identity() {
        let mut root = stamped("root", "lead");
        set(&mut root, "smarty_issue", "root#1");
        membership(&mut root, "repo", false);
        let mut child = workspace("child", "child label", &[("unrelated", "token")]);
        membership(&mut child, "repo", true);
        let endpoints = [
            endpoint(1, Some("ryzen1"), vec![child.clone(), root]),
            endpoint(2, Some("ryzen2"), vec![child]),
        ];
        let rows = project(&endpoints, &config());
        assert_eq!(
            leaves(&rows),
            vec![
                (0, 1, "lead root#1", "Ryzen 1"),
                (0, 0, "child label", "Ryzen 1"),
                (1, 0, "child label", "Ryzen 2")
            ]
        );
        assert_eq!(heading(&rows, &GroupKey::Ungrouped), Some("Ungrouped"));
        assert!(matches!(
            rows.last(),
            Some(GroupedRow::Workspace {
                endpoint: 1,
                depth: 1,
                ..
            })
        ));
    }

    #[test]
    fn ambiguous_invalid_and_unstamped_roots_do_not_authorize_inheritance() {
        let mut root = stamped("root", "root");
        membership(&mut root, "repo", false);
        let mut child = workspace("child", "child", &[]);
        membership(&mut child, "repo", true);
        let mut invalid = root.clone();
        invalid
            .tokens
            .push(("smarty_org_id".into(), "conflict".into()));
        let mut missing = root.clone();
        missing.tokens.clear();
        let mut conflicting_root = root.clone();
        set(&mut conflicting_root, "smarty_project_id", "other-project");
        for roots in [
            vec![root.clone(), root.clone()],
            vec![root.clone(), conflicting_root],
            vec![invalid.clone()],
            vec![missing.clone()],
            vec![root.clone(), invalid],
            vec![root, missing],
        ] {
            let mut workspaces = roots;
            let child_index = workspaces.len();
            workspaces.push(child.clone());
            let rows = project(&[endpoint(1, Some("ryzen1"), workspaces)], &config());
            assert!(rows.iter().any(|row| matches!(row, GroupedRow::Workspace { index, depth: 1, .. } if *index == child_index)));
        }
    }

    #[test]
    fn partial_or_conflicting_child_tokens_never_get_repaired_from_root() {
        let mut root = stamped("root", "root");
        membership(&mut root, "repo", false);
        for tokens in [
            vec![("smarty_project_id", "other")],
            vec![("smarty_host", "ryzen2")],
            vec![
                ("smarty_org_id", "smarty-pants"),
                ("smarty_org_id", "other"),
            ],
        ] {
            let mut child = workspace("child", "child", &tokens);
            membership(&mut child, "repo", true);
            let rows = project(
                &[endpoint(1, Some("ryzen1"), vec![root.clone(), child])],
                &config(),
            );
            assert!(matches!(
                rows.last(),
                Some(GroupedRow::Workspace {
                    index: 1,
                    depth: 1,
                    ..
                })
            ));
        }
    }

    #[test]
    fn explicitly_stamped_lane_owns_its_project_even_if_local_git_root_differs() {
        let mut root = stamped("root", "infrastructure lead");
        membership(&mut root, "repo", false);
        let mut lane = stamped("lane", "product lane");
        set(&mut lane, "smarty_project_id", "product");
        set(&mut lane, "smarty_project_label", "Product");
        set(&mut lane, "smarty_lane_id", "product-1");
        membership(&mut lane, "repo", true);
        let rows = project(&[endpoint(1, Some("ryzen1"), vec![root, lane])], &config());
        assert_eq!(
            heading(
                &rows,
                &GroupKey::Project("smarty-pants".into(), "product".into())
            ),
            Some("Product")
        );
        assert!(heading(&rows, &GroupKey::Ungrouped).is_none());
    }

    #[test]
    fn host_is_actual_connection_target_not_profile_label_or_metadata_hint() {
        for (target, expected) in [
            ("ryzen1", "Ryzen 1"),
            ("user@ryzen2", "Ryzen 2"),
            ("ryzen3.smartypants.ai", "Ryzen 3"),
            ("user@ryzen4.smartypants.ai", "Ryzen 4"),
            ("m5", "m5"),
            ("user@m5.smartypants.ai", "m5"),
            ("ssh://ryzen3", "Ryzen 3"),
            ("ssh://ryzen3:2222", "Ryzen 3"),
            ("ssh://user@ryzen3.smartypants.ai", "Ryzen 3"),
            ("ssh://user@ryzen3.smartypants.ai:22", "Ryzen 3"),
            ("ssh://user@m5.smartypants.ai:2222", "m5"),
            ("ssh://user@other.example:22", "ssh://user@other.example:22"),
            ("ssh://ryzen3.example:22", "ssh://ryzen3.example:22"),
            ("ssh://ryzen3:bad", "ssh://ryzen3:bad"),
            ("ssh://ryzen3:65536", "ssh://ryzen3:65536"),
            ("ssh://ryzen3:22/path", "ssh://ryzen3:22/path"),
            ("ssh://user@[::1]:2222", "ssh://user@[::1]:2222"),
            ("user@other.example", "other.example"),
            ("ryzen1.example", "ryzen1.example"),
            ("ryzen10.smartypants.ai", "ryzen10.smartypants.ai"),
        ] {
            let mut workspace = stamped("ws_1", "root");
            set(&mut workspace, "smarty_host", "ryzen4");
            let mut endpoint = endpoint(1, Some(target), vec![workspace]);
            endpoint.label = "Misleading profile label".into();
            let rows = project(&[endpoint], &config());
            assert_eq!(leaves(&rows)[0].3, expected);
        }
        let mut local = endpoint(1, None, vec![stamped("local", "local")]);
        local.connection_target = Some("ryzen1".into());
        assert_eq!(leaves(&project(&[local], &config()))[0].3, "Local");
    }

    #[test]
    fn config_keys_are_generic_disabled_grouping_does_not_hide_workspaces() {
        let mut config = config();
        let mut workspace = stamped("root", "root");
        for (slot, key) in token_keys(&config).iter().enumerate() {
            for (candidate, _) in &mut workspace.tokens {
                if candidate == key {
                    *candidate = format!("custom_{slot}");
                }
            }
        }
        config.org_id = "custom_0".into();
        config.org_label = "custom_1".into();
        config.project_id = "custom_2".into();
        config.project_label = "custom_3".into();
        let endpoints = [endpoint(1, None, vec![workspace])];
        assert_eq!(
            heading(
                &project(&endpoints, &config),
                &GroupKey::Org("smarty-pants".into())
            ),
            Some("Smarty Pants")
        );
        config.enabled = false;
        assert_eq!(
            heading(&project(&endpoints, &config), &GroupKey::Ungrouped),
            Some("Ungrouped")
        );
        config.enabled = true;
        config.org_label = config.org_id.clone();
        assert_eq!(
            heading(&project(&endpoints, &config), &GroupKey::Ungrouped),
            Some("Ungrouped")
        );
    }

    #[test]
    fn disabled_removed_or_empty_endpoints_leave_no_rows_and_stale_rows_keep_indices() {
        let mut disabled = endpoint(1, Some("ryzen1"), vec![stamped("disabled", "disabled")]);
        disabled.status = ClientEndpointStatus::Disabled;
        let mut reconnecting = endpoint(2, Some("ryzen2"), vec![stamped("stale", "stale")]);
        reconnecting.status = ClientEndpointStatus::Reconnecting;
        reconnecting.snapshot_generation = Some(7);
        let mut empty = endpoint(3, Some("ryzen3"), vec![]);
        empty.snapshot = None;
        let mut endpoints = vec![disabled, reconnecting, empty];
        assert_eq!(
            leaves(&project(&endpoints, &config())),
            vec![(1, 0, "stale", "Ryzen 2")]
        );
        endpoints.remove(1);
        assert!(project(&endpoints, &config()).is_empty());
    }

    #[test]
    fn identical_duplicate_tokens_and_cleared_tokens_are_not_false_conflicts() {
        let mut root = stamped("root", "root");
        root.tokens
            .push(("smarty_org_id".into(), "smarty-pants".into()));
        membership(&mut root, "repo", false);
        let mut child = workspace(
            "child",
            "child",
            &[("smarty_org_id", ""), ("smarty_role", "")],
        );
        membership(&mut child, "repo", true);
        let rows = project(&[endpoint(1, None, vec![root, child])], &config());
        assert_eq!(leaves(&rows).len(), 2);
        assert!(heading(&rows, &GroupKey::Ungrouped).is_none());
    }

    #[test]
    fn collapse_hides_descendants_keeps_heading_and_resumes_at_siblings() {
        let org = workspace(
            "org",
            "org",
            &[
                ("smarty_org_id", "smarty-pants"),
                ("smarty_org_label", "Smarty Pants"),
                ("smarty_role", "org-agent"),
            ],
        );
        let rows = project(
            &[endpoint(
                1,
                None,
                vec![
                    org,
                    stamped("root", "root"),
                    workspace("missing", "missing", &[]),
                ],
            )],
            &config(),
        );
        assert_eq!(visible_rows(&rows, &HashSet::new()), rows);
        let collapsed = HashSet::from([GroupKey::OrgAgents("smarty-pants".into())]);
        let visible = visible_rows(&rows, &collapsed);
        assert_eq!(visible.len(), rows.len() - 1);
        assert_eq!(
            leaves(&visible),
            vec![(0, 1, "root", "Local"), (0, 2, "missing", "Local")]
        );
        assert!(heading(&visible, &GroupKey::OrgAgents("smarty-pants".into())).is_some());
        let collapsed = HashSet::from([
            GroupKey::Org("smarty-pants".into()),
            GroupKey::OrgAgents("smarty-pants".into()),
            GroupKey::Ungrouped,
        ]);
        let visible = visible_rows(&rows, &collapsed);
        assert_eq!(visible.len(), 2);
        assert!(leaves(&visible).is_empty());
        assert!(heading(&visible, &GroupKey::Org("smarty-pants".into())).is_some());
        assert!(heading(&visible, &GroupKey::Ungrouped).is_some());
    }

    #[test]
    fn heading_keys_and_leaf_order_survive_snapshot_reordering() {
        let first = stamped("a", "same");
        let second = stamped("b", "same");
        let mut endpoints = vec![
            endpoint(1, Some("ryzen1"), vec![second, first]),
            endpoint(2, Some("ryzen2"), vec![stamped("a", "same")]),
        ];
        let identities = |endpoints: &[ClientShellEndpoint]| {
            project(endpoints, &config())
                .into_iter()
                .map(|row| match row {
                    GroupedRow::Heading { key, .. } => format!("{key:?}"),
                    GroupedRow::Workspace {
                        endpoint, index, ..
                    } => format!(
                        "{:?}/{}",
                        endpoints[endpoint].endpoint_id,
                        endpoints[endpoint].snapshot.as_ref().unwrap().workspaces[index]
                            .workspace_id
                    ),
                })
                .collect::<Vec<_>>()
        };
        let before = identities(&endpoints);
        endpoints.reverse();
        for endpoint in &mut endpoints {
            endpoint.snapshot.as_mut().unwrap().workspaces.reverse();
        }
        assert_eq!(identities(&endpoints), before);
    }
}

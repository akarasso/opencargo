//! The gate of the addressed repository, loaded once per request and
//! pushed down to every member, with each member's floor: the rules of
//! every `Hide` repository from which that member is reachable, on any
//! path. A floor only narrows, so a group in front of a closed mirror can
//! never widen it, whatever the order of its members.

use std::collections::{HashMap, HashSet};

use crate::config::McpConfig;
use crate::domain::governance::{self, AllowRule, Candidate, Decision, Floor, GateMode, Visibility};
use crate::domain::{Format, RepoKind, Repository, MAX_GROUP_DEPTH};
use crate::error::AppResult;
use crate::ports::mcp::{CatalogRow, McpStore, NameFilter};
use crate::ports::repositories::RepositoryStore;

pub fn mode_of(settings: &HashMap<String, McpConfig>, repo: &str) -> GateMode {
    settings.get(repo).map(|c| c.mode).unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct Gate {
    pub repo_id: i64,
    pub repo: String,
    pub mode: GateMode,
    pub rules: Vec<AllowRule>,
}

#[derive(Debug, Clone)]
pub struct Gates {
    pub addressed: Gate,
    pub floors: HashMap<i64, Floor>,
}

async fn gate_of(mcp: &dyn McpStore, settings: &HashMap<String, McpConfig>, repo: &Repository) -> AppResult<Gate> {
    let rules = mcp.allow_rules(repo.id).await?.into_iter().map(|r| r.rule).collect();
    Ok(Gate {
        repo_id: repo.id,
        repo: repo.name.clone(),
        mode: mode_of(settings, &repo.name),
        rules,
    })
}

impl Gates {
    /// One load per request: the addressed gate, and a floor for every
    /// leaf of its closure.
    pub async fn load(
        repos: &dyn RepositoryStore,
        mcp: &dyn McpStore,
        settings: &HashMap<String, McpConfig>,
        addressed: &Repository,
    ) -> AppResult<Gates> {
        let mut nodes: HashMap<i64, (Repository, Vec<i64>)> = HashMap::new();
        let mut frontier = vec![addressed.clone()];
        for _ in 0..=MAX_GROUP_DEPTH {
            let mut next = Vec::new();
            for repo in frontier {
                if nodes.contains_key(&repo.id) {
                    continue;
                }
                let mut children = Vec::new();
                if repo.kind()? == RepoKind::Group {
                    for name in repo.members() {
                        if let Some(member) = repos.by_name(&name).await? {
                            if member.fmt()? == Format::Mcp {
                                children.push(member.id);
                                next.push(member);
                            }
                        }
                    }
                }
                nodes.insert(repo.id, (repo, children));
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let mut floors: HashMap<i64, Floor> = HashMap::new();
        for (id, (repo, _)) in &nodes {
            let mode = mode_of(settings, &repo.name);
            if *id == addressed.id || mode != GateMode::Hide {
                continue;
            }
            let gate = gate_of(mcp, settings, repo).await?;
            if gate.rules.is_empty() {
                continue;
            }
            for leaf in leaves(&nodes, *id) {
                floors.entry(leaf).or_default().fragments.push((gate.repo.clone(), gate.rules.clone()));
            }
        }
        for floor in floors.values_mut() {
            floor.fragments.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Ok(Gates {
            addressed: gate_of(mcp, settings, addressed).await?,
            floors,
        })
    }

    /// Every ruleset in force as a removal for rows of `member`.
    pub fn filters(&self, member: i64) -> Vec<NameFilter> {
        let mut out = Vec::new();
        if self.addressed.mode == GateMode::Hide && !self.addressed.rules.is_empty() {
            out.push(NameFilter {
                rules: self.addressed.rules.clone(),
            });
        }
        if let Some(floor) = self.floors.get(&member) {
            out.extend(floor.fragments.iter().map(|(_, rules)| NameFilter { rules: rules.clone() }));
        }
        out
    }

    pub fn require_approved(&self) -> bool {
        self.addressed.mode == GateMode::Hide
    }

    /// The one decision about a row; the SQL filters only narrow what is
    /// read.
    pub fn decide(&self, member: i64, row: &CatalogRow, include_deleted: bool) -> Visibility {
        let verdict = governance::EndpointVerdict {
            surface_endpoints: row.surface_endpoints,
            approved_endpoints: row.approved_endpoints,
            worst_drift: row.worst_drift,
            drifted_remote: row.drifted_remote.clone(),
        };
        let candidate = Candidate {
            name: &row.name,
            deleted: row.status == "deleted",
            verdict: &verdict,
            blocked: row.decision == Some(Decision::Blocked),
        };
        let addressed = governance::decide(self.addressed.mode, &self.addressed.rules, &candidate, include_deleted);
        match self.floors.get(&member) {
            Some(floor) => addressed.worst(floor.visibility(&row.name)),
            None => addressed,
        }
    }

    /// A skill: the allow floor over its name and, under `Hide`, one
    /// approval of its current surface.
    pub fn decide_skill(&self, member: i64, name: &str, decision: Option<Decision>) -> Visibility {
        let addressed = governance::decide_single(self.addressed.mode, &self.addressed.rules, name, decision);
        match self.floors.get(&member) {
            Some(floor) => addressed.worst(floor.visibility(name)),
            None => addressed,
        }
    }
}

/// The hosted and proxy repositories reachable from `from`, itself
/// included when it is one.
fn leaves(nodes: &HashMap<i64, (Repository, Vec<i64>)>, from: i64) -> HashSet<i64> {
    let mut out = HashSet::new();
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some((repo, children)) = nodes.get(&id) else {
            continue;
        };
        if repo.kind().ok() == Some(RepoKind::Group) {
            stack.extend(children);
        } else {
            out.insert(id);
        }
    }
    out
}

//! The permissions proposal: source rights on source repositories, mapped
//! onto target users and repositories. Only exact usernames map; anything
//! else is a gap, never a guess, and nothing is written without `apply`.

use std::collections::BTreeMap;

use serde::Serialize;

use super::plan::PlanRules;
use crate::domain::import::GapKind;
use crate::ports::import::{Gap, Principal, Source, TargetAdmin};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    pub principal: String,
    pub user: String,
    pub repo: String,
    pub read: bool,
    /// A source publish right is read and write here: every publish is
    /// preceded by a read.
    pub write: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Proposal {
    pub grants: Vec<Grant>,
    pub gaps: Vec<Gap>,
    /// Users `apply` created, each needing a password or an SSO identity.
    pub created: Vec<String>,
}

pub struct ProposePermissions<'a> {
    pub source: &'a dyn Source,
    pub admin: &'a dyn TargetAdmin,
    pub rules: &'a PlanRules,
}

pub fn map(principals: &[Principal], rules: &PlanRules) -> Proposal {
    let mut merged: BTreeMap<(String, String), (bool, bool)> = BTreeMap::new();
    let mut gaps = Vec::new();
    for p in principals {
        let subject = format!("{} {}", p.kind, p.name);
        if p.kind != "user" {
            gaps.push(Gap::new(
                GapKind::PermissionNotMapped,
                &subject,
                format!("{} on {}: only users map to opencargo accounts", rights(p), p.repo),
            ));
            continue;
        }
        let Some(target) = rules.target_repo(&p.repo) else {
            gaps.push(Gap::new(
                GapKind::PermissionNotMapped,
                &subject,
                format!("{} on {}: no target repository is mapped", rights(p), p.repo),
            ));
            continue;
        };
        let e = merged.entry((p.name.clone(), target.to_string())).or_default();
        e.0 |= p.read || p.publish;
        e.1 |= p.publish;
    }
    let grants = merged
        .into_iter()
        .map(|((user, repo), (read, write))| Grant { principal: user.clone(), user, repo, read, write })
        .collect();
    Proposal { grants, gaps, created: Vec::new() }
}

fn rights(p: &Principal) -> &'static str {
    match (p.read, p.publish) {
        (_, true) => "publish",
        (true, false) => "read",
        _ => "no right",
    }
}

impl ProposePermissions<'_> {
    pub async fn run(&self, apply: bool) -> Result<Proposal, String> {
        let principals = self.source.principals().await.map_err(|e| format!("reading the source's permissions: {e}"))?;
        let mut proposal = map(&principals, self.rules);
        if !apply {
            return Ok(proposal);
        }
        for g in &proposal.grants {
            if !self.admin.user_exists(&g.user).await.map_err(|e| e.to_string())? {
                self.admin.create_user(&g.user).await.map_err(|e| e.to_string())?;
                if !proposal.created.contains(&g.user) {
                    proposal.created.push(g.user.clone());
                }
            }
            self.admin.grant(&g.user, &g.repo, g.read, g.write).await.map_err(|e| e.to_string())?;
        }
        Ok(proposal)
    }
}

pub fn render(p: &Proposal, applied: bool) -> String {
    let mut out = String::new();
    out.push_str(if applied { "applied:\n" } else { "proposed (run again with --apply to write it):\n" });
    for g in &p.grants {
        let rights = match (g.read, g.write) {
            (true, true) => "read+write",
            (true, false) => "read",
            _ => "none",
        };
        out.push_str(&format!("  {} -> user {} -> {} -> {rights}\n", g.principal, g.user, g.repo));
    }
    for u in &p.created {
        out.push_str(&format!(
            "created user {u}: set a password with PUT /api/v1/users/{u}/password or map an SSO identity\n"
        ));
    }
    for g in &p.gaps {
        out.push_str(&format!("  {}: {} ({})\n", g.kind, g.source_ref, g.detail));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, kind: &str, repo: &str, read: bool, publish: bool) -> Principal {
        Principal { name: name.into(), kind: kind.into(), repo: repo.into(), read, publish }
    }

    #[test]
    fn publish_maps_to_read_and_write_and_the_rest_are_gaps() {
        let rules = PlanRules { maps: vec![("npm-*".into(), "npm".into())], ..Default::default() };
        let got = map(
            &[
                p("alice", "user", "npm-internal", false, true),
                p("alice", "user", "npm-other", true, false),
                p("bob", "user", "npm-internal", true, false),
                p("devs", "group", "npm-internal", true, true),
                p("carol", "user", "raw-files", true, false),
            ],
            &rules,
        );
        assert_eq!(
            got.grants,
            vec![
                Grant { principal: "alice".into(), user: "alice".into(), repo: "npm".into(), read: true, write: true },
                Grant { principal: "bob".into(), user: "bob".into(), repo: "npm".into(), read: true, write: false },
            ]
        );
        assert_eq!(got.gaps.len(), 2);
        assert!(got.gaps.iter().all(|g| g.kind == GapKind::PermissionNotMapped));
    }
}

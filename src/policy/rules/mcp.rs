//! The four MCP rules: pure over the facts the read path gathered, each
//! not applicable outside the `mcp` format.

use chrono::{DateTime, Utc};

use super::{PolicyConfig, Rule};
use crate::domain::{Drift, Format, RuleVerdict, Verdict};
use crate::policy::facts::mcp::{Approval, McpFacts};
use crate::policy::Resolution;

fn facts<'a>(rule: &'static str, r: &'a Resolution) -> Result<&'a McpFacts, RuleVerdict> {
    if r.format != Format::Mcp {
        return Err(RuleVerdict::new(
            rule,
            Verdict::NotApplicable,
            format!("{}: an MCP server rule", r.format.as_str()),
        ));
    }
    r.facts
        .mcp
        .as_deref()
        .ok_or_else(|| RuleVerdict::new(rule, Verdict::Unknown, "no MCP facts gathered"))
}

pub struct McpAllowlist;
pub struct McpInjection;
pub struct McpTransport;
pub struct McpDrift;

impl Rule for McpAllowlist {
    fn name(&self) -> &'static str {
        "mcp_allowlist"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.mcp_allowlist
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let f = match facts(self.name(), r) {
            Ok(f) => f,
            Err(v) => return Some(v),
        };
        Some(if f.allowed {
            RuleVerdict::new(self.name(), Verdict::Pass, "allowed")
        } else {
            RuleVerdict::new(
                self.name(),
                Verdict::WouldBlock,
                format!("{} matches no allow rule of {}", r.name, f.addressed),
            )
        })
    }
}

impl Rule for McpInjection {
    fn name(&self) -> &'static str {
        "mcp_injection"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.mcp_injection
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let f = match facts(self.name(), r) {
            Ok(f) => f,
            Err(v) => return Some(v),
        };
        let gating: Vec<String> = f
            .findings
            .iter()
            .filter(|x| x.high || f.scan_medium)
            .take(3)
            .map(|x| {
                let tool = if x.tool.is_empty() { String::new() } else { format!("/{}", x.tool) };
                format!("{} in {}{tool}", x.pattern, x.field)
            })
            .collect();
        Some(if !gating.is_empty() {
            RuleVerdict::new(self.name(), Verdict::WouldBlock, gating.join(", "))
        } else if !f.tools_observed {
            RuleVerdict::new(self.name(), Verdict::Unknown, "tool descriptions not observed (declared only)")
        } else {
            RuleVerdict::new(self.name(), Verdict::Pass, "no finding")
        })
    }
}

impl Rule for McpTransport {
    fn name(&self) -> &'static str {
        "mcp_transport"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.mcp_transport
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let f = match facts(self.name(), r) {
            Ok(f) => f,
            Err(v) => return Some(v),
        };
        let mut why = Vec::new();
        if f.package_transports.iter().any(|t| t == "stdio") && f.approval != Approval::Approved {
            why.push("stdio package not approved");
        }
        if !f.remote_transports.is_empty() && f.remote_transports.iter().all(|t| t == "sse") {
            why.push("sse transport is deprecated");
        }
        Some(if why.is_empty() {
            RuleVerdict::new(self.name(), Verdict::Pass, "transports acceptable")
        } else {
            RuleVerdict::new(self.name(), Verdict::WouldBlock, why.join("; "))
        })
    }
}

impl Rule for McpDrift {
    fn name(&self) -> &'static str {
        "mcp_drift"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.mcp_drift
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let f = match facts(self.name(), r) {
            Ok(f) => f,
            Err(v) => return Some(v),
        };
        let at = f
            .drifted_remote
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(|u| format!(" at {u}"))
            .unwrap_or_default();
        Some(match f.drift {
            _ if !f.reviewed => RuleVerdict::new(self.name(), Verdict::Unknown, "never approved"),
            Drift::None => RuleVerdict::new(self.name(), Verdict::Pass, "unchanged since approval"),
            Drift::NewEndpoint => RuleVerdict::new(
                self.name(),
                Verdict::WouldBlock,
                format!("a new endpoint{at} appeared since approval"),
            ),
            Drift::Tools => RuleVerdict::new(self.name(), Verdict::WouldBlock, format!("tool descriptions changed{at} since approval")),
            Drift::Permissions => RuleVerdict::new(self.name(), Verdict::WouldBlock, format!("declared permissions changed{at} since approval")),
            Drift::Both => RuleVerdict::new(
                self.name(),
                Verdict::WouldBlock,
                format!("permissions and tool descriptions changed{at} since approval"),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::policy::facts::mcp::McpFinding;
    use crate::policy::{Actor, Facts};

    fn resolution(format: Format, f: McpFacts) -> Resolution {
        Resolution {
            requested_repo: "team".into(),
            member_repo: "mirror".into(),
            format,
            name: "io.github.acme/x".into(),
            version: Some("1.0.0".into()),
            digest: None,
            actor: Actor::of(None),
            published_at: None,
            facts: Facts {
                mcp: Some(Arc::new(f)),
                ..Facts::default()
            },
        }
    }

    fn base() -> McpFacts {
        McpFacts {
            addressed: "team".into(),
            package_transports: vec![],
            remote_transports: vec!["streamable-http".into()],
            allowed: true,
            approval: Approval::Approved,
            reviewed: true,
            drift: Drift::None,
            drifted_remote: None,
            findings: vec![],
            scan_medium: false,
            tools_observed: true,
        }
    }

    fn verdict(rule: &dyn Rule, f: McpFacts) -> (Verdict, String) {
        let v = rule.evaluate(&PolicyConfig::default(), &resolution(Format::Mcp, f), Utc::now()).unwrap();
        (v.verdict, v.reason)
    }

    #[test]
    fn every_mcp_rule_is_not_applicable_elsewhere() {
        for rule in [&McpAllowlist as &dyn Rule, &McpInjection, &McpTransport, &McpDrift] {
            let v = rule.evaluate(&PolicyConfig::default(), &resolution(Format::Npm, base()), Utc::now()).unwrap();
            assert_eq!(v.verdict, Verdict::NotApplicable, "{}", rule.name());
        }
    }

    #[test]
    fn the_allowlist_rule_names_the_addressed_repository() {
        let (v, why) = verdict(&McpAllowlist, McpFacts { allowed: false, ..base() });
        assert_eq!(v, Verdict::WouldBlock);
        assert_eq!(why, "io.github.acme/x matches no allow rule of team");
    }

    #[test]
    fn injection_gates_on_high_is_unknown_unobserved_and_counts_medium_only_on_request() {
        let finding = |pattern: &str, high| McpFinding {
            pattern: pattern.into(),
            high,
            field: "tool.description".into(),
            tool: "search".into(),
        };
        let (v, why) = verdict(&McpInjection, McpFacts { findings: vec![finding("model_directive", true)], ..base() });
        assert_eq!((v, why.as_str()), (Verdict::WouldBlock, "model_directive in tool.description/search"));
        assert_eq!(verdict(&McpInjection, McpFacts { findings: vec![finding("cross_tool", false)], ..base() }).0, Verdict::Pass);
        let medium = McpFacts {
            findings: vec![finding("cross_tool", false)],
            scan_medium: true,
            ..base()
        };
        assert_eq!(verdict(&McpInjection, medium).0, Verdict::WouldBlock);
        assert_eq!(verdict(&McpInjection, McpFacts { tools_observed: false, ..base() }).0, Verdict::Unknown);
    }

    #[test]
    fn transport_quantifies_over_both_sets() {
        let stdio = McpFacts {
            package_transports: vec!["stdio".into()],
            approval: Approval::Pending,
            ..base()
        };
        assert_eq!(verdict(&McpTransport, stdio.clone()), (Verdict::WouldBlock, "stdio package not approved".into()));
        assert_eq!(verdict(&McpTransport, McpFacts { approval: Approval::Approved, ..stdio }).0, Verdict::Pass);
        let mixed = McpFacts {
            remote_transports: vec!["sse".into(), "streamable-http".into()],
            ..base()
        };
        assert_eq!(verdict(&McpTransport, mixed).0, Verdict::Pass, "a good endpoint beside an sse one");
        let sse = McpFacts {
            remote_transports: vec!["sse".into()],
            ..base()
        };
        assert_eq!(verdict(&McpTransport, sse).1, "sse transport is deprecated");
    }

    #[test]
    fn drift_is_unknown_before_any_review_and_names_the_endpoint() {
        assert_eq!(verdict(&McpDrift, McpFacts { reviewed: false, ..base() }).0, Verdict::Unknown);
        let moved = McpFacts {
            drift: Drift::Tools,
            drifted_remote: Some("https://b".into()),
            ..base()
        };
        assert_eq!(verdict(&McpDrift, moved), (Verdict::WouldBlock, "tool descriptions changed at https://b since approval".into()));
        let new = McpFacts {
            drift: Drift::NewEndpoint,
            drifted_remote: Some("https://c".into()),
            ..base()
        };
        assert_eq!(verdict(&McpDrift, new).1, "a new endpoint at https://c appeared since approval");
    }
}

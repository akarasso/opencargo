//! Per item: its target format, repository and name, whether the filters
//! keep it, and whether the target could publish it at all — all decided
//! before a byte moves.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::import::{glob_match, oci_qualifier, GapKind};
use crate::domain::{Format, FormatRules};
use crate::ports::import::{Gap, Item, Planned};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanRules {
    /// `src=dst`, first match wins; `src` is a glob over the source repository.
    pub maps: Vec<(String, String)>,
    pub target_repo: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub latest_only: bool,
    pub max_versions: Option<usize>,
    pub flatten_names: bool,
}

pub type RulesOf<'a> = &'a (dyn Fn(Format) -> Option<&'static dyn FormatRules> + Sync);

pub struct Planner<'a> {
    pub rules: &'a PlanRules,
    /// Formats this run has a sink for.
    pub sinks: &'a HashSet<Format>,
    pub names: RulesOf<'a>,
}

impl PlanRules {
    pub fn parse_map(raw: &str) -> Result<(String, String), String> {
        let (src, dst) = raw
            .split_once('=')
            .filter(|(s, d)| !s.is_empty() && !d.is_empty())
            .ok_or_else(|| format!("--map {raw}: expected SRC=DST"))?;
        Ok((src.to_string(), dst.to_string()))
    }

    pub fn target_repo(&self, source_repo: &str) -> Option<&str> {
        self.maps
            .iter()
            .find(|(src, _)| glob_match(src, source_repo))
            .map(|(_, dst)| dst.as_str())
            .or(self.target_repo.as_deref())
    }

    /// The repositories the operator named, whatever the source holds.
    pub fn named_repos(&self) -> Vec<String> {
        let mut out: Vec<String> = self.maps.iter().map(|(_, d)| d.clone()).collect();
        out.extend(self.target_repo.clone());
        out.sort();
        out.dedup();
        out
    }
}

fn keep_by_filters(rules: &PlanRules, it: &Item) -> bool {
    let key = format!("{}/{}", it.coord.repo, it.coord.name);
    if !rules.include.is_empty() && !rules.include.iter().any(|g| glob_match(g, &key)) {
        return false;
    }
    if rules.exclude.iter().any(|g| glob_match(g, &key)) {
        return false;
    }
    match it.published_at {
        Some(at) => rules.since.is_none_or(|s| at >= s) && rules.until.is_none_or(|u| at <= u),
        None => true,
    }
}

fn version_key(v: &str) -> (Option<semver::Version>, String) {
    (semver::Version::parse(v.trim_start_matches('v')).ok(), v.to_string())
}

impl Planner<'_> {
    fn target_name(&self, it: &Item, format: Format) -> Result<String, String> {
        if format != Format::Oci || self.rules.flatten_names {
            return Ok(it.coord.name.clone());
        }
        let q = oci_qualifier(&it.coord.repo).ok_or_else(|| {
            format!("source namespace '{}' leaves no valid OCI segment; route it with --map", it.coord.repo)
        })?;
        Ok(format!("{q}/{}", it.coord.name))
    }

    fn plan_one(&self, it: Item) -> Result<Planned, Gap> {
        let Some(format) = it.format.target() else {
            return Err(Gap::new(
                GapKind::UnsupportedFormat,
                &it.coord.repo,
                format!("{} is not a format this registry serves", it.format.as_str()),
            ));
        };
        if !self.sinks.contains(&format) {
            return Err(Gap::new(
                GapKind::UnsupportedFormat,
                &it.coord.repo,
                format!("the importer has no {} sink yet", format.as_str()),
            ));
        }
        let Some(repo) = self.rules.target_repo(&it.coord.repo).map(String::from) else {
            return Err(Gap::new(
                GapKind::NoTarget,
                &it.source_ref,
                format!("no --map or --target-repo for source repository '{}'", it.coord.repo),
            ));
        };
        let name = self
            .target_name(&it, format)
            .map_err(|e| Gap::new(GapKind::UnpublishableName, &it.source_ref, e))?;
        if let Some(rules) = (self.names)(format) {
            if let Err(e) = rules.admit(&name) {
                return Err(Gap::new(GapKind::UnpublishableName, &it.source_ref, e.to_string()));
            }
            if let Err(e) = rules.validate_version(&it.coord.version) {
                return Err(Gap::new(GapKind::UnpublishableName, &it.source_ref, e.to_string()));
            }
        }
        Ok(Planned { item: it, target_repo: repo, target_name: name, target_format: format })
    }

    /// Filters, then the version limits per package, then the target rules.
    pub fn plan(&self, items: Vec<Item>) -> (Vec<Planned>, Vec<Gap>) {
        let mut by_pkg: BTreeMap<(String, String), Vec<Item>> = BTreeMap::new();
        for it in items.into_iter().filter(|it| keep_by_filters(self.rules, it)) {
            by_pkg.entry((it.coord.repo.clone(), it.coord.name.clone())).or_default().push(it);
        }
        let limit = if self.rules.latest_only { Some(1) } else { self.rules.max_versions };
        let mut planned = Vec::new();
        let mut gaps = Vec::new();
        for (_, mut versions) in by_pkg {
            if let Some(n) = limit {
                versions.sort_by_key(|b| std::cmp::Reverse(version_key(&b.coord.version)));
                versions.truncate(n);
            }
            for it in versions {
                match self.plan_one(it) {
                    Ok(p) => planned.push(p),
                    Err(g) => gaps.push(g),
                }
            }
        }
        (planned, gaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::import::{Coord, Digests, Origin, PkgExtra, SourceFormat, VersionExtra};

    fn item(repo: &str, name: &str, version: &str, format: SourceFormat) -> Item {
        Item {
            source_ref: format!("{repo}/{name}@{version}"),
            format,
            coord: Coord { repo: repo.into(), name: name.into(), version: version.into() },
            published_at: None,
            size: None,
            want: Digests::default(),
            origin: Origin::Npm { registry: "https://s/".into(), package: name.into() },
            pkg: PkgExtra::default(),
            extra: VersionExtra::default(),
        }
    }

    fn all_sinks() -> HashSet<Format> {
        Format::ALL.into_iter().collect()
    }

    fn planner<'a>(rules: &'a PlanRules, sinks: &'a HashSet<Format>) -> Planner<'a> {
        Planner { rules, sinks, names: &crate::registry::rules::rules }
    }

    #[test]
    fn npm_names_are_never_qualified() {
        let rules = PlanRules { target_repo: Some("t".into()), ..Default::default() };
        let sinks = all_sinks();
        let (p, g) = planner(&rules, &sinks).plan(vec![item("npm-internal", "@acme/utils", "1.0.0", SourceFormat::Npm)]);
        assert!(g.is_empty(), "{g:?}");
        assert_eq!(p[0].target_name, "@acme/utils");
    }

    #[test]
    fn oci_names_are_qualified_by_their_normalised_namespace() {
        let rules = PlanRules { target_repo: Some("t".into()), ..Default::default() };
        let sinks = all_sinks();
        let (p, _) = planner(&rules, &sinks).plan(vec![
            item("docker-Hosted", "nginx", "build_1234", SourceFormat::Oci),
            item("proj-b", "nginx", "latest", SourceFormat::Oci),
        ]);
        let names: Vec<_> = p.iter().map(|p| p.target_name.as_str()).collect();
        assert_eq!(names, ["docker-hosted/nginx", "proj-b/nginx"]);
        let flat = PlanRules { flatten_names: true, ..rules };
        let (p, _) = planner(&flat, &sinks).plan(vec![item("proj", "nginx", "1", SourceFormat::Oci)]);
        assert_eq!(p[0].target_name, "nginx");
    }

    #[test]
    fn oci_tag_with_an_underscore_is_planned_not_gapped() {
        let rules = PlanRules { target_repo: Some("t".into()), ..Default::default() };
        let sinks = all_sinks();
        let (p, g) = planner(&rules, &sinks).plan(vec![item("p", "app", "build_1234", SourceFormat::Oci)]);
        assert!(g.is_empty(), "{g:?}");
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn unpublishable_name_is_a_plan_time_gap() {
        let rules = PlanRules { target_repo: Some("t".into()), ..Default::default() };
        let sinks = all_sinks();
        let (p, g) = planner(&rules, &sinks).plan(vec![
            item("r", "JSONStream", "1.0.0", SourceFormat::Npm),
            item("r", "ok", "1.0.0_bad", SourceFormat::Npm),
        ]);
        assert!(p.is_empty());
        assert_eq!(g.len(), 2);
        assert!(g.iter().all(|g| g.kind == GapKind::UnpublishableName));
    }

    #[test]
    fn unsupported_and_untargeted_items_are_gaps() {
        let rules = PlanRules { maps: vec![("npm-*".into(), "npm".into())], ..Default::default() };
        let sinks: HashSet<Format> = [Format::Npm].into_iter().collect();
        let (p, g) = planner(&rules, &sinks).plan(vec![
            item("raw-files", "a", "1", SourceFormat::Raw),
            item("npm-a", "a", "1.0.0", SourceFormat::Npm),
            item("other", "b", "1.0.0", SourceFormat::Npm),
            item("npm-a", "c", "1.0.0", SourceFormat::Cargo),
        ]);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].target_repo, "npm");
        let kinds: Vec<_> = g.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, [GapKind::UnsupportedFormat, GapKind::NoTarget, GapKind::UnsupportedFormat]);
    }

    #[test]
    fn filters_include_exclude_since_max_versions() {
        let now = Utc::now();
        let mut old = item("r", "a", "1.0.0", SourceFormat::Npm);
        old.published_at = Some(now - chrono::Duration::days(40));
        let mut newer = item("r", "a", "1.1.0", SourceFormat::Npm);
        newer.published_at = Some(now);
        let newest = item("r", "a", "2.0.0", SourceFormat::Npm);
        let excluded = item("r", "secret", "1.0.0", SourceFormat::Npm);
        let rules = PlanRules {
            target_repo: Some("t".into()),
            exclude: vec!["r/secret".into()],
            since: Some(now - chrono::Duration::days(30)),
            max_versions: Some(1),
            ..Default::default()
        };
        let sinks = all_sinks();
        let (p, _) = planner(&rules, &sinks).plan(vec![old, newer.clone(), newest, excluded]);
        let v: Vec<_> = p.iter().map(|p| p.item.coord.version.as_str()).collect();
        assert_eq!(v, ["2.0.0"]);
        let rules = PlanRules { include: vec!["r/a".into()], target_repo: Some("t".into()), ..Default::default() };
        let (p, _) = planner(&rules, &sinks).plan(vec![newer, item("r", "b", "1.0.0", SourceFormat::Npm)]);
        assert_eq!(p.len(), 1);
    }
}

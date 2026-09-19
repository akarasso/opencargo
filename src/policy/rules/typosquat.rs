use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};

use super::{PolicyConfig, Rule};
use crate::domain::Format;
use crate::policy::distance::{names, normalize, one_edit};
use crate::domain::{RuleVerdict, Verdict};
use crate::policy::Resolution;

const MIN_WORD: usize = 5;

/// Top names bucketed by length and first or last two bytes: an edit
/// touches at most two adjacent positions, so a name of 5+ characters
/// keeps one of the two bigrams, and a candidate reads two buckets per
/// length instead of the whole list.
#[derive(Default)]
struct Pool {
    by_prefix: HashMap<(usize, [u8; 2]), Vec<String>>,
    by_suffix: HashMap<(usize, [u8; 2]), Vec<String>>,
}

fn bigrams(name: &str) -> Option<([u8; 2], [u8; 2])> {
    let b = name.as_bytes();
    (b.len() >= MIN_WORD).then(|| ([b[0], b[1]], [b[b.len() - 2], b[b.len() - 1]]))
}

impl Pool {
    fn insert(&mut self, name: String) {
        let Some((prefix, suffix)) = bigrams(&name) else {
            return;
        };
        let len = name.len();
        self.by_prefix
            .entry((len, prefix))
            .or_default()
            .push(name.clone());
        self.by_suffix.entry((len, suffix)).or_default().push(name);
    }

    /// The top names one edit from `candidate` could come from.
    fn candidates<'a>(&'a self, candidate: &str) -> impl Iterator<Item = &'a String> {
        let len = candidate.len();
        let keys = bigrams(candidate).map_or(Vec::new(), |(prefix, suffix)| {
            vec![
                (&self.by_prefix, (len, prefix)),
                (&self.by_suffix, (len, suffix)),
                (&self.by_prefix, (len + 1, prefix)),
                (&self.by_suffix, (len + 1, suffix)),
            ]
        });
        keys.into_iter()
            .filter_map(|(map, key)| map.get(&key))
            .flatten()
    }
}

/// One ecosystem's lists, parsed once: the top names compared against and
/// the known names that pass untouched (`lists/NOTICE` names the sources).
pub struct Lists {
    top: HashSet<String>,
    unscoped: Pool,
    scoped: Pool,
    scopes: HashSet<String>,
    known: HashSet<String>,
}

impl Lists {
    pub fn parse(top: &str, known: &str) -> Self {
        let top = names(top);
        let (mut unscoped, mut scoped) = (Pool::default(), Pool::default());
        let mut scopes = HashSet::new();
        for name in &top {
            match name.strip_prefix('@').and_then(|n| n.split_once('/')) {
                Some((scope, _)) => {
                    scopes.insert(scope.to_string());
                    scoped.insert(name.clone());
                }
                None => unscoped.insert(name.clone()),
            }
        }
        Self {
            top,
            unscoped,
            scoped,
            scopes,
            known: names(known),
        }
    }

    pub fn top_names(&self) -> impl Iterator<Item = &str> {
        self.top.iter().map(String::as_str)
    }

    pub fn known_names(&self) -> impl Iterator<Item = &str> {
        self.known.iter().map(String::as_str)
    }
}

pub fn shipped(format: Format) -> Option<&'static Lists> {
    static NPM: OnceLock<Lists> = OnceLock::new();
    static CRATES: OnceLock<Lists> = OnceLock::new();
    static GO: OnceLock<Lists> = OnceLock::new();
    static PYPI: OnceLock<Lists> = OnceLock::new();
    Some(match format {
        Format::Npm => NPM.get_or_init(|| {
            Lists::parse(
                include_str!("../lists/npm.txt"),
                include_str!("../lists/known/npm.txt"),
            )
        }),
        Format::Cargo => CRATES.get_or_init(|| {
            Lists::parse(
                include_str!("../lists/crates.txt"),
                include_str!("../lists/known/crates.txt"),
            )
        }),
        Format::Go => GO.get_or_init(|| {
            Lists::parse(
                include_str!("../lists/go.txt"),
                include_str!("../lists/known/go.txt"),
            )
        }),
        Format::Pypi => PYPI.get_or_init(|| {
            Lists::parse(
                include_str!("../lists/pypi.txt"),
                include_str!("../lists/known/pypi.txt"),
            )
        }),
        Format::Oci | Format::Maven | Format::Nuget | Format::Mcp | Format::Raw => return None,
    })
}

/// The pure check: exact, known and top-scoped names pass, short words are
/// not compared, the rest is one edit against the pool of its own kind.
pub fn classify(lists: &Lists, format: Format, name: &str) -> (Verdict, String) {
    let n = normalize(format, name);
    let full = n.full();
    if lists.top.contains(&full) {
        return (Verdict::Pass, "exact match of a top-N name".into());
    }
    if lists.known.contains(&full) {
        return (Verdict::Pass, "known package, not a squat".into());
    }
    if let Some(scope) = n.scope.as_ref().filter(|s| lists.scopes.contains(*s)) {
        return (Verdict::Pass, format!("scope @{scope} is itself top-N"));
    }
    if n.word.chars().count() < MIN_WORD {
        return (Verdict::Pass, "too short for a 1-edit comparison".into());
    }
    let pool = if n.scope.is_some() {
        &lists.scoped
    } else {
        &lists.unscoped
    };
    let hit = pool
        .candidates(&full)
        .find_map(|top| one_edit(&full, top).map(|edit| (edit, top)));
    match hit {
        Some((edit, top)) => (
            Verdict::WouldBlock,
            format!("{} of '{top}', not in the top 20 k", edit.as_str()),
        ),
        None => (Verdict::Pass, "no top-N name within one edit".into()),
    }
}

pub struct Typosquat;

impl Rule for Typosquat {
    fn name(&self) -> &'static str {
        "typosquat"
    }

    fn enabled(&self, cfg: &PolicyConfig) -> bool {
        cfg.typosquat
    }

    fn evaluate(&self, _: &PolicyConfig, r: &Resolution, _: DateTime<Utc>) -> Option<RuleVerdict> {
        let (verdict, reason) = match (r.format, shipped(r.format)) {
            (Format::Oci, _) => (
                Verdict::NotApplicable,
                "oci: a misspelt official image is never served".into(),
            ),
            (format, None) => (
                Verdict::NotApplicable,
                format!("{}: no name list", format.as_str()),
            ),
            (format, Some(lists)) => classify(lists, format, &r.name),
        };
        Some(RuleVerdict::new(self.name(), verdict, reason))
    }
}

#[cfg(test)]
#[path = "typosquat_tests.rs"]
mod tests;

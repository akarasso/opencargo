//! The gap report: everything not copied, derived from the journal's current
//! state rather than accumulated, so a fixed cause leaves it on the next run.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::Serialize;

use crate::domain::import::{exit_code, GapKind, ItemStatus};
use crate::ports::import::{Gap, ImportJournal, JournalError, Journaled, RunHeader};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub kind: GapKind,
    pub source_ref: String,
    pub detail: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemLine {
    pub source_ref: String,
    pub target: String,
    pub status: ItemStatus,
    pub bytes: Option<u64>,
    pub note: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub source: Option<String>,
    pub source_url: Option<String>,
    pub target_url: Option<String>,
    pub phase: Option<String>,
    pub counts: BTreeMap<String, u64>,
    pub gaps: Vec<Row>,
    pub items: Vec<ItemLine>,
    pub exit_code: u8,
}

/// Failures and annotations carry their kind as a prefix, `Kind: message`,
/// which is how an item re-derives its gap without a stored row.
pub fn tagged(kind: GapKind, message: &str) -> String {
    format!("{}: {message}", kind.as_str())
}

pub fn untag(s: &str) -> Option<(GapKind, &str)> {
    let (kind, rest) = s.split_once(": ")?;
    Some((GapKind::from_str(kind).ok()?, rest))
}

fn item_gaps(it: &Journaled) -> Vec<Gap> {
    let source_ref = &it.planned.item.source_ref;
    let mut out = Vec::new();
    if it.status == ItemStatus::Failed {
        let (kind, detail) = it
            .error
            .as_deref()
            .and_then(untag)
            .filter(|(k, _)| k.item_derived())
            .unwrap_or((GapKind::Failed, it.error.as_deref().unwrap_or("failed")));
        out.push(Gap::new(kind, source_ref, detail));
    }
    if let Some((kind, detail)) = it.note.as_deref().and_then(untag).filter(|(k, _)| k.item_derived()) {
        out.push(Gap::new(kind, source_ref, detail));
    }
    out
}

fn target_of(it: &Journaled) -> String {
    let p = &it.planned;
    format!("{}/{}@{}", p.target_repo, p.target_name, p.item.coord.version)
}

/// Rows sharing a kind and a detail collapse above `collapse` into one.
pub fn build(
    header: Option<&RunHeader>,
    items: &[Journaled],
    stored: &[Gap],
    allow_incomplete: bool,
    collapse: usize,
) -> Report {
    let mut counts = BTreeMap::new();
    let mut rows: BTreeMap<(GapKind, String, String), u64> = BTreeMap::new();
    for it in items {
        *counts.entry(it.status.as_str().to_string()).or_insert(0) += 1;
        for g in item_gaps(it) {
            *rows.entry((g.kind, g.source_ref, g.detail)).or_insert(0) += 1;
        }
    }
    for g in stored {
        *rows.entry((g.kind, g.source_ref.clone(), g.detail.clone())).or_insert(0) += 1;
    }
    let code = exit_code(rows.keys().map(|(k, _, _)| *k), allow_incomplete);
    let mut by_fact: BTreeMap<(GapKind, String), Vec<(String, u64)>> = BTreeMap::new();
    for ((kind, source_ref, detail), count) in rows {
        by_fact.entry((kind, detail)).or_default().push((source_ref, count));
    }
    let mut gaps = Vec::new();
    for ((kind, detail), refs) in by_fact {
        if collapse > 0 && refs.len() > collapse {
            gaps.push(Row {
                kind,
                source_ref: format!("{} coordinates", refs.len()),
                detail,
                count: refs.iter().map(|(_, c)| c).sum(),
            });
        } else {
            for (source_ref, count) in refs {
                gaps.push(Row { kind, source_ref, detail: detail.clone(), count });
            }
        }
    }
    gaps.sort_by(|a, b| (a.kind, &a.source_ref, &a.detail).cmp(&(b.kind, &b.source_ref, &b.detail)));
    let lines = items
        .iter()
        .map(|it| ItemLine {
            source_ref: it.planned.item.source_ref.clone(),
            target: target_of(it),
            status: it.status,
            bytes: it.bytes,
            note: it.note.clone(),
            error: it.error.clone(),
        })
        .collect();
    Report {
        source: header.map(|h| h.source.clone()),
        source_url: header.map(|h| h.source_url.clone()),
        target_url: header.map(|h| h.target_url.clone()),
        phase: header.map(|h| h.phase.clone()),
        counts,
        gaps,
        items: lines,
        exit_code: code,
    }
}

pub async fn from_journal(
    journal: &dyn ImportJournal,
    allow_incomplete: bool,
    collapse: usize,
) -> Result<Report, JournalError> {
    let header = journal.header().await?;
    let items = journal.items().await?;
    let gaps = journal.gaps().await?;
    Ok(build(header.as_ref(), &items, &gaps, allow_incomplete, collapse))
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

pub fn render_json(r: &Report) -> String {
    serde_json::to_string_pretty(r).unwrap_or_default()
}

pub fn render_table(r: &Report) -> String {
    let mut out = String::new();
    if let (Some(source), Some(from), Some(to)) = (&r.source, &r.source_url, &r.target_url) {
        out.push_str(&format!("# Import report\n\n{source} {from} -> {to}"));
        if let Some(phase) = &r.phase {
            out.push_str(&format!(" ({phase})"));
        }
        out.push_str("\n\n");
    }
    let counts: Vec<String> = r.counts.iter().map(|(k, v)| format!("{k} {v}")).collect();
    out.push_str(&format!("items: {}\n\n", if counts.is_empty() { "none".into() } else { counts.join(", ") }));
    let notes: Vec<&ItemLine> = r.items.iter().filter(|i| i.note.is_some()).collect();
    if !notes.is_empty() {
        out.push_str("| item | note |\n|---|---|\n");
        for i in notes {
            out.push_str(&format!("| {} | {} |\n", cell(&i.target), cell(i.note.as_deref().unwrap_or(""))));
        }
        out.push('\n');
    }
    if r.gaps.is_empty() {
        out.push_str("no gaps\n");
    } else {
        out.push_str("| kind | source | detail | count |\n|---|---|---|---|\n");
        for g in &r.gaps {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                g.kind,
                cell(&g.source_ref),
                cell(&g.detail),
                g.count
            ));
        }
    }
    out.push_str(&format!("\nexit code {}\n", r.exit_code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Format;
    use crate::ports::import::{Coord, Digests, Item, Origin, PkgExtra, Planned, SourceFormat, VersionExtra};

    fn item(source_ref: &str, status: ItemStatus, error: Option<&str>, note: Option<&str>) -> Journaled {
        Journaled {
            planned: Planned {
                item: Item {
                    source_ref: source_ref.into(),
                    format: SourceFormat::Npm,
                    coord: Coord { repo: "r".into(), name: source_ref.into(), version: "1.0.0".into() },
                    published_at: None,
                    size: None,
                    want: Digests::default(),
                    origin: Origin::Npm { registry: "https://s/".into(), package: source_ref.into() },
                    pkg: PkgExtra::default(),
                    extra: VersionExtra::default(),
                },
                target_repo: "t".into(),
                target_name: source_ref.into(),
                target_format: Format::Npm,
            },
            status,
            attempts: 1,
            bytes: None,
            sha256: None,
            error: error.map(String::from),
            note: note.map(String::from),
        }
    }

    #[test]
    fn ordering_is_deterministic() {
        let items = vec![
            item("b", ItemStatus::Failed, Some("TooLarge: 10 bytes over the 5-byte limit"), None),
            item("a", ItemStatus::Failed, Some("checksum mismatch"), None),
            item("c", ItemStatus::Skipped, None, Some("SkippedUnverifiable: go exposes no checksum")),
        ];
        let stored = vec![Gap::new(GapKind::NoTarget, "z", "no --map"), Gap::new(GapKind::NoTarget, "y", "no --map")];
        let one = build(None, &items, &stored, false, 20);
        let mut reversed = items.clone();
        reversed.reverse();
        let two = build(None, &reversed, &stored.iter().rev().cloned().collect::<Vec<_>>(), false, 20);
        assert_eq!(one.gaps, two.gaps);
        let kinds: Vec<_> = one.gaps.iter().map(|g| (g.kind, g.source_ref.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (GapKind::NoTarget, "y"),
                (GapKind::NoTarget, "z"),
                (GapKind::SkippedUnverifiable, "c"),
                (GapKind::TooLarge, "b"),
                (GapKind::Failed, "a"),
            ]
        );
        assert_eq!(one.exit_code, 2);
    }

    #[test]
    fn a_copied_item_leaves_no_gap_behind() {
        let fixed = vec![item("b", ItemStatus::Copied, None, Some("Retagged sha256:1 -> sha256:2"))];
        let r = build(None, &fixed, &[], false, 20);
        assert!(r.gaps.is_empty());
        assert_eq!(r.exit_code, 0);
        assert!(render_table(&r).contains("Retagged"));
    }

    #[test]
    fn collapse_threshold_aggregates_rows() {
        let stored: Vec<Gap> =
            (0..25).map(|i| Gap::new(GapKind::UnsupportedFormat, format!("r/{i}"), "raw")).collect();
        let r = build(None, &[], &stored, false, 20);
        assert_eq!(r.gaps.len(), 1);
        assert_eq!(r.gaps[0].source_ref, "25 coordinates");
        assert_eq!(r.gaps[0].count, 25);
        assert_eq!(build(None, &[], &stored, false, 0).gaps.len(), 25);
        assert_eq!(build(None, &[], &stored, true, 0).exit_code, 0);
    }
}

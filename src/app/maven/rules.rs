//! What a deposit into a unit may do, decided on the unit as read: pure, so
//! the use case can decide again on a fresher read whenever its
//! compare-and-set loses.

use chrono::{DateTime, Utc};

use crate::ports::maven::{Digests, SumAlgorithm, Unit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileVerdict {
    /// Record the file; `replaces` a pending file of the same name.
    Store { replaces: bool, reveal: bool },
    /// The same bytes are already there under that name.
    Identical,
    /// Another principal's different bytes, or new file, in a pending unit:
    /// refused, and the unit marked contested.
    Contest,
    /// A visible file never changes, a visible unit takes files from its
    /// depositor only, a refused unit takes nothing.
    Refuse(Refusal),
    /// The depositor's own declaration for this file disagrees.
    Mismatch(SumAlgorithm),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Immutable,
    NotDepositor,
    Refused,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::Immutable => "the file is published and immutable",
            Refusal::NotDepositor => "the version belongs to another depositor",
            Refusal::Refused => "the version was refused",
        }
    }
}

/// Declarations still waiting for their file once `extra` is added.
fn waiting(unit: &Unit, extra: Option<&str>) -> bool {
    unit.declarations.iter().any(|d| {
        Some(d.filename.as_str()) != extra && unit.file(&d.filename).is_none()
    })
}

fn has_pom(unit: &Unit, extra_pom: bool) -> bool {
    extra_pom || unit.files.iter().any(|f| f.filename.ends_with(".pom"))
}

pub fn judge_file(
    unit: Option<&Unit>,
    filename: &str,
    digests: &Digests,
    principal: &str,
    pom: bool,
) -> FileVerdict {
    let Some(unit) = unit else {
        return FileVerdict::Store {
            replaces: false,
            reveal: pom,
        };
    };
    if unit.refused {
        return FileVerdict::Refuse(Refusal::Refused);
    }
    let own = unit.depositor == principal;
    let reveal = |extra: Option<&str>| {
        !unit.visible() && own && has_pom(unit, pom) && !waiting(unit, extra)
    };
    if let Some(existing) = unit.file(filename) {
        if existing.digests.sha256 == digests.sha256 {
            return FileVerdict::Identical;
        }
        return match (unit.visible(), own) {
            (true, _) => FileVerdict::Refuse(Refusal::Immutable),
            (false, true) => FileVerdict::Store {
                replaces: true,
                reveal: reveal(Some(filename)),
            },
            (false, false) => FileVerdict::Contest,
        };
    }
    match (unit.visible(), own) {
        (true, false) => FileVerdict::Refuse(Refusal::NotDepositor),
        (false, false) => FileVerdict::Contest,
        (_, true) => {
            let disagrees = unit
                .declarations
                .iter()
                .find(|d| d.filename == filename && digests.get(d.algorithm) != d.value);
            match disagrees {
                Some(d) => FileVerdict::Mismatch(d.algorithm),
                None => FileVerdict::Store {
                    replaces: false,
                    reveal: reveal(Some(filename)),
                },
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SumVerdict {
    /// Keep the declaration: to check the file when it comes, or as agreed.
    Record,
    /// It agrees with the stored file and there is nothing to keep.
    Agrees,
    Contest,
    Refuse(Refusal),
    Mismatch,
}

pub fn judge_sum(
    unit: Option<&Unit>,
    filename: &str,
    algorithm: SumAlgorithm,
    value: &str,
    principal: &str,
) -> SumVerdict {
    let Some(unit) = unit else {
        return SumVerdict::Record;
    };
    if unit.refused {
        return SumVerdict::Refuse(Refusal::Refused);
    }
    let own = unit.depositor == principal;
    let agrees = unit
        .file(filename)
        .map(|f| f.digests.get(algorithm) == value);
    match (agrees, own, unit.visible()) {
        (Some(true), true, _) => SumVerdict::Record,
        (Some(true), false, _) => SumVerdict::Agrees,
        (Some(false), true, _) => SumVerdict::Mismatch,
        (None, true, _) => SumVerdict::Record,
        (_, false, false) => SumVerdict::Contest,
        (_, false, true) => SumVerdict::Refuse(Refusal::NotDepositor),
    }
}

/// Whether `RunCleanup` may make a pending unit visible without its POM:
/// old enough, never contested, and no declaration still waiting.
pub fn promotable(unit: &Unit, now: DateTime<Utc>, window: chrono::Duration) -> bool {
    !unit.visible()
        && !unit.refused
        && !unit.contested
        && !unit.files.is_empty()
        && !waiting(unit, None)
        && unit.created_at + window <= now
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::ports::maven::{Declaration, StoredFile};

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 12, minute, 0).unwrap()
    }

    fn digests(seed: &str) -> Digests {
        Digests {
            sha1: format!("{seed}1"),
            md5: format!("{seed}5"),
            sha256: format!("{seed}256"),
            sha512: format!("{seed}512"),
        }
    }

    fn unit(files: &[(&str, &str)], declarations: &[(&str, SumAlgorithm, &str)]) -> Unit {
        Unit {
            version: "1.0".into(),
            build: String::new(),
            revision: 1,
            depositor: "alice".into(),
            contested: false,
            refused: false,
            visible_at: None,
            created_at: at(0),
            files: files
                .iter()
                .map(|(name, seed)| StoredFile {
                    filename: name.to_string(),
                    physical_key: format!("k/{name}"),
                    size: 1,
                    digests: digests(seed),
                    depositor: "alice".into(),
                    created_at: at(0),
                })
                .collect(),
            declarations: declarations
                .iter()
                .map(|(f, a, v)| Declaration {
                    filename: f.to_string(),
                    algorithm: *a,
                    value: v.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_pom_reveals_its_unit_unless_a_declaration_waits() {
        let jar = unit(&[("a-1.0.jar", "j")], &[]);
        assert_eq!(
            judge_file(Some(&jar), "a-1.0.pom", &digests("p"), "alice", true),
            FileVerdict::Store { replaces: false, reveal: true }
        );
        let waits = unit(&[("a-1.0.jar", "j")], &[("a-1.0-sources.jar", SumAlgorithm::Sha1, "s1")]);
        assert_eq!(
            judge_file(Some(&waits), "a-1.0.pom", &digests("p"), "alice", true),
            FileVerdict::Store { replaces: false, reveal: false }
        );
        assert_eq!(
            judge_file(None, "a-1.0.jar", &digests("j"), "alice", false),
            FileVerdict::Store { replaces: false, reveal: false }
        );
    }

    #[test]
    fn a_pending_file_is_replaced_by_its_depositor_and_contested_by_anyone_else() {
        let pending = unit(&[("a-1.0.jar", "j")], &[]);
        assert_eq!(
            judge_file(Some(&pending), "a-1.0.jar", &digests("j"), "bob", false),
            FileVerdict::Identical
        );
        assert_eq!(
            judge_file(Some(&pending), "a-1.0.jar", &digests("x"), "alice", false),
            FileVerdict::Store { replaces: true, reveal: false }
        );
        assert_eq!(
            judge_file(Some(&pending), "a-1.0.jar", &digests("x"), "bob", false),
            FileVerdict::Contest
        );
        assert_eq!(
            judge_file(Some(&pending), "a-1.0-evil.jar", &digests("x"), "bob", false),
            FileVerdict::Contest
        );
        let mut visible = pending.clone();
        visible.visible_at = Some(at(1));
        assert_eq!(
            judge_file(Some(&visible), "a-1.0.jar", &digests("x"), "alice", false),
            FileVerdict::Refuse(Refusal::Immutable)
        );
        assert_eq!(
            judge_file(Some(&visible), "a-1.0-sources.jar", &digests("s"), "bob", false),
            FileVerdict::Refuse(Refusal::NotDepositor)
        );
        assert!(matches!(
            judge_file(Some(&visible), "a-1.0-sources.jar", &digests("s"), "alice", false),
            FileVerdict::Store { replaces: false, reveal: false }
        ));
    }

    #[test]
    fn every_algorithm_is_checked_in_either_order() {
        for algorithm in SumAlgorithm::ALL {
            let right = digests("j").get(algorithm).to_string();
            let before = unit(&[], &[("a-1.0.jar", algorithm, "wrong")]);
            assert_eq!(
                judge_file(Some(&before), "a-1.0.jar", &digests("j"), "alice", false),
                FileVerdict::Mismatch(algorithm),
                "sum first, {algorithm:?}"
            );
            let agreed = unit(&[], &[("a-1.0.jar", algorithm, &right)]);
            assert!(matches!(
                judge_file(Some(&agreed), "a-1.0.jar", &digests("j"), "alice", false),
                FileVerdict::Store { .. }
            ));
            let after = unit(&[("a-1.0.jar", "j")], &[]);
            assert_eq!(judge_sum(Some(&after), "a-1.0.jar", algorithm, "wrong", "alice"), SumVerdict::Mismatch);
            assert_eq!(judge_sum(Some(&after), "a-1.0.jar", algorithm, &right, "alice"), SumVerdict::Record);
        }
    }

    #[test]
    fn a_third_party_declaration_contests_and_is_never_kept() {
        let pending = unit(&[("a-1.0.jar", "j")], &[]);
        assert_eq!(judge_sum(Some(&pending), "a-1.0.jar", SumAlgorithm::Sha1, "x", "bob"), SumVerdict::Contest);
        assert_eq!(judge_sum(Some(&pending), "a-1.0.pom", SumAlgorithm::Sha1, "x", "bob"), SumVerdict::Contest);
        assert_eq!(judge_sum(Some(&pending), "a-1.0.jar", SumAlgorithm::Sha1, "j1", "bob"), SumVerdict::Agrees);
        assert_eq!(judge_sum(None, "a-1.0.jar", SumAlgorithm::Sha1, "x", "bob"), SumVerdict::Record);
    }

    #[test]
    fn only_an_uncontested_quiet_unit_past_the_window_is_promoted() {
        let window = chrono::Duration::minutes(10);
        let jar = unit(&[("a-1.0.jar", "j")], &[]);
        assert!(promotable(&jar, at(10), window));
        assert!(!promotable(&jar, at(9), window));
        let mut contested = jar.clone();
        contested.contested = true;
        assert!(!promotable(&contested, at(30), window));
        let waits = unit(&[("a-1.0.jar", "j")], &[("a-1.0.pom", SumAlgorithm::Md5, "m")]);
        assert!(!promotable(&waits, at(30), window));
        assert!(!promotable(&unit(&[], &[]), at(30), window));
    }
}

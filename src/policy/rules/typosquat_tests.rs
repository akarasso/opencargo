use super::*;
use crate::policy::{Actor, Facts};

/// Ceilings set from the measured residual over the shipped lists: 67,
/// 101 and 5 once digit, separator and short-head substitutions read as
/// family markers (82, 138 and 5 before); PyPI measured 160.
const HOLDOUT_FP_MAX: [(Format, usize); 4] =
    [(Format::Npm, 75), (Format::Cargo, 110), (Format::Go, 10), (Format::Pypi, 175)];

fn lists(top: &str, known: &str) -> Lists {
    Lists::parse(top, known)
}

fn npm() -> Lists {
    lists(
        "lodash\ncross-env\nlodash.merge\nisarray\nisnumber\nobjectassign\nrequest\ncolors\nassert\nmoment\n@babel/core\ncors\n@types/node\n@types/lodash\nmysql\nes6-shim\nws\n",
        "mssql\nes5-shim\nis-array\nrequests\n",
    )
}

fn check(lists: &Lists, format: Format, name: &str) -> (Verdict, String) {
    classify(lists, format, name)
}

fn would_block(lists: &Lists, format: Format, name: &str) -> String {
    let (verdict, reason) = check(lists, format, name);
    assert_eq!(verdict, Verdict::WouldBlock, "{name}: {reason}");
    reason
}

fn passes(lists: &Lists, format: Format, name: &str) -> String {
    let (verdict, reason) = check(lists, format, name);
    assert_eq!(verdict, Verdict::Pass, "{name}: {reason}");
    reason
}

#[test]
fn exact_top_name_never_flagged() {
    for format in [Format::Npm, Format::Cargo, Format::Go] {
        let lists = shipped(format).unwrap();
        for name in lists.top_names() {
            assert_eq!(
                check(lists, format, name).0,
                Verdict::Pass,
                "{format:?} {name}"
            );
        }
    }
    assert_eq!(
        passes(&npm(), Format::Npm, "lodash"),
        "exact match of a top-N name"
    );
}

#[test]
fn substitution_flagged() {
    assert_eq!(
        would_block(&npm(), Format::Npm, "lodask"),
        "substitution of 'lodash', not in the top 20 k"
    );
    let shipped = shipped(Format::Npm).unwrap();
    assert_eq!(check(shipped, Format::Npm, "lodask").0, Verdict::WouldBlock);
}

#[test]
fn transposition_flagged() {
    assert_eq!(
        would_block(&npm(), Format::Npm, "lodahs"),
        "transposition of 'lodash', not in the top 20 k"
    );
}

#[test]
fn separator_dropped_flagged() {
    assert_eq!(
        would_block(&npm(), Format::Npm, "crossenv"),
        "dropped separator of 'cross-env', not in the top 20 k"
    );
    assert_eq!(
        would_block(&npm(), Format::Npm, "lodashmerge"),
        "dropped separator of 'lodash.merge', not in the top 20 k"
    );
}

#[test]
fn separator_added_passes() {
    let concatenated = lists("isarray\nisnumber\nobjectassign\n", "");
    for name in ["is-array", "is-number", "object-assign", "is.array"] {
        assert_eq!(
            passes(&concatenated, Format::Npm, name),
            "no top-N name within one edit"
        );
    }
}

#[test]
fn family_markers_pass() {
    let families = lists(
        "bzip2\nmurmur3\nis-array\ngit-config\nndk-sys\nsha1-asm\nreact-dom\n",
        "",
    );
    for name in [
        "bzip3",
        "murmur2",
        "is.array",
        "gix-config",
        "wdk-sys",
        "sha3-asm",
    ] {
        assert_eq!(
            passes(&families, Format::Npm, name),
            "no top-N name within one edit"
        );
    }
    would_block(&families, Format::Npm, "reakt-dom");
    would_block(&families, Format::Npm, "gitxconfig");
    let real = shipped(Format::Npm).unwrap();
    for name in ["axois", "hasky", "kocha", "lodask", "lodahs"] {
        assert_eq!(
            check(real, Format::Npm, name).0,
            Verdict::WouldBlock,
            "{name}"
        );
    }
    let crates = shipped(Format::Cargo).unwrap();
    for name in ["tokyo", "rustis"] {
        assert_eq!(
            check(crates, Format::Cargo, name).0,
            Verdict::WouldBlock,
            "{name}"
        );
    }
    for name in ["sha3-asm", "gix-config", "wdk-sys", "bzip3", "jl-sys"] {
        assert_eq!(
            check(crates, Format::Cargo, name).0,
            Verdict::Pass,
            "{name}"
        );
    }
}

#[test]
fn plural_suffix_passes() {
    let top_only = lists("request\ncolors\nassert\nmoment\n", "");
    for name in ["requests", "colours", "asserts", "moments"] {
        passes(&top_only, Format::Npm, name);
    }
}

#[test]
fn letter_dropped_passes() {
    let top_only = lists("lodash\n", "");
    passes(&top_only, Format::Npm, "lodas");
    passes(&top_only, Format::Npm, "loddash");
}

#[test]
fn scoped_top_scope_passes() {
    assert_eq!(
        passes(&npm(), Format::Npm, "@babel/core"),
        "exact match of a top-N name"
    );
    assert_eq!(
        passes(&npm(), Format::Npm, "@babel/cors"),
        "scope @babel is itself top-N"
    );
}

#[test]
fn scoped_only_against_scoped() {
    passes(&npm(), Format::Npm, "@acme/lodask");
    assert_eq!(
        would_block(&npm(), Format::Npm, "@typse/lodash"),
        "transposition of '@types/lodash', not in the top 20 k"
    );
    passes(&npm(), Format::Npm, "types-lodash");
}

#[test]
fn underscore_dash_normalised() {
    let crates = lists("serde-json\n", "");
    passes(&crates, Format::Cargo, "serde_json");
    passes(&crates, Format::Cargo, "Serde_JSON");
    would_block(&crates, Format::Cargo, "serde_jsom");
}

#[test]
fn go_major_version_bump_passes() {
    let go = lists("github.com/go-redis/redis\ngopkg.in/yaml\n", "");
    for name in [
        "github.com/go-redis/redis/v9",
        "github.com/go-redis/redis/v8",
        "github.com/go-redis/redis",
        "gopkg.in/yaml.v3",
        "gopkg.in/yaml.v2",
    ] {
        assert_eq!(passes(&go, Format::Go, name), "exact match of a top-N name");
    }
}

#[test]
fn go_one_edit_in_base_path_flagged() {
    let go = lists("github.com/go-redis/redis\n", "");
    assert_eq!(
        would_block(&go, Format::Go, "github.com/go-redis/redsi/v9"),
        "transposition of 'github.com/go-redis/redis', not in the top 20 k"
    );
    passes(&go, Format::Go, "github.com/go-redis/redis-v9");
    passes(&go, Format::Go, "github.com/go-redis/redis.v9");
}

#[test]
fn short_names_not_compared() {
    assert_eq!(
        passes(&npm(), Format::Npm, "vs"),
        "too short for a 1-edit comparison"
    );
    passes(&npm(), Format::Npm, "@acme/ws");
    let short_top = lists("abcd\n", "");
    passes(&short_top, Format::Npm, "abce");
}

#[test]
fn known_sibling_passes() {
    for name in ["mssql", "es5-shim", "is-array", "requests"] {
        assert_eq!(
            passes(&npm(), Format::Npm, name),
            "known package, not a squat"
        );
    }
    let crates = lists("base64\n", "base32\n");
    passes(&crates, Format::Cargo, "base32");
    would_block(&crates, Format::Cargo, "basf64");
    assert_eq!(
        passes(&crates, Format::Cargo, "base65"),
        "no top-N name within one edit",
        "a digit swap is a version marker, known list or not"
    );
    let real = shipped(Format::Npm).unwrap();
    for name in ["mssql", "es5-shim"] {
        assert_eq!(
            check(real, Format::Npm, name).1,
            "known package, not a squat",
            "{name} in the shipped known list"
        );
    }
}

fn list_text(format: Format, tier: &str) -> &'static str {
    match (format, tier) {
        (Format::Npm, "top") => include_str!("../lists/npm.txt"),
        (Format::Cargo, "top") => include_str!("../lists/crates.txt"),
        (Format::Go, "top") => include_str!("../lists/go.txt"),
        (Format::Pypi, "top") => include_str!("../lists/pypi.txt"),
        (Format::Npm, _) => include_str!("../lists/holdout/npm.txt"),
        (Format::Cargo, _) => include_str!("../lists/holdout/crates.txt"),
        (Format::Go, _) => include_str!("../lists/holdout/go.txt"),
        (Format::Pypi, _) => include_str!("../lists/holdout/pypi.txt"),
        _ => unreachable!(),
    }
}

#[test]
fn typosquat_holdout_false_positive_rate() {
    for (format, max) in HOLDOUT_FP_MAX {
        let lists = shipped(format).unwrap();
        let holdout = names(list_text(format, "holdout"));
        let flagged: Vec<(&str, String)> = holdout
            .iter()
            .filter_map(|name| {
                let (verdict, reason) = check(lists, format, name);
                (verdict == Verdict::WouldBlock).then_some((name.as_str(), reason))
            })
            .collect();
        println!(
            "{format:?}: {} of {} holdout names flagged, e.g. {:?}",
            flagged.len(),
            holdout.len(),
            flagged.iter().take(5).collect::<Vec<_>>()
        );
        assert!(
            holdout.len() > 10_000,
            "{format:?}: holdout is {}",
            holdout.len()
        );
        assert!(
            flagged.len() <= max,
            "{format:?}: {} holdout names flagged, ceiling {max}",
            flagged.len()
        );
    }
}

#[test]
fn known_list_suppresses_siblings() {
    for format in [Format::Npm, Format::Cargo, Format::Go, Format::Pypi] {
        let lists = shipped(format).unwrap();
        let without_known = Lists::parse(list_text(format, "top"), "");
        let siblings = lists
            .known_names()
            .filter(|name| check(&without_known, format, name).0 == Verdict::WouldBlock)
            .count();
        println!("{format:?}: the known list saves {siblings} names one edit from a top name");
        assert!(siblings > 0, "{format:?}");
        assert!(
            lists.known_names().count() > 10_000,
            "{format:?}: known is {}",
            lists.known_names().count()
        );
    }
}

#[test]
fn oci_not_applicable() {
    let r = Resolution {
        requested_repo: "r".into(),
        member_repo: "m".into(),
        format: Format::Oci,
        name: "ngnix".into(),
        version: Some("latest".into()),
        digest: None,
        actor: Actor::of(None),
        published_at: None,
        facts: Facts::default(),
    };
    let cfg = PolicyConfig {
        typosquat: true,
        ..Default::default()
    };
    let v = Typosquat.evaluate(&cfg, &r, Utc::now()).unwrap();
    assert_eq!(v.verdict, Verdict::NotApplicable);
    assert_eq!(v.reason, "oci: a misspelt official image is never served");
    let npm = Resolution {
        format: Format::Npm,
        name: "lodask".into(),
        ..r
    };
    assert_eq!(
        Typosquat.evaluate(&cfg, &npm, Utc::now()).unwrap().verdict,
        Verdict::WouldBlock
    );
}

#[test]
fn pypi_names_compare_as_pep_503_spells_them() {
    let pypi = shipped(Format::Pypi).unwrap();
    for spelling in ["requests", "Requests", "python.dateutil", "Python_DateUtil"] {
        assert_eq!(check(pypi, Format::Pypi, spelling).0, Verdict::Pass, "{spelling}");
    }
    assert_eq!(check(pypi, Format::Pypi, "reqeusts").0, Verdict::WouldBlock);
    assert_eq!(check(pypi, Format::Pypi, "Reqeusts").0, Verdict::WouldBlock, "normalized first");
}

#[test]
fn has_lists_agrees_with_what_ships() {
    for format in Format::ALL {
        assert_eq!(has_lists(format), shipped(format).is_some(), "{format:?}");
    }
    assert!(!has_lists(Format::Maven));
}

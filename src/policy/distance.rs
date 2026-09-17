use std::collections::HashSet;

use crate::db::kinds::Format;

/// The one edit that turns a top name into the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Substitution,
    Transposition,
    Separator,
}

impl Edit {
    pub const fn as_str(self) -> &'static str {
        match self {
            Edit::Substitution => "substitution",
            Edit::Transposition => "transposition",
            Edit::Separator => "dropped separator",
        }
    }
}

/// A name as the lists spell it: lowercase, `_` as `-`, an npm scope kept
/// apart, a Go major suffix stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub scope: Option<String>,
    pub word: String,
}

impl Normalized {
    pub fn full(&self) -> String {
        match &self.scope {
            Some(scope) => format!("@{scope}/{}", self.word),
            None => self.word.clone(),
        }
    }
}

pub fn normalize(format: Format, name: &str) -> Normalized {
    let lower = name.trim().to_ascii_lowercase().replace('_', "-");
    match format {
        Format::Npm => match lower
            .strip_prefix('@')
            .and_then(|rest| rest.split_once('/'))
        {
            Some((scope, word)) => Normalized {
                scope: Some(scope.to_string()),
                word: word.to_string(),
            },
            None => Normalized {
                scope: None,
                word: lower,
            },
        },
        Format::Go => Normalized {
            scope: None,
            word: strip_go_major(&lower).to_string(),
        },
        _ => Normalized {
            scope: None,
            word: lower,
        },
    }
}

/// `github.com/go-redis/redis/v9` and `gopkg.in/yaml.v3` name the module
/// the list entry names.
fn strip_go_major(path: &str) -> &str {
    if let Some((base, suffix)) = path.rsplit_once('/') {
        let major = suffix.strip_prefix('v').and_then(|n| n.parse::<u32>().ok());
        if major.is_some_and(|n| n >= 2) {
            return base;
        }
    }
    if path.starts_with("gopkg.in/") {
        if let Some((base, v)) = path.rsplit_once(".v") {
            if v.parse::<u32>().is_ok() {
                return base;
            }
        }
    }
    path
}

const SEPARATORS: [u8; 3] = [b'-', b'.', b'_'];
const FAMILY_HEAD: usize = 3;

/// A substitution that marks a sibling in a family, not a typo: a digit
/// on either side (`bzip2`/`bzip3`), one separator for another
/// (`is-array`/`is.array`), or a change inside a leading `-` token of up
/// to three characters (`git-*`/`gix-*`, `ndk-sys`/`wdk-sys`).
fn family_marker(c: &[u8], t: &[u8], i: usize) -> bool {
    if c[i].is_ascii_digit() || t[i].is_ascii_digit() {
        return true;
    }
    if SEPARATORS.contains(&c[i]) && SEPARATORS.contains(&t[i]) {
        return true;
    }
    let Some(head) = t.iter().position(|b| *b == b'-') else {
        return false;
    };
    i < head && head <= FAMILY_HEAD
}

/// A single substitution, an adjacent transposition, or one `-`/`.` of
/// `top` missing from `candidate`; equal strings, a letter added or
/// dropped, a separator added, or a family marker (above) are not edits.
pub fn one_edit(candidate: &str, top: &str) -> Option<Edit> {
    let (c, t) = (candidate.as_bytes(), top.as_bytes());
    if c.len() == t.len() {
        let mut differing = [0usize; 2];
        let mut n = 0;
        for i in 0..c.len() {
            if c[i] != t[i] {
                if n == 2 {
                    return None;
                }
                differing[n] = i;
                n += 1;
            }
        }
        let [i, j] = differing;
        return match n {
            1 if family_marker(c, t, i) => None,
            1 => Some(Edit::Substitution),
            2 if j == i + 1 && c[i] == t[j] && c[j] == t[i] => Some(Edit::Transposition),
            _ => None,
        };
    }
    if c.len() + 1 != t.len() {
        return None;
    }
    let i = c.iter().zip(t).position(|(a, b)| a != b).unwrap_or(c.len());
    (matches!(t[i], b'-' | b'.') && t[i + 1..] == c[i..]).then_some(Edit::Separator)
}

/// The names of one list file: one per line, `#` lines skipped.
pub fn names(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_edit_table() {
        let table = [
            ("ab", "ba", Some(Edit::Transposition)),
            ("abc", "abd", Some(Edit::Substitution)),
            ("abc", "ab-c", Some(Edit::Separator)),
            ("abc", "ab.c", Some(Edit::Separator)),
            ("ab-c", "abc", None),
            ("abc", "abcd", None),
            ("abcd", "abc", None),
            ("abc", "xyz", None),
            ("abc", "abxc", None),
            ("acb", "bca", None),
            ("bzip3", "bzip2", None),
            ("soup2", "soup3", None),
            ("sha3-asm", "sha1-asm", None),
            ("es5-shim", "es6-shim", None),
            ("is.array", "is-array", None),
            ("is_array", "is-array", None),
            ("gix-config", "git-config", None),
            ("wdk-sys", "ndk-sys", None),
            ("jl-sys", "js-sys", None),
            ("gitxconfig", "git-config", Some(Edit::Substitution)),
            ("reakt-dom", "react-dom", Some(Edit::Substitution)),
            ("lodask", "lodash", Some(Edit::Substitution)),
        ];
        for (candidate, top, edit) in table {
            assert_eq!(one_edit(candidate, top), edit, "{candidate} vs {top}");
        }
    }

    #[test]
    fn equal_strings_are_not_an_edit() {
        assert_eq!(one_edit("lodash", "lodash"), None);
        assert_eq!(one_edit("", ""), None);
    }

    #[test]
    fn normalize_scopes_underscores_and_go_majors() {
        let scoped = normalize(Format::Npm, "@Babel/Core_Utils");
        assert_eq!(scoped.scope.as_deref(), Some("babel"));
        assert_eq!(scoped.word, "core-utils");
        assert_eq!(scoped.full(), "@babel/core-utils");
        assert_eq!(normalize(Format::Cargo, "serde_json").word, "serde-json");
        for (path, base) in [
            ("github.com/go-redis/redis/v9", "github.com/go-redis/redis"),
            ("gopkg.in/yaml.v3", "gopkg.in/yaml"),
            ("gopkg.in/src-d/go-git.v4", "gopkg.in/src-d/go-git"),
            ("golang.org/x/sys", "golang.org/x/sys"),
            ("example.com/v1", "example.com/v1"),
            ("example.com/vendor", "example.com/vendor"),
        ] {
            assert_eq!(normalize(Format::Go, path).word, base, "{path}");
        }
        assert_eq!(names("# header\nlodash\n\n  react \n").len(), 2);
    }
}

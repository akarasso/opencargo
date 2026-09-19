//! Project names (PEP 508, PEP 503) and distribution filenames (wheel,
//! PEP 625 sdist), recomposed from their canonical parts before any key.

use crate::domain::{compile_pattern, DomainError, FormatRules, Pattern};

use super::version::Pep440;

pub struct PypiRules;

fn invalid(what: &str, value: &str) -> DomainError {
    DomainError::InvalidName(format!("invalid pypi {what}: '{value}'"))
}

/// PEP 508: ASCII letters and digits, with `.`, `_` and `-` inside.
pub fn is_valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && name.len() <= 200
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// PEP 503: lowercase, every run of `-`, `_` and `.` one `-`.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut run = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            run = true;
            continue;
        }
        if run && !out.is_empty() {
            out.push('-');
        }
        run = false;
        out.push(c.to_ascii_lowercase());
    }
    out
}

impl FormatRules for PypiRules {
    /// PEP 503 is already the store's identity: one normalized name, one row,
    /// so nothing is left for `match_key` to coarsen.
    fn ident_key(&self, name: &str) -> String {
        normalize(name)
    }

    fn match_key(&self, name: &str) -> String {
        normalize(name)
    }

    fn canonical_pattern(&self, pattern: &str) -> Result<Pattern, DomainError> {
        compile_pattern(pattern, normalize)
    }

    fn validate(&self, name: &str) -> Result<(), DomainError> {
        if is_valid_name(name) {
            Ok(())
        } else {
            Err(invalid("project name", name))
        }
    }

    fn normalize(&self, name: &str) -> String {
        normalize(name)
    }

    fn reserved(&self) -> &'static [&'static str] {
        &[]
    }

    fn validate_version(&self, version: &str) -> Result<(), DomainError> {
        Pep440::parse(version)
            .map(|_| ())
            .ok_or_else(|| invalid("version", version))
    }

    fn normalize_version(&self, version: &str) -> String {
        Pep440::parse(version).map_or_else(|| version.to_string(), |v| v.canonical())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wheel,
    Sdist,
}

impl Kind {
    /// The `packagetype` PyPI's JSON API names.
    pub const fn packagetype(self) -> &'static str {
        match self {
            Kind::Wheel => "bdist_wheel",
            Kind::Sdist => "sdist",
        }
    }
}

/// A distribution filename taken apart: the project and version it claims,
/// and the name it is served and keyed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filename {
    pub kind: Kind,
    /// PEP 503.
    pub project: String,
    pub version: Pep440,
    /// Recomposed: escaped name, normalized version, lowercase tags.
    pub canonical: String,
}

/// `foo.bar-baz` as a filename component: `foo_bar_baz`.
fn escaped(project: &str) -> String {
    normalize(project).replace('-', "_")
}

const SDIST_SUFFIXES: [&str; 2] = [".tar.gz", ".zip"];

pub fn parse_filename(filename: &str) -> Result<Filename, DomainError> {
    let bad = || invalid("filename", filename);
    if filename.contains(['/', '\\']) || filename.starts_with('.') || filename.len() > 255 {
        return Err(bad());
    }
    if let Some(stem) = filename.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        if !(parts.len() == 5 || parts.len() == 6) {
            return Err(bad());
        }
        let (name, version) = (parts[0], parts[1]);
        let tags = &parts[2..];
        if !is_valid_name(name)
            || tags.iter().any(|t| {
                t.is_empty() || !t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
            })
        {
            return Err(bad());
        }
        let version = Pep440::parse(version).ok_or_else(bad)?;
        let tags: Vec<String> = tags.iter().map(|t| t.to_ascii_lowercase()).collect();
        let canonical = format!(
            "{}-{}-{}.whl",
            escaped(name),
            version.normalized(),
            tags.join("-")
        );
        return Ok(Filename {
            kind: Kind::Wheel,
            project: normalize(name),
            version,
            canonical,
        });
    }
    for suffix in SDIST_SUFFIXES {
        let Some(stem) = filename
            .len()
            .checked_sub(suffix.len())
            .filter(|&at| filename.is_char_boundary(at) && filename[at..].eq_ignore_ascii_case(suffix))
            .map(|at| &filename[..at])
        else {
            continue;
        };
        let (name, version) = stem.rsplit_once('-').ok_or_else(bad)?;
        if !is_valid_name(name) {
            return Err(bad());
        }
        let version = Pep440::parse(version).ok_or_else(bad)?;
        let canonical = format!("{}-{}{suffix}", escaped(name), version.normalized());
        return Ok(Filename {
            kind: Kind::Sdist,
            project: normalize(name),
            version,
            canonical,
        });
    }
    Err(bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::rules::rules_of;
    use crate::domain::Format;

    #[test]
    fn names_that_normalize_alike_are_one_project() {
        let r = rules_of(Format::Pypi).unwrap();
        for spelling in ["Foo_Bar", "foo-bar", "foo.bar", "FOO--bar", "foo._-bar"] {
            assert_eq!(r.admit(spelling).unwrap(), "foo-bar", "{spelling}");
        }
        for bad in ["", "-foo", "foo-", "foo bar", "foo/bar", "../x", "föo"] {
            assert!(r.validate(bad).is_err(), "{bad:?}");
        }
        assert_eq!(r.normalize_version("1.0.0"), r.normalize_version("1.0"));
        assert!(r.validate_version("1.0-x").is_err());
    }

    #[test]
    fn pypi_rules_change_no_other_format() {
        let npm = rules_of(Format::Npm).unwrap();
        assert_ne!(npm.normalize("Foo_Bar"), npm.normalize("foo-bar"));
        assert_ne!(npm.normalize_version("1.0"), npm.normalize_version("1.0.0"));
        let cargo = rules_of(Format::Cargo).unwrap();
        assert_ne!(cargo.normalize("foo_bar"), cargo.normalize("foo-bar"));
    }

    #[test]
    fn filenames_are_recomposed_from_canonical_parts() {
        let wheel = parse_filename("Foo.Bar-1.0-py3-none-ANY.whl").unwrap();
        assert_eq!(wheel.kind, Kind::Wheel);
        assert_eq!(wheel.project, "foo-bar");
        assert_eq!(wheel.canonical, "foo_bar-1.0-py3-none-any.whl");
        let tagged = parse_filename("foo-1.0-1build-cp312-cp312-manylinux_2_17_x86_64.whl").unwrap();
        assert_eq!(tagged.canonical, "foo-1.0-1build-cp312-cp312-manylinux_2_17_x86_64.whl");
        let sdist = parse_filename("foo_bar-01.0.tar.gz").unwrap();
        assert_eq!(sdist.kind, Kind::Sdist);
        assert_eq!(sdist.canonical, "foo_bar-1.0.tar.gz");
        assert_eq!(
            parse_filename("Foo-Bar-1.0.tar.gz").unwrap().canonical,
            parse_filename("foo_bar-1.0.tar.gz").unwrap().canonical,
            "equivalent filenames are one file"
        );
        for bad in [
            "foo-1.0.exe",
            "foo.whl",
            "foo-1.0-py3.whl",
            "../foo-1.0.tar.gz",
            "foo-x.y.tar.gz",
            ".foo-1.0.tar.gz",
            "foo-1.0-py3-none-any/x.whl",
        ] {
            assert!(parse_filename(bad).is_err(), "{bad}");
        }
    }
}

//! PEP 440 versions: parsed once, printed in the normalized spelling, and
//! compared as PEP 440 orders them.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pre {
    Alpha(u64),
    Beta(u64),
    Rc(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pep440 {
    epoch: u64,
    release: Vec<u64>,
    pre: Option<Pre>,
    post: Option<u64>,
    dev: Option<u64>,
    local: Vec<String>,
}

struct Scan<'a> {
    s: &'a [u8],
    at: usize,
}

impl<'a> Scan<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn number(&mut self) -> Option<u64> {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        if start == self.at {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.at]).ok()?.parse().ok()
    }

    fn separator(&mut self) -> bool {
        if matches!(self.peek(), Some(b'-' | b'_' | b'.')) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self, words: &[&'a str]) -> Option<&'a str> {
        let rest = &self.s[self.at..];
        let found = words
            .iter()
            .filter(|w| rest.starts_with(w.as_bytes()))
            .max_by_key(|w| w.len())?;
        self.at += found.len();
        Some(found)
    }

    /// An optional separator, a label, an optional separator and number;
    /// nothing consumed when the label is absent.
    fn labelled(&mut self, words: &[&'a str]) -> Option<(&'a str, Option<u64>)> {
        let mark = self.at;
        self.separator();
        let Some(label) = self.word(words) else {
            self.at = mark;
            return None;
        };
        let before = self.at;
        self.separator();
        let n = self.number();
        if n.is_none() {
            self.at = before;
        }
        Some((label, n))
    }
}

impl Pep440 {
    pub fn parse(raw: &str) -> Option<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        let mut s = Scan {
            s: lower.as_bytes(),
            at: 0,
        };
        if s.peek() == Some(b'v') {
            s.at += 1;
        }
        let mut epoch = 0;
        let first = s.number()?;
        let mut release = vec![first];
        if s.peek() == Some(b'!') {
            s.at += 1;
            epoch = first;
            release = vec![s.number()?];
        }
        while s.peek() == Some(b'.') && s.s.get(s.at + 1).is_some_and(u8::is_ascii_digit) {
            s.at += 1;
            release.push(s.number()?);
        }
        let pre = s
            .labelled(&["alpha", "a", "beta", "b", "preview", "pre", "rc", "c"])
            .map(|(label, n)| {
                let n = n.unwrap_or(0);
                match label {
                    "alpha" | "a" => Pre::Alpha(n),
                    "beta" | "b" => Pre::Beta(n),
                    _ => Pre::Rc(n),
                }
            });
        let post = if s.peek() == Some(b'-') && s.s.get(s.at + 1).is_some_and(u8::is_ascii_digit) {
            s.at += 1;
            s.number()
        } else {
            s.labelled(&["post", "rev", "r"]).map(|(_, n)| n.unwrap_or(0))
        };
        let dev = s.labelled(&["dev"]).map(|(_, n)| n.unwrap_or(0));
        let mut local = Vec::new();
        if s.peek() == Some(b'+') {
            s.at += 1;
            let rest = std::str::from_utf8(&s.s[s.at..]).ok()?;
            for part in rest.split(['.', '-', '_']) {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return None;
                }
                local.push(part.to_string());
            }
            s.at = s.s.len();
        }
        if s.at != s.s.len() {
            return None;
        }
        Some(Self {
            epoch,
            release,
            pre,
            post,
            dev,
            local,
        })
    }

    fn render(&self, release: &[u64]) -> String {
        let mut out = String::new();
        if self.epoch != 0 {
            out.push_str(&format!("{}!", self.epoch));
        }
        let parts: Vec<String> = release.iter().map(u64::to_string).collect();
        out.push_str(&parts.join("."));
        match &self.pre {
            Some(Pre::Alpha(n)) => out.push_str(&format!("a{n}")),
            Some(Pre::Beta(n)) => out.push_str(&format!("b{n}")),
            Some(Pre::Rc(n)) => out.push_str(&format!("rc{n}")),
            None => {}
        }
        if let Some(n) = self.post {
            out.push_str(&format!(".post{n}"));
        }
        if let Some(n) = self.dev {
            out.push_str(&format!(".dev{n}"));
        }
        if !self.local.is_empty() {
            out.push('+');
            out.push_str(&self.local.join("."));
        }
        out
    }

    /// The normalized spelling, as a filename carries it.
    pub fn normalized(&self) -> String {
        self.render(&self.release)
    }

    /// One string per PEP 440 version: trailing zero components dropped, so
    /// `1.0` and `1.0.0` are the same release.
    pub fn canonical(&self) -> String {
        let mut release = self.release.clone();
        while release.len() > 1 && release.last() == Some(&0) {
            release.pop();
        }
        self.render(&release)
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    fn release_cmp(&self, other: &Self) -> Ordering {
        let n = self.release.len().max(other.release.len());
        (0..n)
            .map(|i| {
                let a = self.release.get(i).copied().unwrap_or(0);
                let b = other.release.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }

    /// PEP 440's `_cmpkey`: a dev release sorts before its pre-releases, a
    /// pre-release before the release, a post-release after it.
    fn pre_key(&self) -> (i8, u64) {
        match (&self.pre, self.post, self.dev) {
            (None, None, Some(_)) => (-1, 0),
            (None, _, _) => (3, 0),
            (Some(Pre::Alpha(n)), _, _) => (0, *n),
            (Some(Pre::Beta(n)), _, _) => (1, *n),
            (Some(Pre::Rc(n)), _, _) => (2, *n),
        }
    }

    fn local_cmp(&self, other: &Self) -> Ordering {
        let part = |p: &str| match p.parse::<u64>() {
            Ok(n) => (1, n, String::new()),
            Err(_) => (0, 0, p.to_string()),
        };
        let a: Vec<_> = self.local.iter().map(|p| part(p)).collect();
        let b: Vec<_> = other.local.iter().map(|p| part(p)).collect();
        a.cmp(&b)
    }
}

impl Ord for Pep440 {
    fn cmp(&self, other: &Self) -> Ordering {
        let post = |v: &Self| v.post.map_or((0, 0), |n| (1, n));
        let dev = |v: &Self| v.dev.map_or((1, 0), |n| (0, n));
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.release_cmp(other))
            .then_with(|| self.pre_key().cmp(&other.pre_key()))
            .then_with(|| post(self).cmp(&post(other)))
            .then_with(|| dev(self).cmp(&dev(other)))
            .then_with(|| self.local_cmp(other))
    }
}

impl PartialOrd for Pep440 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// PEP 440 order for two strings; one that does not parse sorts first.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (Pep440::parse(a), Pep440::parse(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &str) -> String {
        Pep440::parse(v).unwrap().normalized()
    }

    #[test]
    fn spellings_normalize_as_pep_440_prints_them() {
        for (raw, want) in [
            ("1.0", "1.0"),
            ("v1.0", "1.0"),
            ("01.02.003", "1.2.3"),
            ("1.0a", "1.0a0"),
            ("1.0-alpha.1", "1.0a1"),
            ("1.0.BETA2", "1.0b2"),
            ("1.0c1", "1.0rc1"),
            ("1.0preview3", "1.0rc3"),
            ("1.0-1", "1.0.post1"),
            ("1.0.rev2", "1.0.post2"),
            ("1.0post", "1.0.post0"),
            ("1.0-dev", "1.0.dev0"),
            ("1!2.0", "1!2.0"),
            ("0!2.0", "2.0"),
            ("1.0+Ubuntu-1", "1.0+ubuntu.1"),
            (" 1.0 ", "1.0"),
        ] {
            assert_eq!(norm(raw), want, "{raw}");
        }
        for bad in ["", "a", "1.0.", "1..0", "1.0+", "1.0+a..b", "1.0 beta", "1/0", "1.0-x"] {
            assert!(Pep440::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn equal_versions_share_one_canonical_string() {
        let c = |v: &str| Pep440::parse(v).unwrap().canonical();
        assert_eq!(c("1.0"), c("1.0.0"));
        assert_eq!(c("1"), c("1.0.0.0"));
        assert_eq!(c("1.0.0a1"), c("1a1"));
        assert_ne!(c("1.0"), c("1.0.1"));
        assert_ne!(c("1.0"), c("1.0+local"));
        assert_eq!(c("0.0"), "0");
    }

    #[test]
    fn versions_order_as_pep_440_says() {
        let ordered = [
            "1.0.dev1", "1.0a1", "1.0a2.dev1", "1.0a2", "1.0b1", "1.0rc1", "1.0", "1.0+local",
            "1.0.post1.dev1", "1.0.post1", "1.1", "2!0.1",
        ];
        for w in ordered.windows(2) {
            assert_eq!(compare(w[0], w[1]), Ordering::Less, "{} < {}", w[0], w[1]);
        }
        assert_eq!(compare("1.0", "1.0.0"), Ordering::Equal);
        assert!(Pep440::parse("1.0rc1").unwrap().is_prerelease());
        assert!(!Pep440::parse("1.0.post1").unwrap().is_prerelease());
    }
}

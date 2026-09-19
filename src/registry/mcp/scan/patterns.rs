//! The eight text patterns. No regex crate: each is a small scanner over
//! the ASCII-lowercased text, whose byte offsets are the original's.

use super::{Confidence, Pattern};

const SINK_WINDOW: usize = 200;
const PARAM_WINDOW: usize = 120;

struct InvisibleChars;
struct ModelDirective;
struct CredentialPath;
struct ConfigPath;
struct CrossTool;
struct ExfilSink;
struct ParamSink;

pub fn all_patterns() -> &'static [&'static dyn Pattern] {
    &[
        &InvisibleChars,
        &ModelDirective,
        &CredentialPath,
        &ConfigPath,
        &CrossTool,
        &ExfilSink,
        &ParamSink,
    ]
}

/// Text the reviewer cannot see and the model reads.
pub fn invisible(c: char) -> bool {
    matches!(c as u32,
        0xE0000..=0xE007F | 0x202A..=0x202E | 0x2066..=0x2069 | 0x200B..=0x200D | 0xFEFF)
}

fn emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF)
}

/// Every occurrence of `needle` in `hay`, both already lowercase.
fn occurrences(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    hay.match_indices(needle).map(|(i, m)| (i, i + m.len())).collect()
}

fn lower(text: &str) -> String {
    text.to_ascii_lowercase()
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `needle` not glued to a word on either side.
fn bounded(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let bytes = hay.as_bytes();
    occurrences(hay, needle)
        .into_iter()
        .filter(|&(s, e)| {
            let before = s.checked_sub(1).map(|i| bytes[i]);
            let after = bytes.get(e).copied();
            let starts = needle.as_bytes().first().is_some_and(|b| !word_byte(*b));
            (starts || !before.is_some_and(word_byte)) && !after.is_some_and(word_byte)
        })
        .collect()
}

impl Pattern for InvisibleChars {
    fn name(&self) -> &'static str {
        "invisible_chars"
    }

    fn confidence(&self) -> Confidence {
        Confidence::High
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut out = Vec::new();
        for (i, &(at, c)) in chars.iter().enumerate() {
            if !invisible(c) {
                continue;
            }
            if c == '\u{200D}' {
                let before = chars[..i].iter().rev().map(|(_, c)| *c).find(|c| *c != '\u{FE0F}');
                let after = chars.get(i + 1).map(|(_, c)| *c);
                if before.is_some_and(emoji) && after.is_some_and(emoji) {
                    continue;
                }
            }
            out.push((at, at + c.len_utf8()));
        }
        out
    }
}

const DIRECTIVE_TAGS: &[&str] = &["<important", "</important", "<system", "</system", "<secret", "<instructions", "<hidden"];
const DIRECTIVE_PHRASES: &[&str] = &[
    "ignore previous instructions",
    "ignore all previous instructions",
    "ignore the previous instructions",
    "disregard previous instructions",
    "do not tell the user",
    "don't tell the user",
    "without telling the user",
    "do not mention",
    "don't mention",
    "before using this tool you must",
    "before using this tool, you must",
    "mask this",
];

impl Pattern for ModelDirective {
    fn name(&self) -> &'static str {
        "model_directive"
    }

    fn confidence(&self) -> Confidence {
        Confidence::High
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        let mut out: Vec<(usize, usize)> = DIRECTIVE_TAGS
            .iter()
            .flat_map(|tag| occurrences(&hay, tag))
            .filter(|&(_, e)| matches!(hay.as_bytes().get(e), Some(b'>' | b' ' | b'_' | b'-')))
            .collect();
        out.extend(DIRECTIVE_PHRASES.iter().flat_map(|p| occurrences(&hay, p)));
        out.sort();
        out
    }
}

const CREDENTIAL_PATHS: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", ".aws/credentials", ".netrc"];

impl Pattern for CredentialPath {
    fn name(&self) -> &'static str {
        "credential_path"
    }

    fn confidence(&self) -> Confidence {
        Confidence::High
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        CREDENTIAL_PATHS.iter().flat_map(|p| bounded(&hay, p)).collect()
    }
}

const CONFIG_PATHS: &[&str] = &[
    ".env",
    ".mcp.json",
    "settings.json",
    ".claude.json",
    ".cursor/mcp.json",
    ".docker/config.json",
];

impl Pattern for ConfigPath {
    fn name(&self) -> &'static str {
        "config_path"
    }

    fn confidence(&self) -> Confidence {
        Confidence::Medium
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        let mut out: Vec<(usize, usize)> = CONFIG_PATHS
            .iter()
            .flat_map(|p| bounded(&hay, p))
            .collect();
        out.sort();
        out.dedup_by(|b, a| b.0 < a.1);
        out
    }
}

impl Pattern for CrossTool {
    fn name(&self) -> &'static str {
        "cross_tool"
    }

    fn confidence(&self) -> Confidence {
        Confidence::Medium
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        let mut out = bounded(&hay, "tools/call");
        out.extend(
            occurrences(&hay, "mcp_")
                .into_iter()
                .filter(|&(s, _)| s == 0 || !word_byte(hay.as_bytes()[s - 1])),
        );
        for (s, _) in occurrences(&hay, "the ") {
            let rest = &hay[s + 4..];
            let word_len = rest.bytes().take_while(|b| word_byte(*b) || *b == b'-' || *b == b'`').count();
            if word_len == 0 {
                continue;
            }
            let tail = &rest[word_len..];
            for verb in [" tool must", " tool should", " tool always"] {
                if tail.starts_with(verb) {
                    out.push((s, s + 4 + word_len + verb.len()));
                }
            }
        }
        out.sort();
        out
    }
}

/// `http(s)://…` URLs and `x@y.z` addresses, as spans.
fn sinks(hay: &str) -> Vec<(usize, usize)> {
    let bytes = hay.as_bytes();
    let stop = |b: u8| b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'<' | b'>' | b')' | b'`');
    let mut out: Vec<(usize, usize)> = ["http://", "https://"]
        .iter()
        .flat_map(|scheme| occurrences(hay, scheme))
        .map(|(s, _)| (s, s + bytes[s..].iter().take_while(|b| !stop(**b)).count()))
        .collect();
    for (at, _) in hay.match_indices('@') {
        let local = bytes[..at].iter().rev().take_while(|b| word_byte(**b) || matches!(b, b'.' | b'-' | b'+')).count();
        let domain = bytes[at + 1..].iter().take_while(|b| word_byte(**b) || matches!(b, b'.' | b'-')).count();
        let domain_text = &hay[at + 1..at + 1 + domain];
        if local > 0 && domain_text.contains('.') && !domain_text.ends_with('.') {
            out.push((at - local, at + 1 + domain));
        }
    }
    out
}

const SINK_VERBS: &[&str] = &["send", "post", "upload", "forward", "report to"];

impl Pattern for ExfilSink {
    fn name(&self) -> &'static str {
        "exfil_sink"
    }

    fn confidence(&self) -> Confidence {
        Confidence::Medium
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        let verbs: Vec<usize> = SINK_VERBS
            .iter()
            .flat_map(|v| occurrences(&hay, v))
            .map(|(s, _)| s)
            .collect();
        sinks(&hay)
            .into_iter()
            .filter(|&(s, e)| verbs.iter().any(|&v| v.abs_diff(s) <= SINK_WINDOW || v.abs_diff(e) <= SINK_WINDOW))
            .collect()
    }
}

const PARAM_NOUNS: &[&str] = &[" argument", " parameter", " param", " field of the"];

/// `hay[from..from + len]`, cut short at a char boundary.
fn window(hay: &str, from: usize, len: usize) -> &str {
    let mut end = (from + len).min(hay.len());
    while !hay.is_char_boundary(end) {
        end -= 1;
    }
    &hay[from..end]
}

fn followed_by_param(hay: &str, from: usize) -> Option<usize> {
    let w = window(hay, from, PARAM_WINDOW);
    PARAM_NOUNS
        .iter()
        .filter_map(|n| w.find(n).map(|i| from + i + n.len()))
        .min()
}

impl Pattern for ParamSink {
    fn name(&self) -> &'static str {
        "param_sink"
    }

    fn confidence(&self) -> Confidence {
        Confidence::Medium
    }

    fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let hay = lower(text);
        let mut out = Vec::new();
        for (s, e) in occurrences(&hay, "contents of") {
            if let Some(end) = followed_by_param(&hay, e) {
                out.push((s, end));
            }
        }
        for (s, e) in bounded(&hay, "pass").into_iter().chain(bounded(&hay, "passes")) {
            if let Some(i) = window(&hay, e, 80).find(" as ") {
                if let Some(end) = followed_by_param(&hay, e + i) {
                    out.push((s, end));
                }
            }
        }
        for (s, e) in bounded(&hay, "set") {
            if let Some(i) = window(&hay, e, PARAM_WINDOW).find(" to the contents of") {
                out.push((s, e + i + " to the contents of".len()));
            }
        }
        out.sort();
        out.dedup_by(|b, a| b.0 < a.1);
        out
    }
}

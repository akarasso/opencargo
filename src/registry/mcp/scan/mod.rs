//! The injection scan: hand-written patterns over every model-facing text,
//! each finding carrying the span and an excerpt an admin judges from.
//!
//! A scan is never suppressed: it runs on the member that holds the text,
//! and suppression belongs to whichever repository addresses it.

mod patterns;

use serde_json::Value;

use super::schema::ServerDetail;
use super::surface::{header_params, schema_texts, Tool};

pub use patterns::all_patterns;

const EXCERPT_CONTEXT: usize = 40;
const LONG_SCHEMA_TEXT: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Confidence {
    Medium,
    High,
}

impl Confidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "medium" => Some(Confidence::Medium),
            "high" => Some(Confidence::High),
            _ => None,
        }
    }
}

/// Where a text sat.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    ServerDescription,
    ServerTitle,
    ToolDescription,
    ToolTitle,
    Annotation,
    SkillDescription,
    SkillAllowedTools,
    SkillBody,
    /// The JSON pointer of a string inside `inputSchema` or `outputSchema`.
    SchemaText(String),
}

impl Field {
    pub fn as_string(&self) -> String {
        match self {
            Field::ServerDescription => "server.description".into(),
            Field::ServerTitle => "server.title".into(),
            Field::ToolDescription => "tool.description".into(),
            Field::ToolTitle => "tool.title".into(),
            Field::Annotation => "tool.annotations.title".into(),
            Field::SkillDescription => "skill.description".into(),
            Field::SkillAllowedTools => "skill.allowed-tools".into(),
            Field::SkillBody => "skill.body".into(),
            Field::SchemaText(pointer) => format!("schema:{pointer}"),
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "server.description" => Field::ServerDescription,
            "server.title" => Field::ServerTitle,
            "tool.description" => Field::ToolDescription,
            "tool.title" => Field::ToolTitle,
            "tool.annotations.title" => Field::Annotation,
            "skill.description" => Field::SkillDescription,
            "skill.allowed-tools" => Field::SkillAllowedTools,
            "skill.body" => Field::SkillBody,
            other => Field::SchemaText(other.strip_prefix("schema:").unwrap_or(other).to_string()),
        }
    }

    /// A skill's frontmatter: the short field the patterns are calibrated
    /// for, and the only one a finding may un-ship a skill from.
    pub fn is_skill_frontmatter(&self) -> bool {
        matches!(self, Field::SkillDescription | Field::SkillAllowedTools)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub pattern: &'static str,
    pub confidence: Confidence,
    /// `Some(promoter)`: `high` by promotion, not natively.
    pub promoted_by: Option<&'static str>,
    pub field: Field,
    pub tool: Option<String>,
    pub span: (usize, usize),
    pub excerpt: String,
}

impl Finding {
    pub fn native_high(&self) -> bool {
        self.confidence == Confidence::High && self.promoted_by.is_none()
    }
}

/// A text scanner: pure, synchronous, byte spans into the text it was given.
pub trait Pattern: Send + Sync {
    fn name(&self) -> &'static str;
    fn confidence(&self) -> Confidence;
    fn find(&self, text: &str) -> Vec<(usize, usize)>;
}

/// One text to scan: where it sat, which tool it belongs to, and the text.
pub type Text<'a> = (Field, Option<&'a str>, &'a str);

/// An `x-mcp-header` a conforming client drops the tool for: (tool, pointer, why).
pub type InvalidHeader = (String, String, &'static str);

/// Every pattern over every text, `config_path` promoted where a directive
/// or a parameter sink shares its text, plus the invalid annotations.
pub fn scan(texts: &[Text<'_>], invalid_headers: &[InvalidHeader]) -> Vec<Finding> {
    let mut out = Vec::new();
    for (field, tool, text) in texts {
        let mut found = scan_text(field, *tool, text);
        if let Field::SchemaText(pointer) = field {
            let triggers: Vec<&'static str> = found.iter().map(|f| f.pattern).collect();
            found.extend(schema_payload(field, *tool, text, pointer, &triggers));
        }
        out.extend(found);
    }
    for (tool, pointer, why) in invalid_headers {
        out.push(Finding {
            pattern: "invalid_x_mcp_header",
            confidence: Confidence::High,
            promoted_by: None,
            field: Field::SchemaText(format!("/inputSchema{pointer}")),
            tool: Some(tool.clone()),
            span: (0, 0),
            excerpt: (*why).to_string(),
        });
    }
    out
}

fn scan_text(field: &Field, tool: Option<&str>, text: &str) -> Vec<Finding> {
    let mut found: Vec<Finding> = Vec::new();
    for pattern in all_patterns() {
        for span in pattern.find(text) {
            found.push(Finding {
                pattern: pattern.name(),
                confidence: pattern.confidence(),
                promoted_by: None,
                field: field.clone(),
                tool: tool.map(str::to_string),
                span,
                excerpt: excerpt(text, span),
            });
        }
    }
    let promoter = ["model_directive", "param_sink"]
        .into_iter()
        .find(|p| found.iter().any(|f| f.pattern == *p));
    if let Some(promoter) = promoter {
        for f in found.iter_mut().filter(|f| f.pattern == "config_path") {
            f.confidence = Confidence::High;
            f.promoted_by = Some(promoter);
        }
    }
    found
}

/// A payload anywhere in a schema, or a schema string long enough to hide
/// one. Buried more than one `properties` level down it is hidden from the
/// reviewer as well, and `high` — unless all that fired there is a sink or
/// a cross reference, which honest field docs name ("post URL") all the time.
fn schema_payload(
    field: &Field,
    tool: Option<&str>,
    text: &str,
    pointer: &str,
    triggers: &[&'static str],
) -> Option<Finding> {
    if triggers.is_empty() && text.len() <= LONG_SCHEMA_TEXT {
        return None;
    }
    let deep = pointer.matches("/properties/").count() > 1;
    let instruction = triggers.iter().any(|p| !matches!(*p, "exfil_sink" | "cross_tool"));
    Some(Finding {
        pattern: "schema_payload",
        confidence: if deep && instruction { Confidence::High } else { Confidence::Medium },
        promoted_by: None,
        field: field.clone(),
        tool: tool.map(str::to_string),
        span: (0, text.len()),
        excerpt: excerpt(text, (0, text.len().min(EXCERPT_CONTEXT))),
    })
}

/// The span and forty bytes either side, each end walked outward to a char
/// boundary, every non-printable escaped.
pub fn excerpt(text: &str, (start, end): (usize, usize)) -> String {
    let mut from = start.saturating_sub(EXCERPT_CONTEXT).min(text.len());
    while !text.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = end.saturating_add(EXCERPT_CONTEXT).min(text.len());
    while !text.is_char_boundary(to) {
        to += 1;
    }
    escape(&text[from..to])
}

/// Every character a reviewer could not see, spelled `\u{xxxx}`.
pub fn escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || patterns::invisible(c) {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A record's model-facing prose.
pub fn texts_of_server(d: &ServerDetail) -> Vec<Text<'_>> {
    let mut out = vec![(Field::ServerDescription, None, d.description.as_str())];
    if let Some(title) = &d.title {
        out.push((Field::ServerTitle, None, title.as_str()));
    }
    out
}

/// Every model-facing string of every tool, schemas walked to any depth.
pub fn texts_of_tools(tools: &[Tool]) -> Vec<Text<'_>> {
    let mut out = Vec::new();
    for t in tools {
        let name = Some(t.name.as_str());
        if let Some(d) = &t.description {
            out.push((Field::ToolDescription, name, d.as_str()));
        }
        if let Some(title) = &t.title {
            out.push((Field::ToolTitle, name, title.as_str()));
        }
        if let Some(Value::String(title)) = t.annotations.get("title") {
            out.push((Field::Annotation, name, title.as_str()));
        }
        for (pointer, text) in schema_texts(&t.input_schema, "/inputSchema") {
            out.push((Field::SchemaText(pointer), name, text));
        }
        if let Some(output) = &t.output_schema {
            for (pointer, text) in schema_texts(output, "/outputSchema") {
                out.push((Field::SchemaText(pointer), name, text));
            }
        }
    }
    out
}

/// The annotations a conforming client drops a tool for.
pub fn invalid_headers_of(tools: &[Tool]) -> Vec<InvalidHeader> {
    tools
        .iter()
        .flat_map(|t| {
            header_params(t)
                .invalid
                .into_iter()
                .map(move |(pointer, why)| (t.name.clone(), pointer, why))
        })
        .collect()
}

/// A tool list's findings: its texts and its annotations.
pub fn scan_tools(tools: &[Tool]) -> Vec<Finding> {
    scan(&texts_of_tools(tools), &invalid_headers_of(tools))
}

#[cfg(test)]
mod tests;

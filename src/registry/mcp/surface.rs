//! What is fingerprinted: the declared permission set of a record and the
//! tool list a server answered, each canonicalised and hashed on its own.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::schema::{Package, RemoteTransport, ServerDetail};
use crate::domain::Fingerprint;

const INPUT_KEYS: &[&str] = &[
    "type", "name", "valueHint", "isRepeated", "format", "isRequired", "isSecret", "value", "default", "choices",
];
const SECRET: &str = "<secret>";
const MAX_SCHEMA_NODES: usize = 4000;
const MAX_SCHEMA_DEPTH: usize = 32;

/// One tool as `tools/list` answers it, every field of the revision kept.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub icons: Option<Value>,
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    #[serde(default = "empty_object")]
    pub annotations: Value,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// A surface's hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hashes {
    pub permissions_sha256: String,
    pub tools_sha256: Option<String>,
    pub combined_sha256: String,
}

impl Hashes {
    pub fn of(permissions: &Value, tools: Option<&[Tool]>) -> Self {
        let permissions_sha256 = canonical_sha256(permissions);
        let tools_sha256 = tools.map(|t| canonical_sha256(&tool_surface(t)));
        let combined_sha256 = combine(&permissions_sha256, tools_sha256.as_deref());
        Self {
            permissions_sha256,
            tools_sha256,
            combined_sha256,
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            permissions: self.permissions_sha256.clone(),
            tools: self.tools_sha256.clone(),
        }
    }
}

/// The digest of the pair, derived from the halves and never the reverse.
pub fn combine(permissions: &str, tools: Option<&str>) -> String {
    hex(&Sha256::digest(format!("{permissions}:{}", tools.unwrap_or("-")).as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Object keys sorted by code point, arrays in order, no Unicode
/// normalisation: a homoglyph is a drift, never folded away.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn canonical_sha256(v: &Value) -> String {
    hex(&Sha256::digest(canonical_json(v).as_bytes()))
}

/// Every input field that decides a command line or a URL, recursively,
/// with a secret's value and default redacted and its flag kept.
fn input(v: &Value) -> Value {
    let Some(obj) = v.as_object() else {
        return v.clone();
    };
    let secret = obj.get("isSecret").and_then(Value::as_bool).unwrap_or(false);
    let mut out = Map::new();
    for key in INPUT_KEYS {
        if let Some(value) = obj.get(*key) {
            let value = if secret && matches!(*key, "value" | "default") {
                Value::String(SECRET.to_string())
            } else {
                value.clone()
            };
            out.insert((*key).to_string(), value);
        }
    }
    out.insert("isRequired".into(), obj.get("isRequired").cloned().unwrap_or(Value::Bool(false)));
    out.insert("isSecret".into(), Value::Bool(secret));
    out.insert("variables".into(), variables(obj.get("variables").and_then(Value::as_object)));
    Value::Object(out)
}

fn variables(map: Option<&Map<String, Value>>) -> Value {
    Value::Object(
        map.into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), input(v)))
            .collect(),
    )
}

fn inputs(list: &[Value]) -> Value {
    Value::Array(list.iter().map(input).collect())
}

fn package_surface(p: &Package) -> Value {
    serde_json::json!({
        "registryType": p.registry_type,
        "identifier": p.identifier,
        "version": p.version,
        "registryBaseUrl": p.registry_base_url,
        "fileSha256": p.file_sha256,
        "runtimeHint": p.runtime_hint,
        "transport": {"type": p.transport.kind, "url": p.transport.url, "headers": inputs(&p.transport.headers)},
        "runtimeArguments": inputs(&p.runtime_arguments),
        "packageArguments": inputs(&p.package_arguments),
        "environmentVariables": inputs(&p.environment_variables),
    })
}

fn remote_surface(r: &RemoteTransport) -> Value {
    serde_json::json!({
        "type": r.kind,
        "url": r.url,
        "headers": inputs(&r.headers),
        "variables": variables(Some(&r.variables)),
    })
}

/// Built from the parsed record, never from its bytes, so field order and
/// prose (`description`, `title`, `websiteUrl`) never move the hash.
pub fn permission_surface(d: &ServerDetail) -> Value {
    serde_json::json!({
        "packages": d.packages.iter().map(package_surface).collect::<Vec<_>>(),
        "remotes": d.remotes.iter().map(remote_surface).collect::<Vec<_>>(),
        "iconSrcs": d.icons.iter().map(|i| i.src.clone()).collect::<Vec<_>>(),
    })
}

/// Tools sorted by name, every field of the revision included.
pub fn tool_surface(tools: &[Tool]) -> Value {
    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    Value::Array(
        sorted
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name, "title": t.title, "description": t.description, "icons": t.icons,
                    "inputSchema": t.input_schema, "outputSchema": t.output_schema,
                    "annotations": t.annotations, "_meta": t.meta,
                })
            })
            .collect(),
    )
}

/// A JSON pointer segment.
pub fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// Every `description` and `title` string anywhere in a schema, by JSON
/// pointer, depth-first over literal nodes only (a `$ref` is never
/// followed), bounded in nodes and depth.
pub fn schema_texts<'a>(schema: &'a Value, root: &str) -> Vec<(String, &'a str)> {
    let mut out = Vec::new();
    let mut nodes = 0;
    walk_texts(schema, root.to_string(), 0, &mut nodes, &mut out);
    out
}

fn walk_texts<'a>(v: &'a Value, at: String, depth: usize, nodes: &mut usize, out: &mut Vec<(String, &'a str)>) {
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH {
        return;
    }
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let here = format!("{at}/{}", escape_pointer(key));
                match (key.as_str(), child) {
                    ("description" | "title", Value::String(text)) => out.push((here, text.as_str())),
                    _ => walk_texts(child, here, depth + 1, nodes, out),
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk_texts(child, format!("{at}/{i}"), depth + 1, nodes, out);
            }
        }
        _ => {}
    }
}

/// A parameter mirrored into an `Mcp-Param-{name}` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderParam {
    pub pointer: String,
    pub header: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderParams {
    pub valid: Vec<HeaderParam>,
    /// (pointer, the constraint it breaks)
    pub invalid: Vec<(String, &'static str)>,
}

fn http_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Every `x-mcp-header`: legal only on a chain of `properties` keys from
/// the schema root, on a primitive that is not a `number`, with a token
/// name no other annotation of the tool shares case-insensitively.
pub fn header_params(t: &Tool) -> HeaderParams {
    let mut out = HeaderParams::default();
    let mut nodes = 0;
    walk_headers(&t.input_schema, String::new(), true, 0, &mut nodes, &mut out);
    let lower = |p: &HeaderParam| p.header.to_ascii_lowercase();
    let (valid, duplicated): (Vec<HeaderParam>, Vec<HeaderParam>) = out
        .valid
        .iter()
        .cloned()
        .partition(|p| out.valid.iter().filter(|q| lower(q) == lower(p)).count() == 1);
    out.invalid
        .extend(duplicated.into_iter().map(|p| (p.pointer, "duplicate header name")));
    out.valid = valid;
    out
}

fn walk_headers(v: &Value, at: String, reachable: bool, depth: usize, nodes: &mut usize, out: &mut HeaderParams) {
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES || depth > MAX_SCHEMA_DEPTH {
        return;
    }
    match v {
        Value::Object(map) => {
            if let Some(annotation) = map.get("x-mcp-header") {
                classify_header(map, annotation, &at, reachable && !at.is_empty(), out);
            }
            for (key, child) in map {
                let here = format!("{at}/{}", escape_pointer(key));
                if key == "properties" {
                    if let Value::Object(props) = child {
                        for (name, schema) in props {
                            let prop = format!("{here}/{}", escape_pointer(name));
                            walk_headers(schema, prop, reachable, depth + 2, nodes, out);
                        }
                    }
                } else if key != "x-mcp-header" {
                    walk_headers(child, here, false, depth + 1, nodes, out);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk_headers(child, format!("{at}/{i}"), false, depth + 1, nodes, out);
            }
        }
        _ => {}
    }
}

fn classify_header(map: &Map<String, Value>, annotation: &Value, at: &str, reachable: bool, out: &mut HeaderParams) {
    let pointer = if at.is_empty() { "/".to_string() } else { at.to_string() };
    let why = if !reachable {
        Some("not on a chain of properties keys from the schema root")
    } else {
        match annotation.as_str() {
            None | Some("") => Some("empty or not a string"),
            Some(name) if !http_token(name) => Some("not an HTTP token"),
            Some(_) => match map.get("type").and_then(Value::as_str) {
                Some("string" | "integer" | "boolean") => None,
                Some("number") => Some("a number parameter"),
                _ => Some("not a primitive parameter"),
            },
        }
    };
    match why {
        Some(why) => out.invalid.push((pointer, why)),
        None => out.valid.push(HeaderParam {
            pointer,
            header: format!("Mcp-Param-{}", annotation.as_str().unwrap_or_default()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::mcp::schema::parse;
    use serde_json::json;

    fn record(extra: Value) -> ServerDetail {
        let mut base = json!({"name": "io.github.acme/x", "description": "d", "version": "1.0.0"});
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        parse(&base).unwrap().0
    }

    fn perms(extra: Value) -> String {
        canonical_sha256(&permission_surface(&record(extra)))
    }

    fn tool(v: Value) -> Tool {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn canonical_json_is_key_order_only_and_not_nfc() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"d":[2,1],"c":"x"}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"c":"x","d":[2,1]},"b":1}"#).unwrap();
        assert_eq!(canonical_json(&a), r#"{"a":{"c":"x","d":[2,1]},"b":1}"#);
        assert_eq!(canonical_sha256(&a), canonical_sha256(&b));
        let composed = json!({"n": "caf\u{e9}"});
        let decomposed = json!({"n": "cafe\u{301}"});
        assert_ne!(canonical_sha256(&composed), canonical_sha256(&decomposed));
    }

    #[test]
    fn prose_does_not_move_the_permission_hash() {
        let a = perms(json!({"description": "one", "websiteUrl": "https://a"}));
        let b = perms(json!({"description": "two", "title": "t"}));
        assert_eq!(a, b);
    }

    #[test]
    fn a_default_flip_moves_the_permission_hash() {
        let with = |default: &str| {
            perms(json!({"packages": [{"registryType": "npm", "identifier": "x", "transport": {"type": "stdio"},
                "packageArguments": [{"type": "named", "name": "--mode", "default": default}]}]}))
        };
        assert_ne!(with("--readonly"), with("--allow-write"));
    }

    #[test]
    fn a_remote_variable_default_flip_moves_the_permission_hash() {
        let with = |base: &str| {
            perms(json!({"remotes": [{"type": "streamable-http", "url": "{baseUrl}/mcp",
                "variables": {"baseUrl": {"default": base}}}]}))
        };
        assert_ne!(with("https://mcp.acme.com"), with("https://attacker.test"));
    }

    #[test]
    fn secret_values_are_redacted_but_the_secret_flag_is_hashed() {
        let env = |secret: bool, value: &str| {
            json!({"packages": [{"registryType": "npm", "identifier": "x", "transport": {"type": "stdio"},
                "environmentVariables": [{"name": "TOKEN", "isSecret": secret, "value": value}]}]})
        };
        let surface = permission_surface(&record(env(true, "hunter2")));
        assert!(!canonical_json(&surface).contains("hunter2"));
        assert_eq!(perms(env(true, "hunter2")), perms(env(true, "other")));
        assert_ne!(perms(env(true, "hunter2")), perms(env(false, "hunter2")));
    }

    #[test]
    fn a_server_icon_src_change_moves_the_permission_hash() {
        let a = perms(json!({"icons": [{"src": "https://a/icon.svg"}]}));
        let b = perms(json!({"icons": [{"src": "https://b/icon.svg"}]}));
        assert_ne!(a, b);
    }

    #[test]
    fn tool_icon_src_change_moves_the_tools_hash() {
        let t = |src: &str| vec![tool(json!({"name": "search", "icons": [{"src": src}]}))];
        let a = Hashes::of(&json!({}), Some(&t("https://a/i.svg")));
        let b = Hashes::of(&json!({}), Some(&t("https://b/i.svg")));
        assert_eq!(a.permissions_sha256, b.permissions_sha256);
        assert_ne!(a.tools_sha256, b.tools_sha256);
        assert_ne!(a.combined_sha256, b.combined_sha256);
        let reordered = vec![tool(json!({"name": "b"})), tool(json!({"name": "a"}))];
        let sorted = vec![tool(json!({"name": "a"})), tool(json!({"name": "b"}))];
        assert_eq!(Hashes::of(&json!({}), Some(&reordered)), Hashes::of(&json!({}), Some(&sorted)));
        assert_ne!(Hashes::of(&json!({}), None).combined_sha256, Hashes::of(&json!({}), Some(&[])).combined_sha256);
    }

    #[test]
    fn schema_texts_walk_every_depth_and_both_schemas() {
        let schema = json!({"type": "object", "description": "root", "properties": {
            "filter": {"type": "object", "properties": {"sidenote": {"type": "string", "description": "deep"}}},
            "list": {"type": "array", "items": {"oneOf": [{"title": "branch"}]}},
            "description": {"type": "string", "description": "a property named description"}},
            "$defs": {"x": {"description": "def"}}});
        let texts = schema_texts(&schema, "/inputSchema");
        let find = |p: &str| texts.iter().find(|(ptr, _)| ptr == p).map(|(_, t)| *t);
        assert_eq!(find("/inputSchema/description"), Some("root"));
        assert_eq!(find("/inputSchema/properties/filter/properties/sidenote/description"), Some("deep"));
        assert_eq!(find("/inputSchema/properties/list/items/oneOf/0/title"), Some("branch"));
        assert_eq!(find("/inputSchema/$defs/x/description"), Some("def"));
        assert_eq!(find("/inputSchema/properties/description/description"), Some("a property named description"));
    }

    #[test]
    fn a_schema_deeper_than_the_node_cap_is_truncated_not_hung() {
        let mut deep = json!({"description": "bottom"});
        for _ in 0..100 {
            deep = json!({"properties": {"x": deep}});
        }
        let texts = schema_texts(&deep, "");
        assert!(texts.is_empty(), "the payload lies past the depth bound");
        let wide = Value::Array((0..10_000).map(|i| json!({"description": format!("{i}")})).collect());
        assert!(schema_texts(&wide, "").len() < MAX_SCHEMA_NODES);
    }

    #[test]
    fn header_params_extracts_every_x_mcp_header() {
        let t = tool(json!({"name": "q", "inputSchema": {"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "filter": {"type": "object", "properties": {"tenant": {"type": "string", "x-mcp-header": "Tenant"}}},
            "tags": {"type": "array", "items": {"type": "string", "x-mcp-header": "Tag"}},
            "ratio": {"type": "number", "x-mcp-header": "Ratio"},
            "dup": {"type": "string", "x-mcp-header": "region"},
            "bad": {"type": "string", "x-mcp-header": "a b"},
            "any": {"anyOf": [{"type": "string", "x-mcp-header": "Any"}]}}}}));
        let hp = header_params(&t);
        let valid: Vec<(&str, &str)> = hp.valid.iter().map(|p| (p.pointer.as_str(), p.header.as_str())).collect();
        assert_eq!(valid, vec![("/properties/filter/properties/tenant", "Mcp-Param-Tenant")]);
        let invalid: Vec<&str> = hp.invalid.iter().map(|(p, _)| p.as_str()).collect();
        for p in ["/properties/tags/items", "/properties/ratio", "/properties/bad", "/properties/any/anyOf/0",
                  "/properties/region", "/properties/dup"] {
            assert!(invalid.contains(&p), "{p} in {invalid:?}");
        }
        assert_eq!(hp.valid.len() + hp.invalid.len(), 7);
        assert!(hp.invalid.iter().any(|(_, why)| *why == "duplicate header name"));
    }
}

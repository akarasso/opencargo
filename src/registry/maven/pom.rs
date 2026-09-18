//! What the server keeps of a POM on the `versions` row: coordinates, a
//! description and the declared dependencies, as JSON. Nothing is resolved:
//! a property or a parent's dependency management is not followed.

use serde_json::{json, Map, Value};

fn text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

/// The JSON a `versions` row carries for a Maven version; an unreadable POM
/// still yields the coordinates it was deposited under.
pub fn metadata_json(pom: &[u8], group: &str, artifact: &str, version: &str) -> String {
    let fallback = || json!({"groupId": group, "artifactId": artifact, "version": version});
    let Ok(text_body) = std::str::from_utf8(pom) else {
        return fallback().to_string();
    };
    let Ok(doc) = roxmltree::Document::parse(text_body) else {
        return fallback().to_string();
    };
    let project = doc.root_element();
    let mut dependencies = Map::new();
    if let Some(deps) = child(project, "dependencies") {
        for dep in deps
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "dependency")
        {
            if text(dep, "scope").as_deref() == Some("test") {
                continue;
            }
            let (Some(g), Some(a)) = (text(dep, "groupId"), text(dep, "artifactId")) else {
                continue;
            };
            let v = text(dep, "version").unwrap_or_else(|| "*".to_string());
            dependencies.insert(format!("{g}:{a}"), Value::String(v));
        }
    }
    let mut meta = fallback();
    if let Some(description) = text(project, "description").or_else(|| text(project, "name")) {
        meta["description"] = Value::String(description);
    }
    meta["packaging"] = Value::String(text(project, "packaging").unwrap_or_else(|| "jar".to_string()));
    meta["dependencies"] = Value::Object(dependencies);
    meta.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_are_kept_as_a_name_to_requirement_map() {
        let pom = br#"<project xmlns="http://maven.apache.org/POM/4.0.0">
            <groupId>org.example</groupId><artifactId>lib</artifactId><version>1.0</version>
            <description>A lib</description>
            <dependencies>
              <dependency><groupId>com.x</groupId><artifactId>y</artifactId><version>2.1</version></dependency>
              <dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>4</version><scope>test</scope></dependency>
            </dependencies>
          </project>"#;
        let meta: Value = serde_json::from_str(&metadata_json(pom, "org.example", "lib", "1.0")).unwrap();
        assert_eq!(meta["dependencies"], json!({"com.x:y": "2.1"}));
        assert_eq!(meta["description"], "A lib");
        assert_eq!(meta["packaging"], "jar");
        let broken: Value = serde_json::from_str(&metadata_json(b"<", "g", "a", "1")).unwrap();
        assert_eq!(broken["artifactId"], "a");
    }
}

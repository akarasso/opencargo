//! The `.nuspec`: the one source of a package's metadata. Read from the
//! root of the `.nupkg` under a size cap, as XML without a DTD: a
//! `<!DOCTYPE>` is refused and an entity other than the five predefined
//! ones is an error, so nothing expands.

use std::io::Read as _;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

pub const MAX_NUSPEC_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NuspecError {
    #[error("invalid nupkg: {0}")]
    Package(String),
    #[error("invalid nuspec: {0}")]
    Xml(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyGroup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_framework: Option<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageType {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// What the registration, the search and the policy read. Every field is
/// as the nuspec spells it; keys are derived elsewhere.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nuspec {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub authors: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub project_url: Option<String>,
    #[serde(default)]
    pub license_url: Option<String>,
    #[serde(default)]
    pub license_expression: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub release_notes: Option<String>,
    #[serde(default)]
    pub copyright: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub min_client_version: Option<String>,
    #[serde(default)]
    pub require_license_acceptance: bool,
    #[serde(default)]
    pub dependency_groups: Vec<DependencyGroup>,
    #[serde(default)]
    pub package_types: Vec<PackageType>,
}

fn xml(e: impl std::fmt::Display) -> NuspecError {
    NuspecError::Xml(e.to_string())
}

fn attr(e: &BytesStart<'_>, name: &[u8]) -> Result<Option<String>, NuspecError> {
    for a in e.attributes() {
        let a = a.map_err(xml)?;
        if a.key.local_name().as_ref() == name {
            return Ok(Some(a.unescape_value().map_err(xml)?.into_owned()));
        }
    }
    Ok(None)
}

fn open_element(n: &mut Nuspec, path: &[String], e: &BytesStart<'_>) -> Result<(), NuspecError> {
    let at = |p: &[&str]| path.len() == p.len() && path.iter().zip(p).all(|(a, b)| a == b);
    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
    if at(&["package", "metadata", "dependencies"]) && local == "group" {
        n.dependency_groups.push(DependencyGroup {
            target_framework: attr(e, b"targetFramework")?,
            dependencies: Vec::new(),
        });
    } else if local == "dependency"
        && (at(&["package", "metadata", "dependencies"])
            || at(&["package", "metadata", "dependencies", "group"]))
    {
        let dep = Dependency {
            id: attr(e, b"id")?.ok_or_else(|| xml("a dependency without an id"))?,
            range: attr(e, b"version")?,
            exclude: attr(e, b"exclude")?,
            include: attr(e, b"include")?,
        };
        if path.len() == 3 {
            match n
                .dependency_groups
                .iter_mut()
                .find(|g| g.target_framework.is_none())
            {
                Some(g) => g.dependencies.push(dep),
                None => n.dependency_groups.push(DependencyGroup {
                    target_framework: None,
                    dependencies: vec![dep],
                }),
            }
        } else if let Some(group) = n.dependency_groups.last_mut() {
            group.dependencies.push(dep);
        }
    } else if at(&["package", "metadata", "packageTypes"]) && local == "packageType" {
        n.package_types.push(PackageType {
            name: attr(e, b"name")?.ok_or_else(|| xml("a packageType without a name"))?,
            version: attr(e, b"version")?,
        });
    } else if at(&["package", "metadata"]) && local == "license" {
        if attr(e, b"type")?.as_deref() == Some("expression") {
            n.license_expression = Some(String::new());
        }
    } else if at(&["package"]) && local == "metadata" {
        n.min_client_version = attr(e, b"minClientVersion")?;
    }
    Ok(())
}

fn text(n: &mut Nuspec, path: &[String], value: String) {
    if path.len() != 3 || path[0] != "package" || path[1] != "metadata" {
        return;
    }
    let slot = match path[2].as_str() {
        "id" => {
            n.id = value;
            return;
        }
        "version" => {
            n.version = value;
            return;
        }
        "requireLicenseAcceptance" => {
            n.require_license_acceptance = value.eq_ignore_ascii_case("true");
            return;
        }
        "license" => {
            if n.license_expression.is_some() {
                n.license_expression = Some(value);
            }
            return;
        }
        "title" => &mut n.title,
        "description" => &mut n.description,
        "summary" => &mut n.summary,
        "authors" => &mut n.authors,
        "tags" => &mut n.tags,
        "projectUrl" => &mut n.project_url,
        "licenseUrl" => &mut n.license_url,
        "iconUrl" => &mut n.icon_url,
        "releaseNotes" => &mut n.release_notes,
        "copyright" => &mut n.copyright,
        "language" => &mut n.language,
        _ => return,
    };
    *slot = Some(value);
}

/// Parse a nuspec document; `id` and `version` are required.
pub fn parse(bytes: &[u8]) -> Result<Nuspec, NuspecError> {
    if bytes.len() as u64 > MAX_NUSPEC_BYTES {
        return Err(xml("the nuspec exceeds its size cap"));
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut n = Nuspec::default();
    let mut path: Vec<String> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(xml)? {
            Event::DocType(_) => return Err(xml("a DTD is not accepted")),
            Event::Start(e) => {
                open_element(&mut n, &path, &e)?;
                path.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Event::Empty(e) => open_element(&mut n, &path, &e)?,
            Event::End(_) => {
                path.pop();
            }
            Event::Text(t) => text(&mut n, &path, t.unescape().map_err(xml)?.into_owned()),
            Event::CData(t) => text(&mut n, &path, String::from_utf8_lossy(&t).into_owned()),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if n.id.is_empty() || n.version.is_empty() {
        return Err(xml("id and version are required"));
    }
    Ok(n)
}

/// The nuspec at the root of a `.nupkg`, as bytes, and its parse.
pub fn from_package(nupkg: &[u8]) -> Result<(Vec<u8>, Nuspec), NuspecError> {
    let pkg = |e: zip::result::ZipError| NuspecError::Package(e.to_string());
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(nupkg)).map_err(pkg)?;
    let name = archive
        .file_names()
        .find(|n| !n.contains('/') && n.to_ascii_lowercase().ends_with(".nuspec"))
        .map(str::to_string)
        .ok_or_else(|| NuspecError::Package("no .nuspec at the package root".to_string()))?;
    let file = archive.by_name(&name).map_err(pkg)?;
    let mut bytes = Vec::new();
    file.take(MAX_NUSPEC_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| NuspecError::Package(e.to_string()))?;
    let parsed = parse(&bytes)?;
    Ok((bytes, parsed))
}

/// Every entry path of a `.nupkg`, for the facts policy reads.
pub fn entries(nupkg: &[u8]) -> Result<Vec<String>, NuspecError> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(nupkg))
        .map_err(|e| NuspecError::Package(e.to_string()))?;
    Ok(archive.file_names().map(str::to_string).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write as _;

    pub(crate) fn nuspec_xml(id: &str, version: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata minClientVersion="2.12">
    <id>{id}</id>
    <version>{version}</version>
    <authors>Ann &amp; Bob</authors>
    <description><![CDATA[A <b>test</b> package]]></description>
    <license type="expression">MIT</license>
    <tags>json test</tags>
    <packageTypes><packageType name="Dependency" /></packageTypes>
    <dependencies>
      <group targetFramework="net8.0">
        <dependency id="Newtonsoft.Json" version="[13.0.1, )" exclude="Build" />
      </group>
      <group targetFramework="netstandard2.0" />
    </dependencies>
  </metadata>
</package>"#
        )
    }

    pub(crate) fn nupkg(id: &str, version: &str, extra: &[&str]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(format!("{id}.nuspec"), options).unwrap();
        zip.write_all(nuspec_xml(id, version).as_bytes()).unwrap();
        zip.start_file(format!("lib/net8.0/{id}.dll"), options)
            .unwrap();
        zip.write_all(b"MZ").unwrap();
        for path in extra {
            zip.start_file(*path, options).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn a_nuspec_is_read_with_its_groups_and_types() {
        let n = parse(nuspec_xml("My.Lib", "1.0.0-Beta").as_bytes()).unwrap();
        assert_eq!(n.id, "My.Lib");
        assert_eq!(n.version, "1.0.0-Beta");
        assert_eq!(n.authors.as_deref(), Some("Ann & Bob"));
        assert_eq!(n.description.as_deref(), Some("A <b>test</b> package"));
        assert_eq!(n.license_expression.as_deref(), Some("MIT"));
        assert_eq!(n.min_client_version.as_deref(), Some("2.12"));
        assert_eq!(
            n.package_types,
            vec![PackageType {
                name: "Dependency".into(),
                version: None
            }]
        );
        assert_eq!(n.dependency_groups.len(), 2);
        assert_eq!(
            n.dependency_groups[0].target_framework.as_deref(),
            Some("net8.0")
        );
        assert_eq!(n.dependency_groups[0].dependencies[0].id, "Newtonsoft.Json");
        assert_eq!(
            n.dependency_groups[0].dependencies[0].range.as_deref(),
            Some("[13.0.1, )")
        );
        assert!(n.dependency_groups[1].dependencies.is_empty());
    }

    #[test]
    fn a_nuspec_expands_no_entity() {
        let dtd = r#"<?xml version="1.0"?><!DOCTYPE package [<!ENTITY x "boom">]><package><metadata><id>&x;</id><version>1.0.0</version></metadata></package>"#;
        assert!(parse(dtd.as_bytes()).is_err());
        let undeclared =
            r#"<package><metadata><id>&x;</id><version>1.0.0</version></metadata></package>"#;
        assert!(parse(undeclared.as_bytes()).is_err());
        let big = format!(
            "<package>{}</package>",
            " ".repeat(MAX_NUSPEC_BYTES as usize)
        );
        assert!(parse(big.as_bytes()).is_err());
        assert!(parse(b"<package><metadata><id>a</id></metadata></package>").is_err());
    }

    #[test]
    fn the_nuspec_comes_from_the_package_root() {
        let bytes = nupkg("My.Lib", "1.0.0", &["tools/install.ps1"]);
        let (raw, n) = from_package(&bytes).unwrap();
        assert_eq!(n.id, "My.Lib");
        assert!(String::from_utf8(raw).unwrap().contains("<id>My.Lib</id>"));
        assert!(entries(&bytes)
            .unwrap()
            .contains(&"tools/install.ps1".to_string()));
        assert!(matches!(
            from_package(b"not a zip"),
            Err(NuspecError::Package(_))
        ));
    }
}

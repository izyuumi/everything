use std::collections::BTreeMap;

use serde::Deserialize;
use serde_norway::Value;

use super::{MAX_FRONTMATTER_BYTES, SkillIssue};

pub(super) struct Document<'a> {
    pub frontmatter: &'a str,
    pub body: &'a str,
}

pub(super) fn split(raw: &str) -> Option<Document<'_>> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let close = rest.find("\n---")?;
    let frontmatter = rest[..close].strip_suffix('\r').unwrap_or(&rest[..close]);
    let after = &rest[close + 4..];
    let after = after.strip_prefix('\r').unwrap_or(after);
    let body = after.strip_prefix('\n').unwrap_or(after);
    Some(Document { frontmatter, body })
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawFrontmatter {
    name: Option<Value>,
    description: Option<Value>,
    license: Option<Value>,
    compatibility: Option<Value>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<Value>,
    metadata: Option<Value>,
}

#[derive(Default)]
pub(super) struct Fields {
    pub name: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub issues: Vec<SkillIssue>,
}

pub(super) fn parse(yaml: &str) -> Result<Fields, SkillIssue> {
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(SkillIssue::FrontmatterTooLarge { bytes: yaml.len() });
    }
    let raw: RawFrontmatter = if yaml.trim().is_empty() {
        RawFrontmatter::default()
    } else {
        serde_norway::from_str(yaml).map_err(|error| SkillIssue::Frontmatter {
            message: error.to_string(),
        })?
    };
    let mut fields = Fields::default();
    fields.name = string_field(raw.name, "name", &mut fields.issues);
    fields.description = string_field(raw.description, "description", &mut fields.issues);
    fields.license = string_field(raw.license, "license", &mut fields.issues);
    fields.compatibility = string_field(raw.compatibility, "compatibility", &mut fields.issues);
    fields.allowed_tools = string_field(raw.allowed_tools, "allowed-tools", &mut fields.issues);
    match raw.metadata {
        None | Some(Value::Null) => {}
        Some(Value::Mapping(mapping)) => {
            let mut complete = true;
            for (key, value) in mapping {
                match (key, value) {
                    (Value::String(key), Value::String(value)) => {
                        fields.metadata.insert(key, value);
                    }
                    _ => complete = false,
                }
            }
            if !complete {
                fields.issues.push(SkillIssue::MetadataNotStringMap);
            }
        }
        Some(_) => fields.issues.push(SkillIssue::MetadataNotStringMap),
    }
    Ok(fields)
}

fn string_field(
    value: Option<Value>,
    field: &'static str,
    issues: &mut Vec<SkillIssue>,
) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value),
        Some(_) => {
            issues.push(SkillIssue::NotAString { field });
            None
        }
    }
}

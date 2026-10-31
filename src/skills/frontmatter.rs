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
    if has_anchor_or_alias(yaml) {
        return Err(SkillIssue::YamlAnchorOrAlias);
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

fn has_anchor_or_alias(yaml: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut flow = 0_usize;
    let mut block: Option<usize> = None;
    let mut node = true;
    for line in yaml.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if quote.is_none() {
            if let Some(parent) = block {
                if line.trim().is_empty() || indent > parent {
                    continue;
                }
                block = None;
            }
            if flow == 0 {
                node = true;
            }
        }
        let mut chars = line.chars().peekable();
        let mut previous = ' ';
        while let Some(character) = chars.next() {
            let next_blank = chars.peek().is_none_or(|next| next.is_whitespace());
            match quote {
                Some('"') => {
                    if character == '\\' {
                        chars.next();
                    } else if character == '"' {
                        quote = None;
                    }
                    previous = character;
                    continue;
                }
                Some(_) => {
                    if character == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                        } else {
                            quote = None;
                        }
                    }
                    previous = character;
                    continue;
                }
                None => {}
            }
            if character.is_whitespace() {
                previous = character;
                continue;
            }
            if character == '#' && previous.is_whitespace() {
                break;
            }
            if node {
                match character {
                    '&' | '*' => return true,
                    '"' | '\'' => {
                        quote = Some(character);
                        node = false;
                    }
                    '[' | '{' => flow += 1,
                    ']' | '}' => {
                        flow = flow.saturating_sub(1);
                        node = false;
                    }
                    ',' if flow > 0 => {}
                    '-' | '?' | ':' if next_blank => {}
                    '|' | '>' if flow == 0 => {
                        block = Some(indent);
                        break;
                    }
                    '!' => while chars.next_if(|next| !next.is_whitespace()).is_some() {},
                    _ => node = false,
                }
            } else if character == ':' && (next_blank || flow > 0) {
                node = true;
            } else if flow > 0 {
                match character {
                    ',' => node = true,
                    '[' | '{' => {
                        flow += 1;
                        node = true;
                    }
                    ']' | '}' => flow = flow.saturating_sub(1),
                    _ => {}
                }
            }
            previous = character;
        }
    }
    false
}

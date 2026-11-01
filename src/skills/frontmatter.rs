use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{MAX_FRONTMATTER_BYTES, MAX_FRONTMATTER_STRING_BYTES, SkillIssue};

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
    name: Option<Scalar>,
    description: Option<Scalar>,
    license: Option<Scalar>,
    compatibility: Option<Scalar>,
    #[serde(rename = "allowed-tools")]
    allowed_tools: Option<Scalar>,
    metadata: Option<Metadata>,
}

enum Scalar {
    Text(String),
    True,
    Other,
}

impl Scalar {
    fn bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::True | Self::Other => 0,
        }
    }
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ScalarVisitor)
    }
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a scalar")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Scalar, E> {
        Ok(Scalar::Text(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Scalar, E> {
        Ok(Scalar::Text(value))
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Scalar, E> {
        Ok(if value { Scalar::True } else { Scalar::Other })
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_none<E: de::Error>(self) -> Result<Scalar, E> {
        Ok(Scalar::Other)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, _: A) -> Result<Scalar, A::Error> {
        Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
    }

    fn visit_map<A: MapAccess<'de>>(self, _: A) -> Result<Scalar, A::Error> {
        Err(de::Error::invalid_type(de::Unexpected::Map, &self))
    }
}

#[derive(Default)]
struct Metadata {
    entries: BTreeMap<String, String>,
    non_string: Vec<String>,
    internal: bool,
    not_a_map: bool,
    bytes: usize,
}

impl<'de> Deserialize<'de> for Metadata {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(MetadataVisitor)
    }
}

struct MetadataVisitor;

impl MetadataVisitor {
    fn not_a_map() -> Metadata {
        Metadata {
            not_a_map: true,
            ..Metadata::default()
        }
    }
}

impl<'de> Visitor<'de> for MetadataVisitor {
    type Value = Metadata;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map of strings to strings")
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Metadata, E> {
        Ok(Self::not_a_map())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Metadata, E> {
        Ok(Self::not_a_map())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Metadata, E> {
        Ok(Self::not_a_map())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Metadata, E> {
        Ok(Self::not_a_map())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Metadata, E> {
        Ok(Self::not_a_map())
    }

    fn visit_unit<E: de::Error>(self) -> Result<Metadata, E> {
        Ok(Metadata::default())
    }

    fn visit_none<E: de::Error>(self) -> Result<Metadata, E> {
        Ok(Metadata::default())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, _: A) -> Result<Metadata, A::Error> {
        Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Metadata, A::Error> {
        let mut metadata = Metadata::default();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value::<Scalar>()?;
            metadata.bytes = metadata
                .bytes
                .saturating_add(key.len())
                .saturating_add(value.bytes());
            if metadata.bytes > MAX_FRONTMATTER_STRING_BYTES {
                continue;
            }
            match value {
                Scalar::Text(value) => {
                    metadata.entries.insert(key, value);
                }
                Scalar::True => {
                    metadata.internal |= key == "internal";
                    metadata.non_string.push(key);
                }
                Scalar::Other => metadata.non_string.push(key),
            }
        }
        Ok(metadata)
    }
}

#[derive(Default)]
pub(super) struct Fields {
    pub name: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub internal: bool,
    pub issues: Vec<SkillIssue>,
}

pub(super) fn parse(yaml: &str) -> Result<Fields, SkillIssue> {
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(SkillIssue::FrontmatterTooLarge { bytes: yaml.len() });
    }
    if yaml
        .replace("\r\n", "\n")
        .contains(['\r', '\u{85}', '\u{2028}', '\u{2029}'])
    {
        return Err(SkillIssue::UnsupportedLineBreak);
    }
    let raw: RawFrontmatter = if yaml.trim().is_empty() {
        RawFrontmatter::default()
    } else {
        serde_norway::from_str(yaml).map_err(|error| SkillIssue::Frontmatter {
            message: error.to_string(),
        })?
    };
    let metadata = raw.metadata.unwrap_or_default();
    let bytes = [
        &raw.name,
        &raw.description,
        &raw.license,
        &raw.compatibility,
        &raw.allowed_tools,
    ]
    .into_iter()
    .flatten()
    .map(Scalar::bytes)
    .fold(metadata.bytes, usize::saturating_add);
    if bytes > MAX_FRONTMATTER_STRING_BYTES {
        return Err(SkillIssue::FrontmatterStringsTooLarge { bytes });
    }
    let mut fields = Fields::default();
    fields.name = string_field(raw.name, "name", &mut fields.issues);
    fields.description = string_field(raw.description, "description", &mut fields.issues);
    fields.license = string_field(raw.license, "license", &mut fields.issues);
    fields.compatibility = string_field(raw.compatibility, "compatibility", &mut fields.issues);
    fields.allowed_tools = string_field(raw.allowed_tools, "allowed-tools", &mut fields.issues);
    if metadata.not_a_map {
        fields.issues.push(SkillIssue::MetadataNotStringMap);
    }
    fields.issues.extend(
        metadata
            .non_string
            .into_iter()
            .map(|key| SkillIssue::MetadataValueNotString { key }),
    );
    fields.metadata = metadata.entries;
    fields.internal = metadata.internal;
    Ok(fields)
}

fn string_field(
    value: Option<Scalar>,
    field: &'static str,
    issues: &mut Vec<SkillIssue>,
) -> Option<String> {
    match value {
        None => None,
        Some(Scalar::Text(value)) => Some(value),
        Some(Scalar::True | Scalar::Other) => {
            issues.push(SkillIssue::NotAString { field });
            None
        }
    }
}

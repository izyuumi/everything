mod agents_md;
pub mod cli;
mod discover;
mod fetch;
mod frontmatter;
mod install;
mod lock;
mod tool;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use agents_md::{MAX_PROJECT_INSTRUCTIONS_BYTES, project_instructions};
pub use discover::{
    Cancelled, Catalog, MissingSkill, RejectedSkill, SkillRoots, discover, read_body,
};
pub use lock::LockEntry;
pub use tool::{
    MAX_CATALOG_BYTES, MAX_CATALOG_SKILLS, READ_SKILL_NAME, ReadSkillTool, SkillSummary,
    offered_catalog, read_skill_descriptor, summaries,
};

pub const MAX_SKILL_DIRS_PER_ROOT: usize = 512;
pub const MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 16 * 1024;
pub const MAX_FRONTMATTER_STRING_BYTES: usize = 64 * 1024;
pub const MAX_EXTRA_ROOTS: usize = 8;
pub const MAX_NAME_BYTES: usize = 256;
pub const SPEC_MAX_NAME_BYTES: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_COMPATIBILITY_BYTES: usize = 500;

pub fn is_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

pub fn escaped(text: &str, keep_lines: bool) -> Cow<'_, str> {
    let escape = |character: char| {
        (character.is_control() && !(keep_lines && matches!(character, '\n' | '\t')))
            || is_format_character(character)
    };
    if !text.chars().any(escape) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        if escape(character) {
            output.extend(character.escape_unicode());
        } else {
            output.push(character);
        }
    }
    Cow::Owned(output)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillName(String);

impl TryFrom<String> for SkillName {
    type Error = SkillIssue;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse_stored(&value)
    }
}

impl From<SkillName> for String {
    fn from(value: SkillName) -> Self {
        value.0
    }
}

impl SkillName {
    pub fn parse(raw: &str) -> Result<Self, SkillIssue> {
        let name = Self::parse_stored(raw)?;
        match name.issue() {
            Some(issue) => Err(issue),
            None => Ok(name),
        }
    }

    pub fn parse_stored(raw: &str) -> Result<Self, SkillIssue> {
        let reason = if raw.is_empty() {
            Some("empty")
        } else if raw.len() > MAX_NAME_BYTES {
            Some("longer than 256 bytes")
        } else if raw.contains(['/', '\\', '\0']) {
            Some("contains a path separator or NUL")
        } else if raw.chars().any(char::is_control) {
            Some("contains a control character")
        } else if raw.starts_with('.') {
            Some("is a dot name or starts with a dot")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(SkillIssue::UnsafeName {
                name: raw.to_owned(),
                reason,
            }),
            None => Ok(Self(raw.to_owned())),
        }
    }

    pub fn issue(&self) -> Option<SkillIssue> {
        self.0
            .chars()
            .any(is_format_character)
            .then(|| SkillIssue::UnsafeName {
                name: self.0.clone(),
                reason: "contains a bidirectional or invisible format character",
            })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn dir_name(&self) -> SkillDirName {
        SkillDirName::sanitize(&self.0)
    }

    fn follows_spec_grammar(&self) -> bool {
        self.0.len() <= SPEC_MAX_NAME_BYTES
            && self.0.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9'))
            })
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SkillDirName(String);

impl SkillDirName {
    pub fn sanitize(raw: &str) -> Self {
        let lowered = raw.to_lowercase();
        let mut replaced = String::with_capacity(lowered.len());
        let mut in_run = false;
        for character in lowered.chars() {
            if matches!(character, 'a'..='z' | '0'..='9' | '.' | '_') {
                replaced.push(character);
                in_run = false;
            } else if !in_run {
                replaced.push('-');
                in_run = true;
            }
        }
        let trimmed = replaced.trim_matches(['.', '-']);
        let truncated = &trimmed[..trimmed.len().min(255)];
        if truncated.is_empty() {
            Self("unnamed-skill".to_owned())
        } else {
            Self(truncated.to_owned())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillDirName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum SkillRoot {
    Project(PathBuf),
    Global(PathBuf),
    Extra(PathBuf),
}

impl SkillRoot {
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Project(path) | Self::Global(path) | Self::Extra(path) => path,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Project(_) => "project",
            Self::Global(_) => "global",
            Self::Extra(_) => "extra",
        }
    }

    pub fn scope(&self) -> &'static str {
        match self {
            Self::Project(_) => "project",
            Self::Global(_) | Self::Extra(_) => "global",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SkillRecord {
    pub name: SkillName,
    pub dir_name: SkillDirName,
    pub description: String,
    pub root: SkillRoot,
    pub dir: PathBuf,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub allowed_tools: Option<String>,
    pub source: Option<LockEntry>,
    pub issues: Vec<SkillIssue>,
    pub shadowed: bool,
    #[serde(skip)]
    pub preview: String,
}

impl SkillRecord {
    pub fn offered(&self) -> bool {
        !self.shadowed
            && !self.description.is_empty()
            && self.description.len() <= MAX_DESCRIPTION_BYTES
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillIssue {
    UnsafeName { name: String, reason: &'static str },
    MissingName,
    NotAString { field: &'static str },
    NameGrammar,
    DirectoryMismatch { directory: String },
    MissingDescription,
    DescriptionTooLong { bytes: usize },
    CompatibilityTooLong { bytes: usize },
    MetadataNotStringMap,
    MetadataValueNotString { key: String },
    MissingFrontmatter,
    FrontmatterTooLarge { bytes: usize },
    FrontmatterStringsTooLarge { bytes: usize },
    UnsupportedLineBreak,
    Frontmatter { message: String },
    SkillFileTooLarge { bytes: u64 },
    SkillFileNotRegular,
    NotUtf8,
    SymlinkRefused { reason: &'static str },
    Untrusted { reason: &'static str },
    Unreadable { message: String },
}

impl fmt::Display for SkillIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeName { name, reason } => {
                write!(formatter, "unsafe name {name:?}: {reason}; not offered")
            }
            Self::MissingName => formatter.write_str("frontmatter has no name; not offered"),
            Self::NotAString { field } => write!(formatter, "frontmatter {field} is not a string"),
            Self::NameGrammar => formatter.write_str(
                "name does not match the spec: ^[a-z0-9]+(-[a-z0-9]+)*$ and at most 64 bytes",
            ),
            Self::DirectoryMismatch { directory } => {
                write!(formatter, "directory {directory:?} does not equal the name")
            }
            Self::MissingDescription => {
                formatter.write_str("description is missing or empty; not offered")
            }
            Self::DescriptionTooLong { bytes } => write!(
                formatter,
                "description is {bytes} bytes, above 1024; not offered"
            ),
            Self::CompatibilityTooLong { bytes } => {
                write!(formatter, "compatibility is {bytes} bytes, above 500")
            }
            Self::MetadataNotStringMap => {
                formatter.write_str("metadata is not a map of strings to strings")
            }
            Self::MetadataValueNotString { key } => {
                write!(formatter, "metadata {key:?} is not a string")
            }
            Self::MissingFrontmatter => {
                formatter.write_str("SKILL.md has no YAML frontmatter; not offered")
            }
            Self::FrontmatterTooLarge { bytes } => write!(
                formatter,
                "frontmatter is {bytes} bytes, above 16 KiB; not offered"
            ),
            Self::FrontmatterStringsTooLarge { bytes } => write!(
                formatter,
                "frontmatter strings total {bytes} bytes after alias expansion, above 64 KiB; not offered"
            ),
            Self::UnsupportedLineBreak => formatter.write_str(
                "frontmatter contains a lone CR, U+0085, U+2028 or U+2029 line break; not offered",
            ),
            Self::Frontmatter { message } => {
                write!(formatter, "frontmatter is invalid: {message}; not offered")
            }
            Self::SkillFileTooLarge { bytes } => write!(
                formatter,
                "SKILL.md is {bytes} bytes, above 256 KiB; not offered"
            ),
            Self::SkillFileNotRegular => {
                formatter.write_str("SKILL.md is not a regular file; not offered")
            }
            Self::NotUtf8 => formatter.write_str("SKILL.md is not UTF-8; not offered"),
            Self::SymlinkRefused { reason } => {
                write!(formatter, "symlinked skill directory refused: {reason}")
            }
            Self::Untrusted { reason } => {
                write!(formatter, "refused: {reason}; not offered")
            }
            Self::Unreadable { message } => write!(formatter, "unreadable: {message}"),
        }
    }
}

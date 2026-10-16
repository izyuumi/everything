mod filesystem;
mod shell;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::permissions::ToolScopeRule;

use super::{Tool, ToolError, ToolOutput, ToolRegistry};

pub const MAX_BUILTIN_OUTPUT_BYTES: usize = 65_536;

#[derive(Clone, Default)]
pub struct BuiltinConfig {
    pub redact_literals: Vec<String>,
}

pub struct BuiltinTools {
    registrations: Vec<(Arc<dyn Tool>, ToolScopeRule)>,
}

impl BuiltinTools {
    pub fn new(config: BuiltinConfig) -> Result<Self, ToolError> {
        let redactor = Redactor::new(config.redact_literals)?;
        let mut registrations = filesystem::registrations(redactor.clone());
        registrations.push((
            Arc::new(shell::ShellTool::new(redactor)),
            ToolScopeRule::Host {
                service: "native_shell".into(),
            },
        ));
        Ok(Self { registrations })
    }

    pub fn register_into(
        &self,
        registry: &mut ToolRegistry,
    ) -> Result<BTreeMap<String, ToolScopeRule>, ToolError> {
        for (tool, _) in &self.registrations {
            if registry.descriptor(&tool.descriptor().name).is_ok() {
                return Err(ToolError::DuplicateTool {
                    name: tool.descriptor().name.clone(),
                });
            }
        }
        let mut scopes = BTreeMap::new();
        for (tool, scope) in &self.registrations {
            registry.register(Arc::clone(tool))?;
            scopes.insert(tool.descriptor().name.clone(), scope.clone());
        }
        Ok(scopes)
    }
}

#[derive(Clone)]
struct Redactor {
    literals: Arc<Vec<String>>,
}

impl Redactor {
    fn new(mut literals: Vec<String>) -> Result<Self, ToolError> {
        if literals.len() > 64
            || literals
                .iter()
                .any(|value| value.is_empty() || value.len() > 1024)
            || literals.iter().map(String::len).sum::<usize>() > 16_384
        {
            return Err(ToolError::InvalidContext {
                message: "redaction requires at most 64 nonempty literals, 1024 bytes each and 16384 bytes total".into(),
            });
        }
        literals.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
        literals.dedup();
        Ok(Self {
            literals: Arc::new(literals),
        })
    }

    fn lookaround(&self) -> usize {
        self.literals.iter().map(String::len).max().unwrap_or(1) - 1
    }

    fn apply(&self, value: &str, limit: usize, incomplete: bool) -> RenderedText {
        self.range(value.as_bytes(), 0..value.len(), limit, incomplete)
    }

    fn range(
        &self,
        value: &[u8],
        range: std::ops::Range<usize>,
        limit: usize,
        incomplete: bool,
    ) -> RenderedText {
        let start = range.start.min(value.len());
        let end = range.end.min(value.len());
        let mut bytes = Vec::with_capacity((end.saturating_sub(start)).min(limit));
        let mut redacted = false;
        let mut position = start.saturating_sub(self.lookaround());
        let mut masked_until = position;
        while position < end && bytes.len() <= limit {
            let remaining = &value[position..];
            let matched = self
                .literals
                .iter()
                .find(|literal| remaining.starts_with(literal.as_bytes()));
            let length = matched.map(String::len).or_else(|| {
                (incomplete
                    && self
                        .literals
                        .iter()
                        .any(|literal| literal.as_bytes().starts_with(remaining)))
                .then_some(remaining.len())
            });
            if let Some(length) = length {
                let mask_end = position + length;
                if mask_end > start {
                    if masked_until <= start || position >= masked_until {
                        bytes.extend_from_slice(b"[REDACTED]");
                    }
                    redacted = true;
                    masked_until = masked_until.max(mask_end);
                }
            }
            if position >= start && position >= masked_until {
                bytes.push(value[position]);
            }
            position += 1;
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let truncated = position < end || text.len() > limit;
        truncate_utf8(&mut text, limit);
        RenderedText {
            text,
            redacted,
            truncated,
        }
    }

    fn error(&self, value: impl std::fmt::Display) -> String {
        self.apply(&value.to_string(), 2048, false).text
    }
}

struct RenderedText {
    text: String,
    redacted: bool,
    truncated: bool,
}

fn truncate_utf8(value: &mut String, limit: usize) {
    let mut boundary = limit.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

fn output(name: &str, content: Value, is_error: bool) -> Result<ToolOutput, ToolError> {
    let output = ToolOutput { content, is_error };
    if serde_json::to_vec(&output)
        .map_err(|error| ToolError::EffectsUnknown {
            name: name.into(),
            message: error.to_string(),
            cleanup_completed: true,
        })?
        .len()
        > MAX_BUILTIN_OUTPUT_BYTES
    {
        return Err(ToolError::EffectsUnknown {
            name: name.into(),
            message: "builtin output exceeds its serialized 65536-byte limit".into(),
            cleanup_completed: true,
        });
    }
    Ok(output)
}

fn no_effect(name: &str, message: impl Into<String>) -> ToolError {
    ToolError::FailedWithoutSideEffects {
        name: name.into(),
        message: message.into(),
    }
}

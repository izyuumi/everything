use std::{collections::BTreeMap, sync::Arc};

use jsonschema::Validator;
use serde_json::Value;

use super::{
    ArgumentViolation, MAX_TOOL_ARGUMENT_BYTES, MAX_TOOL_SCHEMA_BYTES, Tool, ToolDescriptor,
    ToolError, ToolExecutionContext, ToolOutput,
};

const MAX_ARGUMENT_VIOLATIONS: usize = 32;
const MAX_DIAGNOSTIC_CHARACTERS: usize = 2_048;

struct RegisteredTool {
    tool: Arc<dyn Tool>,
    descriptor: ToolDescriptor,
    validator: Validator,
    settings_only: bool,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<RegisteredTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        self.register_profile(tool, false)
    }

    pub(crate) fn register_settings(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        self.register_profile(tool, true)
    }

    fn register_profile(
        &mut self,
        tool: Arc<dyn Tool>,
        settings_only: bool,
    ) -> Result<(), ToolError> {
        let descriptor = tool.descriptor().clone();
        validate_descriptor(&descriptor)?;
        if self.tools.contains_key(&descriptor.name) {
            return Err(ToolError::DuplicateTool {
                name: descriptor.name,
            });
        }
        check_json_size(
            &descriptor.input_schema,
            "tool argument schema",
            MAX_TOOL_SCHEMA_BYTES,
        )?;
        let validator = jsonschema::options()
            .offline()
            .should_validate_formats(true)
            .with_pattern_options(
                jsonschema::PatternOptions::fancy_regex()
                    .backtrack_limit(10_000)
                    .size_limit(MAX_TOOL_SCHEMA_BYTES)
                    .dfa_size_limit(MAX_TOOL_SCHEMA_BYTES),
            )
            .build(&descriptor.input_schema)
            .map_err(|error| ToolError::InvalidSchema {
                name: descriptor.name.clone(),
                message: diagnostic_message(&error),
            })?;
        self.tools.insert(
            descriptor.name.clone(),
            Arc::new(RegisteredTool {
                tool,
                descriptor,
                validator,
                settings_only,
            }),
        );
        Ok(())
    }

    pub fn discover(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .filter(|entry| !entry.settings_only)
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    pub fn descriptor(&self, name: &str) -> Result<&ToolDescriptor, ToolError> {
        let entry = self.general(name)?;
        Ok(&entry.descriptor)
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.tools
            .values()
            .filter(|entry| !entry.settings_only)
            .map(|entry| &entry.descriptor)
    }

    pub fn validate(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        validate_arguments(self.general(name)?, arguments)
    }

    pub async fn execute(
        &self,
        name: &str,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let entry = Arc::clone(self.get(name)?);
        context.check_cancelled()?;
        match (entry.settings_only, context.settings_capability()) {
            (true, Some(capability)) => capability
                .check(context.request_id(), context.task_id(), name)
                .map_err(|_| ToolError::InvalidContext {
                    message: "restricted tool capability does not match".into(),
                })?,
            (false, None) => {}
            _ => return Err(ToolError::UnknownTool { name: name.into() }),
        }
        let validation_entry = Arc::clone(&entry);
        let retained_context = context.clone();
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|error| ToolError::ValidationWorker {
                message: error.to_string(),
            })?;
        let arguments = runtime
            .spawn_blocking(move || {
                let _retained_context = retained_context;
                validate_arguments(&validation_entry, &arguments)?;
                Ok(arguments)
            })
            .await
            .map_err(|error| ToolError::ValidationWorker {
                message: error.to_string(),
            })??;
        context.check_cancelled()?;
        entry.tool.execute(context, arguments).await
    }

    pub(crate) fn settings_descriptor(&self, name: &str) -> Result<&ToolDescriptor, ToolError> {
        let entry = self.get(name)?;
        if !entry.settings_only {
            return Err(ToolError::UnknownTool { name: name.into() });
        }
        Ok(&entry.descriptor)
    }

    pub(crate) fn validate_settings(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        self.settings_descriptor(name)?;
        validate_arguments(self.get(name)?, arguments)
    }

    fn general(&self, name: &str) -> Result<&Arc<RegisteredTool>, ToolError> {
        let entry = self.get(name)?;
        if entry.settings_only {
            return Err(ToolError::UnknownTool { name: name.into() });
        }
        Ok(entry)
    }

    fn get(&self, name: &str) -> Result<&Arc<RegisteredTool>, ToolError> {
        self.tools.get(name).ok_or_else(|| ToolError::UnknownTool {
            name: name.to_owned(),
        })
    }
}

fn validate_descriptor(descriptor: &ToolDescriptor) -> Result<(), ToolError> {
    let valid_name = !descriptor.name.is_empty()
        && descriptor.name.len() <= 128
        && descriptor
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
    if !valid_name {
        return Err(ToolError::InvalidDescriptor {
            name: descriptor.name.clone(),
            message:
                "name must contain 1 to 128 ASCII letters, digits, underscores, dots or hyphens"
                    .into(),
        });
    }
    if descriptor.description.trim().is_empty() || descriptor.description.len() > 8_192 {
        return Err(ToolError::InvalidDescriptor {
            name: descriptor.name.clone(),
            message: "description must contain 1 to 8192 bytes".into(),
        });
    }
    Ok(())
}

fn validate_arguments(entry: &RegisteredTool, arguments: &Value) -> Result<(), ToolError> {
    check_json_size(arguments, "tool arguments", MAX_TOOL_ARGUMENT_BYTES)?;
    let violations: Vec<_> = entry
        .validator
        .iter_errors(arguments)
        .take(MAX_ARGUMENT_VIOLATIONS)
        .map(|error| ArgumentViolation {
            instance_path: error.instance_path().to_string(),
            schema_path: error.schema_path().to_string(),
            message: diagnostic_message(&error),
        })
        .collect();
    if !violations.is_empty() {
        return Err(ToolError::InvalidArguments {
            name: entry.descriptor.name.clone(),
            violations,
        });
    }
    Ok(())
}

fn check_json_size(value: &Value, resource: &'static str, limit: usize) -> Result<(), ToolError> {
    if value.to_string().len() > limit {
        return Err(ToolError::ResourceLimit { resource, limit });
    }
    Ok(())
}

fn diagnostic_message(error: &impl std::fmt::Display) -> String {
    error
        .to_string()
        .chars()
        .take(MAX_DIAGNOSTIC_CHARACTERS)
        .collect()
}

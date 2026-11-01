use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::agent::{ConversationConfig, MAX_INSTRUCTIONS_BYTES, ProviderEndpoint, ProviderKind};
use crate::permissions::ExecutionMode;
use crate::scheduler::ExecutionPolicy;
use crate::service::{ServiceError, StateLease};
pub use crate::skills::SkillName;
use crate::tools::ToolRegistry;

pub const DEFAULT_AGENT_ID: AgentId = AgentId(Uuid::from_u128(1));
pub const DEFAULT_INSTRUCTIONS: &str = "You are Vorra, a helpful assistant.";
pub const AGENTS_SCHEMA: u64 = 1;
pub const MAX_AGENTS: usize = 64;
pub const MAX_AGENTS_FILE_BYTES: usize = 512 * 1024;
pub const MAX_PERSONA_BYTES: usize = 32 * 1024;
pub const MAX_TRAITS: usize = 16;
pub const MAX_TRAIT_VALUE_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_HANDLE_BYTES: usize = 32;
const MAX_SET_ENTRIES: usize = 256;
const MAX_REFERENCE_BYTES: usize = 256;
const AGENTS_FILE: &str = "agents-v1.json";
use crate::scheduler::MESSAGE_AGENT as MESSAGE_AGENT_TOOL;

#[derive(Debug, Error)]
pub enum AgentsError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("agents filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("agents file must be an owner-only single-link regular file")]
    UnsafeFile,
    #[error("agents file exceeds 512 KiB")]
    TooLarge,
    #[error("agents file is malformed")]
    Malformed,
    #[error("agents file schema {0} is not supported")]
    UnsupportedSchema(u64),
    #[error("invalid agent definition: {0}")]
    Invalid(&'static str),
    #[error("agent produces an invalid conversation configuration: {0}")]
    Configuration(#[source] crate::agent::AgentError),
    #[error("agents changed; reload before saving")]
    Conflict,
    #[error("agent is at revision {0}, not the expected revision")]
    StaleRevision(u64),
    #[error("agent was not found")]
    NotFound,
    #[error("agents state lock was poisoned")]
    Poisoned,
}

impl From<rustix::io::Errno> for AgentsError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for AgentId {
    type Err = AgentsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| AgentsError::Invalid("agent id must be a UUID"))
    }
}

fn valid_handle(value: &str) -> bool {
    (1..=MAX_HANDLE_BYTES).contains(&value.len())
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9'))
        })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentName(String);

impl AgentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentName {
    type Error = AgentsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !valid_handle(&value) {
            return Err(AgentsError::Invalid(
                "agent names are 1 to 32 bytes of lowercase letters, digits and single hyphens",
            ));
        }
        Ok(Self(value))
    }
}

impl From<AgentName> for String {
    fn from(value: AgentName) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TraitKey(String);

impl TraitKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TraitKey {
    type Error = AgentsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !valid_handle(&value) {
            return Err(AgentsError::Invalid(
                "trait keys are 1 to 32 bytes of lowercase letters, digits and single hyphens",
            ));
        }
        Ok(Self(value))
    }
}

impl From<TraitKey> for String {
    fn from(value: TraitKey) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModel {
    pub provider: ProviderKind,
    pub model: String,
    pub endpoint: ProviderEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentTools {
    Off,
    On {
        max_mode: ExecutionMode,
        allow: ToolAllow,
    },
}

impl std::fmt::Display for AgentTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let AgentTools::On { max_mode, allow } = self else {
            return formatter.write_str("tools off");
        };
        formatter.write_str(match max_mode {
            ExecutionMode::ReadOnly => "read-only",
            ExecutionMode::TrustedWorkspace => "trusted-workspace",
            ExecutionMode::FullHost => "full-host",
            ExecutionMode::SettingsRestricted => "settings-restricted",
        })?;
        match allow {
            ToolAllow::All => formatter.write_str(" · all tools"),
            ToolAllow::Only(references) if references.is_empty() => {
                formatter.write_str(" · no tools")
            }
            ToolAllow::Only(references) => {
                formatter.write_str(" · only ")?;
                for (index, reference) in references.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    match reference {
                        ToolRef::Tool(name) => formatter.write_str(name)?,
                        ToolRef::McpServer(server) => write!(formatter, "mcp:{server}")?,
                    }
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolAllow {
    All,
    Only(BTreeSet<ToolRef>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolRef {
    Tool(String),
    McpServer(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAccess {
    Shared,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillAssignment {
    None,
    All,
    Only(BTreeSet<SkillName>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Messaging {
    Closed,
    May(BTreeSet<AgentId>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedAgent {
    pub id: AgentId,
    pub revision: u64,
    pub memory: MemoryAccess,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    pub id: AgentId,
    pub name: AgentName,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AgentModel>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub traits: BTreeMap<TraitKey, String>,
    pub tools: AgentTools,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub memory: MemoryAccess,
    pub skills: SkillAssignment,
    pub messaging: Messaging,
    #[serde(default)]
    pub revision: u64,
}

impl AgentDefinition {
    pub fn synthesized_default() -> Self {
        Self {
            id: DEFAULT_AGENT_ID,
            name: AgentName("default".into()),
            description: "Default Vorra agent".into(),
            persona: None,
            model: None,
            traits: BTreeMap::new(),
            tools: AgentTools::On {
                max_mode: ExecutionMode::FullHost,
                allow: ToolAllow::All,
            },
            workspace: None,
            memory: MemoryAccess::Shared,
            skills: SkillAssignment::None,
            messaging: Messaging::Closed,
            revision: 1,
        }
    }

    pub fn render_instructions(&self, base: &str) -> String {
        let mut rendered = self.persona.as_deref().unwrap_or(base).to_owned();
        if !self.traits.is_empty() {
            rendered.push_str("\n\nAgent traits:");
            for (key, value) in &self.traits {
                rendered.push_str("\n- ");
                rendered.push_str(key.as_str());
                rendered.push_str(": ");
                rendered.push_str(value);
            }
        }
        rendered
    }

    pub fn configure(&self, base: ConversationConfig) -> Result<ConversationConfig, AgentsError> {
        let mut configuration = base;
        configuration.instructions = self.render_instructions(&configuration.instructions);
        if let Some(model) = &self.model {
            configuration.provider = model.provider;
            configuration.model = model.model.clone();
            configuration.endpoint = model.endpoint.clone();
            if let Some(tokens) = model.max_output_tokens {
                configuration.max_output_tokens = tokens;
            }
            if model.temperature.is_some() {
                configuration.temperature = model.temperature;
            }
        }
        configuration
            .validate()
            .map_err(AgentsError::Configuration)?;
        Ok(configuration)
    }

    fn validate(&self) -> Result<(), AgentsError> {
        if self.id.0.is_nil() {
            return Err(AgentsError::Invalid("agent ids must be non-nil UUIDs"));
        }
        if self.revision == 0 || self.revision > i64::MAX as u64 {
            return Err(AgentsError::Invalid("agent revision is out of range"));
        }
        if self.description.trim().is_empty()
            || self.description.len() > MAX_DESCRIPTION_BYTES
            || self.description.chars().any(char::is_control)
        {
            return Err(AgentsError::Invalid(
                "description requires 1 to 1024 bytes without control characters",
            ));
        }
        if let Some(persona) = &self.persona
            && (persona.trim().is_empty()
                || persona.len() > MAX_PERSONA_BYTES
                || persona
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t')))
        {
            return Err(AgentsError::Invalid(
                "persona requires 1 to 32768 bytes without control characters other than newline and tab",
            ));
        }
        if self.traits.len() > MAX_TRAITS {
            return Err(AgentsError::Invalid("at most sixteen traits are allowed"));
        }
        if self.traits.values().any(|value| {
            value.trim().is_empty()
                || value.len() > MAX_TRAIT_VALUE_BYTES
                || value.chars().any(char::is_control)
        }) {
            return Err(AgentsError::Invalid(
                "trait values require 1 to 256 bytes without control characters",
            ));
        }
        if self.render_instructions("").len() > MAX_INSTRUCTIONS_BYTES {
            return Err(AgentsError::Invalid(
                "rendered persona and traits exceed 32768 bytes",
            ));
        }
        if let Some(model) = &self.model {
            let probe = ConversationConfig {
                provider: model.provider,
                model: model.model.clone(),
                instructions: "validate".into(),
                endpoint: model.endpoint.clone(),
                max_output_tokens: model.max_output_tokens.unwrap_or(4096),
                temperature: model.temperature,
            };
            probe.validate().map_err(AgentsError::Configuration)?;
        }
        if let AgentTools::On { max_mode, allow } = &self.tools {
            if *max_mode == ExecutionMode::SettingsRestricted {
                return Err(AgentsError::Invalid(
                    "restricted Settings authority cannot be assigned to an agent",
                ));
            }
            if let ToolAllow::Only(tools) = allow
                && (tools.len() > MAX_SET_ENTRIES
                    || tools.iter().any(|tool| {
                        let (ToolRef::Tool(name) | ToolRef::McpServer(name)) = tool;
                        name.is_empty()
                            || name.len() > MAX_REFERENCE_BYTES
                            || name.chars().any(|character| {
                                character.is_control() || character.is_whitespace()
                            })
                    }))
            {
                return Err(AgentsError::Invalid("invalid tool allowlist"));
            }
        }
        if let Some(workspace) = &self.workspace
            && (!workspace.is_absolute()
                || workspace
                    .to_str()
                    .is_none_or(|path| path.len() > 4096 || path.contains('\0')))
        {
            return Err(AgentsError::Invalid("workspace must be an absolute path"));
        }
        if let SkillAssignment::Only(skills) = &self.skills
            && skills.len() > MAX_SET_ENTRIES
        {
            return Err(AgentsError::Invalid("too many assigned skills"));
        }
        if let Messaging::May(targets) = &self.messaging
            && (targets.len() > MAX_AGENTS || targets.iter().any(|target| target.0.is_nil()))
        {
            return Err(AgentsError::Invalid("invalid messaging allowlist"));
        }
        Ok(())
    }

    pub fn captured(&self) -> CapturedAgent {
        CapturedAgent {
            id: self.id,
            revision: self.revision,
            memory: self.memory,
        }
    }

    pub fn tool_ceiling(
        &self,
        mode: ExecutionMode,
        registry: &ToolRegistry,
    ) -> Option<(ExecutionMode, Option<BTreeSet<String>>)> {
        let AgentTools::On { max_mode, allow } = &self.tools else {
            return None;
        };
        let allowed = match allow {
            ToolAllow::All => None,
            ToolAllow::Only(references) => Some(
                registry
                    .descriptors()
                    .filter(|tool| {
                        references.iter().any(|reference| match reference {
                            ToolRef::Tool(name) => *name == tool.name,
                            ToolRef::McpServer(server) => {
                                crate::tools::mcp::server_of(tool) == Some(server.as_str())
                            }
                        })
                    })
                    .map(|tool| tool.name.clone())
                    .collect(),
            ),
        };
        Some((mode.min(*max_mode), allowed))
    }

    pub fn narrow(&self, mut policy: ExecutionPolicy, registry: &ToolRegistry) -> ExecutionPolicy {
        let ceiling = crate::scheduler::DelegationCeiling {
            mode: policy.mode,
            allowed_tools: policy.allowed_tools.clone(),
        };
        match self.tool_ceiling(policy.mode, registry) {
            None => policy.tools_enabled = false,
            Some((mode, allowed)) => {
                policy.mode = mode;
                if let Some(allowed) = allowed {
                    policy.allowed_tools = Some(match policy.allowed_tools {
                        Some(current) => current.intersection(&allowed).cloned().collect(),
                        None => allowed,
                    });
                }
            }
        }
        policy.agent = Some(self.captured());
        if policy.tools_enabled
            && matches!(&self.messaging, Messaging::May(targets) if !targets.is_empty())
            && registry.descriptor(MESSAGE_AGENT_TOOL).is_ok()
            && policy
                .allowed_tools
                .as_ref()
                .is_none_or(|names| names.contains(MESSAGE_AGENT_TOOL))
            && policy.delegation_ceiling.is_none()
        {
            policy.delegation_ceiling = Some(ceiling);
        }
        policy
    }

    pub fn unavailable_tools(&self, registry: &ToolRegistry) -> Vec<String> {
        let AgentTools::On {
            allow: ToolAllow::Only(references),
            ..
        } = &self.tools
        else {
            return Vec::new();
        };
        references
            .iter()
            .filter(|reference| {
                !registry.descriptors().any(|tool| match reference {
                    ToolRef::Tool(name) => *name == tool.name,
                    ToolRef::McpServer(server) => {
                        crate::tools::mcp::server_of(tool) == Some(server.as_str())
                    }
                })
            })
            .map(|reference| match reference {
                ToolRef::Tool(name) => name.clone(),
                ToolRef::McpServer(server) => format!("mcp:{server}"),
            })
            .collect()
    }

    fn same_content(&self, other: &Self) -> bool {
        let mut left = self.clone();
        left.revision = other.revision;
        left == *other
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsFile {
    pub schema: u64,
    pub revision: u64,
    pub default_agent: AgentId,
    pub agents: Vec<AgentDefinition>,
}

impl AgentsFile {
    fn synthesized() -> Self {
        Self {
            schema: AGENTS_SCHEMA,
            revision: 0,
            default_agent: DEFAULT_AGENT_ID,
            agents: vec![AgentDefinition::synthesized_default()],
        }
    }

    pub fn get(&self, id: AgentId) -> Option<&AgentDefinition> {
        self.agents.iter().find(|agent| agent.id == id)
    }

    pub fn by_name(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents
            .iter()
            .find(|agent| agent.name.as_str().eq_ignore_ascii_case(name))
    }

    pub fn resolve(&self, id: Option<AgentId>) -> Result<&AgentDefinition, AgentsError> {
        self.get(id.unwrap_or(self.default_agent))
            .ok_or(AgentsError::NotFound)
    }

    fn put(
        &mut self,
        expected_revision: Option<u64>,
        definition: AgentDefinition,
    ) -> Result<(), AgentsError> {
        let current = self
            .agents
            .iter_mut()
            .find(|agent| agent.id == definition.id);
        match (expected_revision, current) {
            (None, None) => self.agents.push(definition),
            (None, Some(_)) => return Err(AgentsError::Invalid("agent id already exists")),
            (Some(_), None) => return Err(AgentsError::NotFound),
            (Some(expected), Some(agent)) if agent.revision != expected => {
                return Err(AgentsError::StaleRevision(agent.revision));
            }
            (Some(_), Some(agent)) => *agent = definition,
        }
        Ok(())
    }

    fn checked(&self, id: AgentId, expected_revision: u64) -> Result<(), AgentsError> {
        let agent = self.get(id).ok_or(AgentsError::NotFound)?;
        if agent.revision != expected_revision {
            return Err(AgentsError::StaleRevision(agent.revision));
        }
        Ok(())
    }

    fn remove(&mut self, id: AgentId, expected_revision: u64) -> Result<(), AgentsError> {
        self.checked(id, expected_revision)?;
        if self.default_agent == id {
            return Err(AgentsError::Invalid(
                "the current default agent cannot be removed; choose another default first",
            ));
        }
        self.agents.retain(|agent| agent.id != id);
        Ok(())
    }

    fn set_default(&mut self, id: AgentId, expected_revision: u64) -> Result<(), AgentsError> {
        self.checked(id, expected_revision)?;
        self.default_agent = id;
        Ok(())
    }

    fn validate(&self) -> Result<(), AgentsError> {
        if self.schema != AGENTS_SCHEMA {
            return Err(AgentsError::UnsupportedSchema(self.schema));
        }
        if self.revision > i64::MAX as u64 {
            return Err(AgentsError::Invalid("agents revision is out of range"));
        }
        if self.agents.is_empty() || self.agents.len() > MAX_AGENTS {
            return Err(AgentsError::Invalid(
                "between one and sixty-four agents are allowed",
            ));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for agent in &self.agents {
            agent.validate()?;
            if !ids.insert(agent.id) {
                return Err(AgentsError::Invalid("duplicate agent id"));
            }
            if !names.insert(agent.name.as_str().to_ascii_lowercase()) {
                return Err(AgentsError::Invalid("duplicate agent name"));
            }
        }
        if !ids.contains(&self.default_agent) {
            return Err(AgentsError::Invalid("default agent does not exist"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>, AgentsError> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| AgentsError::Invalid("agents encoding failed"))?;
        if bytes.len() > MAX_AGENTS_FILE_BYTES {
            return Err(AgentsError::TooLarge);
        }
        Ok(bytes)
    }
}

#[derive(Deserialize)]
struct SchemaHeader {
    schema: u64,
}

#[derive(Clone)]
pub struct AgentRepository {
    lease: Arc<StateLease>,
    write: Arc<Mutex<()>>,
}

impl AgentRepository {
    pub fn open(lease: Arc<StateLease>) -> Self {
        Self {
            lease,
            write: Arc::new(Mutex::new(())),
        }
    }

    pub fn for_agent(agent: &crate::agent::Agent) -> Result<Self, AgentsError> {
        agent
            .state_lease()
            .map(Self::open)
            .ok_or(AgentsError::Invalid(
                "agents require a leased state directory",
            ))
    }

    pub fn snapshot(&self) -> Result<AgentsFile, AgentsError> {
        Ok(match self.read()? {
            Some((file, _)) => file,
            None => AgentsFile::synthesized(),
        })
    }

    pub fn update(
        &self,
        expected_revision: u64,
        change: impl FnOnce(&mut AgentsFile) -> Result<(), AgentsError>,
    ) -> Result<AgentsFile, AgentsError> {
        let _write = self.write.lock().map_err(|_| AgentsError::Poisoned)?;
        let current = self.read()?;
        let (previous, previous_bytes) = match current {
            Some((file, bytes)) => (file, Some(bytes)),
            None => (AgentsFile::synthesized(), None),
        };
        if previous.revision != expected_revision {
            return Err(AgentsError::Conflict);
        }
        let mut next = previous.clone();
        change(&mut next)?;
        next.schema = AGENTS_SCHEMA;
        next.revision = expected_revision
            .checked_add(1)
            .ok_or(AgentsError::Invalid("agents revision overflow"))?;
        for agent in &mut next.agents {
            if agent.id == DEFAULT_AGENT_ID && previous.get(agent.id).is_none() {
                return Err(AgentsError::Invalid("the default agent id is reserved"));
            }
            agent.revision = match previous.get(agent.id) {
                Some(old) if agent.same_content(old) => old.revision,
                Some(old) => old
                    .revision
                    .checked_add(1)
                    .ok_or(AgentsError::Invalid("agent revision overflow"))?,
                None => 1,
            };
        }
        next.validate()?;
        let bytes = next.encode()?;
        self.write_file(&bytes, previous_bytes.as_deref())?;
        Ok(next)
    }

    pub fn put(
        &self,
        expected_revision: Option<u64>,
        definition: AgentDefinition,
    ) -> Result<AgentsFile, AgentsError> {
        let revision = self.snapshot()?.revision;
        self.update(revision, |file| file.put(expected_revision, definition))
    }

    pub fn remove(&self, id: AgentId, expected_revision: u64) -> Result<AgentsFile, AgentsError> {
        let revision = self.snapshot()?.revision;
        self.update(revision, |file| file.remove(id, expected_revision))
    }

    pub fn set_default(
        &self,
        id: AgentId,
        expected_revision: u64,
    ) -> Result<AgentsFile, AgentsError> {
        let revision = self.snapshot()?.revision;
        self.update(revision, |file| file.set_default(id, expected_revision))
    }

    fn open_existing(&self) -> Result<Option<rustix::fd::OwnedFd>, AgentsError> {
        match rustix::fs::openat(
            &self.lease.directory.file,
            AGENTS_FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => {
                let metadata = rustix::fs::fstat(&descriptor)?;
                if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
                    || metadata.st_uid != self.lease.directory.uid
                    || metadata.st_mode & 0o7777 != 0o600
                    || metadata.st_nlink != 1
                {
                    return Err(AgentsError::UnsafeFile);
                }
                Ok(Some(descriptor))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::MLINK) => Err(AgentsError::UnsafeFile),
            Err(error) => Err(error.into()),
        }
    }

    fn read_bytes(&self) -> Result<Option<Vec<u8>>, AgentsError> {
        let Some(descriptor) = self.open_existing()? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        File::from(descriptor)
            .take((MAX_AGENTS_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_AGENTS_FILE_BYTES {
            return Err(AgentsError::TooLarge);
        }
        Ok(Some(bytes))
    }

    fn read(&self) -> Result<Option<(AgentsFile, Vec<u8>)>, AgentsError> {
        let Some(bytes) = self.read_bytes()? else {
            return Ok(None);
        };
        let header: SchemaHeader =
            serde_json::from_slice(&bytes).map_err(|_| AgentsError::Malformed)?;
        if header.schema != AGENTS_SCHEMA {
            return Err(AgentsError::UnsupportedSchema(header.schema));
        }
        let file: AgentsFile =
            serde_json::from_slice(&bytes).map_err(|_| AgentsError::Malformed)?;
        file.validate()?;
        if file.revision == 0 {
            return Err(AgentsError::Invalid(
                "stored agents revision must be positive",
            ));
        }
        Ok(Some((file, bytes)))
    }

    fn write_file(&self, bytes: &[u8], expected: Option<&[u8]>) -> Result<(), AgentsError> {
        let directory = &self.lease.directory.file;
        let temporary = format!(".agents-{}.tmp", Uuid::new_v4());
        let descriptor = rustix::fs::openat(
            directory,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let _cleanup = TemporaryAgents {
            lease: Arc::clone(&self.lease),
            name: temporary.clone(),
        };
        let mut file = File::from(descriptor);
        file.write_all(bytes)?;
        file.sync_all()?;
        if self.read_bytes()?.as_deref() != expected {
            return Err(AgentsError::Conflict);
        }
        rustix::fs::renameat(directory, &temporary, directory, AGENTS_FILE)?;
        directory.sync_all()?;
        Ok(())
    }
}

struct TemporaryAgents {
    lease: Arc<StateLease>,
    name: String,
}

impl Drop for TemporaryAgents {
    fn drop(&mut self) {
        match rustix::fs::unlinkat(&self.lease.directory.file, &self.name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => tracing::warn!(error = %error, "could not remove temporary agents file"),
        }
    }
}

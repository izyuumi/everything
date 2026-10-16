use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::agent::{ConversationConfig, LoopLimits, ProviderEndpoint, ProviderKind};
use crate::context::{ContextBudget, ContextBuilder};
use crate::credentials::{CredentialError, CredentialKey};
use crate::permissions::ExecutionMode;
use crate::protocol::SchemaVersion;
use crate::service::{ServiceError, StateLease};

const MAX_SETTINGS_BYTES: usize = 64 * 1024;
const SETTINGS_FILE: &str = "host-settings-v1.json";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("host settings filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid host settings: {0}")]
    Invalid(&'static str),
    #[error("host settings changed; reload before saving")]
    Conflict,
    #[error("host settings state lock was poisoned")]
    Poisoned,
}

impl From<rustix::io::Errno> for SettingsError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCredentialReference {
    pub provider: ProviderKind,
    pub endpoint: ProviderEndpoint,
    pub reference: Uuid,
    pub storage: CredentialStorage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStorage {
    Session,
    Native,
}

impl ModelCredentialReference {
    pub fn credential_key(&self) -> Result<CredentialKey, CredentialError> {
        CredentialKey::new("ai.everything.models", &self.scope_key())
    }

    pub(crate) fn scope_key(&self) -> String {
        let endpoint = match &self.endpoint {
            ProviderEndpoint::Official => "official",
            ProviderEndpoint::Loopback(endpoint) => endpoint.as_str(),
        };
        let mut digest = Sha256::new();
        digest.update(b"everything.model.credential.v1\0");
        digest.update(self.provider.id().as_bytes());
        digest.update([0]);
        digest.update(endpoint.as_bytes());
        digest.update([0]);
        digest.update(self.reference.as_bytes());
        format!("{:x}", digest.finalize())
    }

    pub(crate) fn validate(&self) -> Result<(), SettingsError> {
        if self.reference.is_nil() {
            return Err(SettingsError::Invalid(
                "credential references must be non-nil UUIDs",
            ));
        }
        if let ProviderEndpoint::Loopback(endpoint) = &self.endpoint
            && ProviderEndpoint::loopback(endpoint).ok().as_ref() != Some(&self.endpoint)
        {
            return Err(SettingsError::Invalid(
                "credential endpoint is not canonical loopback",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSettings {
    pub schema_version: SchemaVersion,
    pub revision: u64,
    pub default_conversation: Option<ConversationConfig>,
    pub workspace_root: PathBuf,
    pub execution_mode: ExecutionMode,
    pub tools_enabled: bool,
    pub context_tokens: u64,
    pub loop_limits: LoopLimits,
    pub model_credentials: Vec<ModelCredentialReference>,
    pub relay_origin: Option<crate::relay_protocol::RelayOrigin>,
    #[serde(default)]
    pub worker: Option<super::worker::WorkerCoordinatorSettings>,
    #[serde(default)]
    pub memory: super::memory::MemorySettings,
    #[serde(default)]
    pub mcp: super::mcp::McpSettings,
    #[serde(default)]
    pub selection: super::selection::SelectionSettings,
}

impl HostSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.revision == 0 || self.revision > i64::MAX as u64 {
            return Err(SettingsError::Invalid("settings revision is out of range"));
        }
        if !self.workspace_root.is_absolute()
            || self
                .workspace_root
                .to_str()
                .is_none_or(|path| path.len() > 4096)
            || !self.workspace_root.is_dir()
            || self.workspace_root.canonicalize()? != self.workspace_root
        {
            return Err(SettingsError::Invalid(
                "workspace must name an existing physical absolute directory",
            ));
        }
        if let Some(configuration) = &self.default_conversation {
            configuration.validate().map_err(|_| {
                SettingsError::Invalid("invalid default conversation configuration")
            })?;
        }
        if self.loop_limits.absolute_deadline.is_some() {
            return Err(SettingsError::Invalid(
                "recurring host defaults cannot contain an absolute execution deadline",
            ));
        }
        self.loop_limits
            .validate()
            .map_err(|_| SettingsError::Invalid("invalid native execution limits"))?;
        ContextBuilder::new(ContextBudget {
            max_context_tokens: self.context_tokens,
            reserved_output_tokens: self
                .default_conversation
                .as_ref()
                .map_or(4096, |configuration| configuration.max_output_tokens),
            safety_margin_tokens: 1024,
            max_request_bytes: 32 * 1024 * 1024,
            max_compaction_bytes: 16 * 1024,
        })
        .map_err(|_| SettingsError::Invalid("invalid context budget"))?;
        if self.model_credentials.len() > 16 {
            return Err(SettingsError::Invalid(
                "at most sixteen model credential bindings are allowed",
            ));
        }
        for (index, binding) in self.model_credentials.iter().enumerate() {
            binding.validate()?;
            if self.model_credentials[..index].iter().any(|previous| {
                previous.provider == binding.provider && previous.endpoint == binding.endpoint
            }) {
                return Err(SettingsError::Invalid(
                    "duplicate provider endpoint credential binding",
                ));
            }
        }
        self.memory
            .validate()
            .map_err(|_| SettingsError::Invalid("invalid memory configuration"))?;
        self.mcp
            .validate()
            .map_err(|_| SettingsError::Invalid("invalid MCP configuration"))?;
        self.selection
            .configuration()
            .map_err(|_| SettingsError::Invalid("invalid optional selection configuration"))?;
        let encoded = serde_json::to_vec(self)
            .map_err(|_| SettingsError::Invalid("settings could not be encoded"))?;
        if encoded.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsError::Invalid("settings exceed 64 KiB"));
        }
        Ok(())
    }

    pub fn credential_for(
        &self,
        configuration: &ConversationConfig,
    ) -> Option<&ModelCredentialReference> {
        self.model_credentials.iter().find(|binding| {
            binding.provider == configuration.provider && binding.endpoint == configuration.endpoint
        })
    }
}

#[derive(Clone)]
pub struct SettingsRepository {
    lease: Arc<StateLease>,
    current: Arc<Mutex<HostSettings>>,
}

impl SettingsRepository {
    pub fn open(lease: Arc<StateLease>) -> Result<Self, SettingsError> {
        let current = match read_settings(&lease)? {
            Some(settings) => settings,
            None => {
                let workspace_root = lease.path().join("workspace");
                crate::service::state::StateDirectory::open(&workspace_root, true)?;
                let settings = HostSettings {
                    schema_version: SchemaVersion::CURRENT,
                    revision: 1,
                    default_conversation: None,
                    workspace_root,
                    execution_mode: ExecutionMode::ReadOnly,
                    tools_enabled: false,
                    context_tokens: 128_000,
                    loop_limits: LoopLimits::default(),
                    model_credentials: Vec::new(),
                    relay_origin: None,
                    worker: None,
                    memory: Default::default(),
                    mcp: Default::default(),
                    selection: Default::default(),
                };
                settings.validate()?;
                write_settings(&lease, &settings)?;
                settings
            }
        };
        current.validate()?;
        Ok(Self {
            lease,
            current: Arc::new(Mutex::new(current)),
        })
    }

    pub fn snapshot(&self) -> Result<HostSettings, SettingsError> {
        Ok(self
            .current
            .lock()
            .map_err(|_| SettingsError::Poisoned)?
            .clone())
    }

    pub fn replace(
        &self,
        mut settings: HostSettings,
        expected_revision: u64,
    ) -> Result<HostSettings, SettingsError> {
        let mut current = self.current.lock().map_err(|_| SettingsError::Poisoned)?;
        if current.revision != expected_revision || settings.revision != expected_revision {
            return Err(SettingsError::Conflict);
        }
        let disk = read_settings(&self.lease)?.ok_or(SettingsError::Conflict)?;
        if serde_json::to_value(&disk)
            .map_err(|_| SettingsError::Invalid("settings encoding failed"))?
            != serde_json::to_value(&*current)
                .map_err(|_| SettingsError::Invalid("settings encoding failed"))?
        {
            return Err(SettingsError::Conflict);
        }
        settings.revision = expected_revision
            .checked_add(1)
            .ok_or(SettingsError::Invalid("settings revision overflow"))?;
        settings.validate()?;
        write_settings(&self.lease, &settings)?;
        *current = settings.clone();
        Ok(settings)
    }
}

fn read_settings(lease: &StateLease) -> Result<Option<HostSettings>, SettingsError> {
    let descriptor = match rustix::fs::openat(
        &lease.directory.file,
        SETTINGS_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_file(lease, &descriptor)?;
    let mut bytes = Vec::new();
    File::from(descriptor)
        .take((MAX_SETTINGS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::Invalid("settings exceed 64 KiB"));
    }
    let settings = serde_json::from_slice(&bytes)
        .map_err(|_| SettingsError::Invalid("unsupported or malformed settings"))?;
    Ok(Some(settings))
}

fn validate_file(
    lease: &StateLease,
    descriptor: &impl std::os::fd::AsFd,
) -> Result<(), SettingsError> {
    let metadata = rustix::fs::fstat(descriptor)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != lease.directory.uid
        || metadata.st_mode & 0o7777 != 0o600
        || metadata.st_nlink != 1
    {
        return Err(SettingsError::Invalid(
            "settings must be an owner-only single-link regular file",
        ));
    }
    Ok(())
}

fn write_settings(lease: &Arc<StateLease>, settings: &HostSettings) -> Result<(), SettingsError> {
    if let Some(existing) = match rustix::fs::openat(
        &lease.directory.file,
        SETTINGS_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(file) => Some(file),
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => return Err(error.into()),
    } {
        validate_file(lease, &existing)?;
    }
    let temporary = format!(".host-settings-{}.tmp", Uuid::new_v4());
    let file = rustix::fs::openat(
        &lease.directory.file,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let _cleanup = TemporarySettings {
        lease: Arc::clone(lease),
        name: temporary.clone(),
    };
    let mut file = File::from(file);
    let bytes = serde_json::to_vec(settings)
        .map_err(|_| SettingsError::Invalid("settings encoding failed"))?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::Invalid("settings exceed 64 KiB"));
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    rustix::fs::renameat(
        &lease.directory.file,
        &temporary,
        &lease.directory.file,
        SETTINGS_FILE,
    )?;
    lease.directory.file.sync_all()?;
    Ok(())
}

struct TemporarySettings {
    lease: Arc<StateLease>,
    name: String,
}
impl Drop for TemporarySettings {
    fn drop(&mut self) {
        match rustix::fs::unlinkat(&self.lease.directory.file, &self.name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => {
                tracing::warn!(error = %error, "could not remove temporary host settings")
            }
        }
    }
}

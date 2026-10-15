use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::McpError;

#[derive(Clone, Debug)]
pub struct McpLimits {
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub cleanup_timeout: Duration,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            cleanup_timeout: Duration::from_secs(5),
        }
    }
}

impl McpLimits {
    pub(super) fn validate(&self) -> Result<(), McpError> {
        for duration in [self.startup_timeout, self.request_timeout] {
            if duration < Duration::from_millis(1) || duration > Duration::from_secs(300) {
                return Err(McpError::Configuration(
                    "deadlines require 1 ms through 300 seconds",
                ));
            }
        }
        if self.cleanup_timeout < Duration::from_millis(100)
            || self.cleanup_timeout > Duration::from_secs(10)
        {
            return Err(McpError::Configuration(
                "cleanup requires 100 ms through 10 seconds",
            ));
        }
        Ok(())
    }
}

pub struct McpStdioConfig {
    pub(super) server_id: String,
    pub(super) executable: PathBuf,
    pub(super) working_directory: PathBuf,
    pub(super) arguments: Vec<Zeroizing<String>>,
    pub(super) environment: BTreeMap<String, Zeroizing<String>>,
    pub(super) limits: McpLimits,
}

impl fmt::Debug for McpStdioConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpStdioConfig")
            .field("server_id", &self.server_id)
            .field("argument_count", &self.arguments.len())
            .field(
                "environment_keys",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl McpStdioConfig {
    pub fn new(
        server_id: impl Into<String>,
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
    ) -> Result<Self, McpError> {
        let server_id = server_id.into();
        if server_id.is_empty()
            || server_id.len() > 20
            || !server_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(McpError::Configuration(
                "server ID requires 1 to 20 ASCII letters, digits, underscores or hyphens",
            ));
        }
        let executable = executable.into();
        let working_directory = working_directory.into();
        validate_path(&executable)?;
        validate_path(&working_directory)?;
        Ok(Self {
            server_id,
            executable,
            working_directory,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            limits: McpLimits::default(),
        })
    }

    pub fn with_arguments(mut self, arguments: Vec<String>) -> Result<Self, McpError> {
        if arguments.len() > 64 || arguments.iter().map(String::len).sum::<usize>() > 32 * 1024 {
            return Err(McpError::Configuration(
                "arguments exceed 64 entries or 32 KiB",
            ));
        }
        if arguments.iter().any(|value| value.contains('\0')) {
            return Err(McpError::Configuration("arguments cannot contain NUL"));
        }
        self.arguments = arguments.into_iter().map(Zeroizing::new).collect();
        Ok(self)
    }

    pub fn with_environment(
        mut self,
        environment: BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        if environment.len() > 64
            || environment
                .iter()
                .map(|(key, value)| key.len().saturating_add(value.len()))
                .sum::<usize>()
                > 32 * 1024
        {
            return Err(McpError::Configuration(
                "environment exceeds 64 entries or 32 KiB",
            ));
        }
        for (key, value) in &environment {
            if key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || key.as_bytes()[0].is_ascii_digit()
                || value.contains('\0')
            {
                return Err(McpError::Configuration(
                    "invalid explicit environment entry",
                ));
            }
        }
        self.environment = environment
            .into_iter()
            .map(|(key, value)| (key, Zeroizing::new(value)))
            .collect();
        Ok(self)
    }

    pub fn with_limits(mut self, limits: McpLimits) -> Result<Self, McpError> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }

    pub(super) fn prepare(mut self) -> Result<Self, McpError> {
        self.limits.validate()?;
        self.executable = self
            .executable
            .canonicalize()
            .map_err(|_| McpError::Configuration("executable cannot be resolved"))?;
        self.working_directory = self
            .working_directory
            .canonicalize()
            .map_err(|_| McpError::Configuration("working directory cannot be resolved"))?;
        if !self.executable.is_file() || !self.working_directory.is_dir() {
            return Err(McpError::Configuration(
                "executable must be a file and working directory must be a directory",
            ));
        }
        Ok(self)
    }

    pub(super) fn fingerprint(&self) -> Result<String, McpError> {
        let mut digest = Sha256::new();
        digest.update(b"everything.mcp.launch.v1\0");
        let mut field = |value: &[u8]| {
            digest.update((value.len() as u64).to_le_bytes());
            digest.update(value);
        };
        field(self.server_id.as_bytes());
        field(
            self.executable
                .to_str()
                .ok_or(McpError::Configuration("executable must be UTF-8"))?
                .as_bytes(),
        );
        field(
            self.working_directory
                .to_str()
                .ok_or(McpError::Configuration("working directory must be UTF-8"))?
                .as_bytes(),
        );
        field(&(self.arguments.len() as u64).to_le_bytes());
        for value in &self.arguments {
            field(value.as_bytes());
        }
        field(&(self.environment.len() as u64).to_le_bytes());
        for (key, value) in &self.environment {
            field(key.as_bytes());
            field(value.as_bytes());
        }
        Ok(format!("{:x}", digest.finalize()))
    }
}

fn validate_path(path: &Path) -> Result<(), McpError> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|value| value.len() > 4096 || value.contains('\0'))
    {
        return Err(McpError::Configuration(
            "paths must be absolute UTF-8 and at most 4096 bytes",
        ));
    }
    Ok(())
}

use sha2::{Digest, Sha256};

use super::super::operations::{
    SettingsFileFingerprint, SettingsWriteIntent, SettingsWriteResolution,
};
use super::super::{HostSettings, MAX_SETTINGS_BYTES, SettingsRepository, read_settings};
use super::SettingsServiceError;

pub(super) struct PreparedSettingsFile {
    pub before: SettingsFileFingerprint,
    pub after: SettingsFileFingerprint,
    pub candidate: HostSettings,
}

pub(super) struct SettingsFileObservation {
    pub resolution: SettingsWriteResolution,
    pub settings: HostSettings,
}

impl SettingsRepository {
    pub(super) fn prepare_file(
        &self,
        candidate: HostSettings,
        expected_revision: u64,
    ) -> Result<PreparedSettingsFile, SettingsServiceError> {
        let current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        let before = file_fingerprint(&actual)?;
        if before != file_fingerprint(&current)?
            || before.revision != expected_revision
            || candidate.revision != expected_revision
            || expected_revision >= i64::MAX as u64
        {
            return Err(SettingsServiceError::Conflict);
        }
        let mut post = candidate.clone();
        post.revision = expected_revision + 1;
        post.validate()?;
        let after = file_fingerprint(&post)?;
        Ok(PreparedSettingsFile {
            before,
            after,
            candidate,
        })
    }

    pub(super) fn observe_file(
        &self,
        intent: &SettingsWriteIntent,
    ) -> Result<Option<SettingsFileObservation>, SettingsServiceError> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        let fingerprint = file_fingerprint(&actual)?;
        let resolution = if fingerprint == intent.before {
            SettingsWriteResolution::NotApplied
        } else if fingerprint == intent.after {
            SettingsWriteResolution::Applied
        } else {
            return Ok(None);
        };
        *current = actual.clone();
        Ok(Some(SettingsFileObservation {
            resolution,
            settings: actual,
        }))
    }

    pub(super) fn observed_snapshot(&self) -> Result<HostSettings, SettingsServiceError> {
        let current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        if file_fingerprint(&actual)? != file_fingerprint(&current)? {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(actual)
    }
}

pub(super) fn file_fingerprint(
    settings: &HostSettings,
) -> Result<SettingsFileFingerprint, SettingsServiceError> {
    let bytes = serde_json::to_vec(settings).map_err(|_| SettingsServiceError::Encoding)?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsServiceError::Capacity);
    }
    let mut digest = Sha256::new();
    digest.update(b"everything.settings.file.v1\0");
    digest.update(&bytes);
    Ok(SettingsFileFingerprint {
        revision: settings.revision,
        sha256: format!("{:x}", digest.finalize()),
    })
}

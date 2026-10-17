use std::fmt;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::crypto::DevicePublicKeys;
use crate::devices::{DeviceAccess, DeviceError};
use crate::storage::StorageError;

use super::{SettingsReceiptAuthority, SettingsServiceError, original_reply_text};

const MAX_REPLY_PIN_BYTES: usize = 16 * 1024;

#[derive(Error)]
pub(super) enum ReplyPinError {
    #[error("Settings reply current pin validation failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings reply pin metadata validation failed")]
    Metadata(#[source] serde_json::Error),
}

impl From<SettingsServiceError> for ReplyPinError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl From<ReplyPinError> for SettingsServiceError {
    fn from(error: ReplyPinError) -> Self {
        match error {
            ReplyPinError::Authority(cause) => *cause,
            ReplyPinError::Metadata(cause) => Self::Storage(StorageError::Json(cause)),
        }
    }
}

impl fmt::Debug for ReplyPinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReplyPinError([retained cause])")
    }
}

impl SettingsReceiptAuthority {
    pub(super) fn check_reply_pins_on(
        &self,
        connection: &Connection,
        expected_peer_keys: &DevicePublicKeys,
        expected_registration: &Value,
    ) -> Result<(), ReplyPinError> {
        let mut statement = connection.prepare("SELECT device_id,configuration,revoked_at FROM transport_endpoint WHERE singleton=1")
            .map_err(SettingsServiceError::from)?;
        let mut rows = statement.query([]).map_err(SettingsServiceError::from)?;
        let row = rows
            .next()
            .map_err(SettingsServiceError::from)?
            .ok_or(SettingsServiceError::NotFound)?;
        let configuration: ReplyEndpointConfiguration =
            serde_json::from_str(original_reply_text(row, 1, MAX_REPLY_PIN_BYTES)?)
                .map_err(ReplyPinError::Metadata)?;
        if original_reply_text(row, 0, 36)? != self.core.authority.host_id.to_string()
            || configuration.version != 1
            || configuration.keys != self.core.endpoint.public_keys()
            || !matches!(configuration.role, ReplyEndpointRole::Host)
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if row
            .get::<_, Option<i64>>(2)
            .map_err(SettingsServiceError::from)?
            .is_some()
        {
            return Err(SettingsServiceError::Device(DeviceError::EndpointRevoked).into());
        }
        drop(rows);
        drop(statement);
        let mut statement = connection
            .prepare("SELECT public_identity,revoked_at FROM devices WHERE id=?1")
            .map_err(SettingsServiceError::from)?;
        let mut rows = statement
            .query([expected_peer_keys.device_id.to_string()])
            .map_err(SettingsServiceError::from)?;
        let row = rows
            .next()
            .map_err(SettingsServiceError::from)?
            .ok_or(SettingsServiceError::NotFound)?;
        let peer: ReplyPinnedPeer =
            serde_json::from_str(original_reply_text(row, 0, MAX_REPLY_PIN_BYTES)?)
                .map_err(ReplyPinError::Metadata)?;
        peer.keys
            .validate()
            .map_err(DeviceError::from)
            .map_err(SettingsServiceError::from)?;
        peer.access.validate().map_err(SettingsServiceError::from)?;
        if peer.version != 1
            || peer.keys != *expected_peer_keys
            || serde_json::to_value(&peer).map_err(ReplyPinError::Metadata)?
                != *expected_registration
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if row
            .get::<_, Option<i64>>(1)
            .map_err(SettingsServiceError::from)?
            .is_some()
        {
            return Err(SettingsServiceError::Storage(StorageError::RevokedDevice(
                peer.keys.device_id.to_string(),
            ))
            .into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyEndpointConfiguration {
    version: u16,
    keys: DevicePublicKeys,
    role: ReplyEndpointRole,
}

#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
enum ReplyEndpointRole {
    Host,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyPinnedPeer {
    version: u16,
    keys: DevicePublicKeys,
    access: DeviceAccess,
}

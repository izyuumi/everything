use super::{DeviceEndpoint, DeviceError, EndpointRole, PeerRegistration, REGISTRATION_VERSION};
use crate::crypto::DevicePublicKeys;
use crate::protocol::{DeviceId, MessageId, UnixMillis};
use crate::storage::StorageError;
use rusqlite::types::ValueRef;
use serde::Serialize;

pub const MAX_DEVICE_SETTINGS_PAGE: usize = 100;
pub const MAX_DEVICE_SETTINGS_PAGE_BYTES: usize = 28 * 1024;
const MAX_REGISTRATION_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceSettingsSummary {
    pub device_id: DeviceId,
    pub label: String,
    pub public_keys: DevicePublicKeys,
    pub revoked: bool,
    pub this_device: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceSettingsPage {
    pub host_id: DeviceId,
    pub observed_at: UnixMillis,
    pub devices: Vec<DeviceSettingsSummary>,
    pub next_after: Option<DeviceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingCancellation {
    Cancelled,
    Expired,
    NotPending,
}

impl DeviceEndpoint {
    pub fn settings_devices(
        &self,
        after: Option<DeviceId>,
        limit: usize,
        observed_at: UnixMillis,
    ) -> Result<DeviceSettingsPage, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        if limit == 0 || limit > MAX_DEVICE_SETTINGS_PAGE {
            return Err(DeviceError::Invalid("invalid settings device page limit"));
        }
        super::time(observed_at)?;
        let host_id = self.public_keys().device_id;
        let mut page = DeviceSettingsPage {
            host_id,
            observed_at,
            devices: Vec::with_capacity(limit),
            next_after: Some(host_id),
        };
        let mut encoded_bytes = serde_json::to_vec(&page)?.len();
        page.next_after = None;
        let connection = self.inner.store.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, label, public_identity, created_at, revoked_at FROM devices WHERE id > ?1 ORDER BY id LIMIT ?2",
            )
            .map_err(StorageError::from)?;
        let cursor = after.map(|id| id.to_string()).unwrap_or_default();
        let mut rows = statement
            .query(rusqlite::params![cursor, (limit + 1) as i64])
            .map_err(StorageError::from)?;
        for index in 0..=limit {
            let Some(row) = rows.next().map_err(StorageError::from)? else {
                break;
            };
            if index == limit {
                page.next_after = page.devices.last().map(|device| device.device_id);
                break;
            }
            let summary = summary(row, host_id)?;
            let candidate_bytes = encoded_bytes
                .checked_add(serde_json::to_vec(&summary)?.len())
                .and_then(|size| size.checked_add(usize::from(!page.devices.is_empty())))
                .ok_or(DeviceError::Invalid("settings device page exceeds bound"))?;
            if candidate_bytes > MAX_DEVICE_SETTINGS_PAGE_BYTES {
                page.next_after = Some(
                    page.devices
                        .last()
                        .ok_or(DeviceError::Invalid("settings device row exceeds bound"))?
                        .device_id,
                );
                break;
            }
            encoded_bytes = candidate_bytes;
            page.devices.push(summary);
        }
        if serde_json::to_vec(&page)?.len() > MAX_DEVICE_SETTINGS_PAGE_BYTES {
            return Err(DeviceError::Invalid("settings device page exceeds bound"));
        }
        Ok(page)
    }

    pub fn cancel_pairing(
        &self,
        pairing_id: MessageId,
        now: UnixMillis,
    ) -> Result<PairingCancellation, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        let pending = self
            .inner
            .pairings
            .lock()
            .map_err(|_| DeviceError::Poisoned)?
            .remove(&pairing_id);
        Ok(match pending {
            Some(pending) if pending.invitation().expires_at_ms <= now.get() => {
                PairingCancellation::Expired
            }
            Some(_) => PairingCancellation::Cancelled,
            None => PairingCancellation::NotPending,
        })
    }
}

fn summary(
    row: &rusqlite::Row<'_>,
    host_id: DeviceId,
) -> Result<DeviceSettingsSummary, DeviceError> {
    let id = text(row, 0, 36)?;
    let device_id: DeviceId = id.parse().map_err(|_| DeviceError::MalformedRegistration)?;
    let label = text(row, 1, 256)?;
    super::validate_label(label)?;
    let registration: PeerRegistration =
        serde_json::from_str(text(row, 2, MAX_REGISTRATION_BYTES)?)
            .map_err(|_| DeviceError::MalformedRegistration)?;
    if id != device_id.to_string()
        || registration.version != REGISTRATION_VERSION
        || registration.keys.device_id != device_id
    {
        return Err(DeviceError::MalformedRegistration);
    }
    registration.keys.validate()?;
    registration.access.validate()?;
    let created_at: i64 = row.get(3).map_err(StorageError::from)?;
    let revoked_at: Option<i64> = row.get(4).map_err(StorageError::from)?;
    if created_at < 0 || revoked_at.is_some_and(|revoked| revoked < created_at) {
        return Err(DeviceError::MalformedRegistration);
    }
    Ok(DeviceSettingsSummary {
        device_id,
        label: label.to_owned(),
        public_keys: registration.keys,
        revoked: revoked_at.is_some(),
        this_device: device_id == host_id,
    })
}

fn text<'row>(
    row: &'row rusqlite::Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, DeviceError> {
    let ValueRef::Text(bytes) = row.get_ref(column).map_err(StorageError::from)? else {
        return Err(DeviceError::MalformedRegistration);
    };
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(DeviceError::MalformedRegistration);
    }
    std::str::from_utf8(bytes).map_err(|_| DeviceError::MalformedRegistration)
}

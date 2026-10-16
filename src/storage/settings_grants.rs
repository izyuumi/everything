use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::DevicePublicKeys;
use crate::devices::DeviceAccess;
use crate::host::settings::grants::{
    MAX_SETTINGS_GRANT_BYTES, MAX_SETTINGS_GRANT_RECEIPT_BYTES,
    SETTINGS_GRANT_REVOKE_RESERVE_BYTES, SettingsGrantChange, SettingsGrantCommit,
    SettingsGrantHead, SettingsGrantOwner, SettingsGrantReceipt, SettingsGrantReference,
    SettingsGrantState, SettingsGrantVersion,
};
use crate::protocol::{CommandId, DeviceId, UnixMillis};

use super::{StorageError, StorageResult, Store, invalid};

const MAX_GRANT_HEADS: i64 = 4096;
const MAX_GRANT_VERSIONS: i64 = 8192;
const MAX_GRANT_VERSION_BYTES: i64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointConfiguration {
    version: u16,
    keys: DevicePublicKeys,
    role: StoredEndpointRole,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEndpointRole {
    role: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedPeer {
    version: u16,
    keys: DevicePublicKeys,
    access: DeviceAccess,
}

impl Store {
    pub(crate) fn set_settings_grant(
        &self,
        owner: SettingsGrantOwner,
        change: &SettingsGrantChange,
        now: UnixMillis,
    ) -> StorageResult<SettingsGrantCommit> {
        validate_owner(owner)?;
        change.validate().map_err(contract_error)?;
        let canonical_change = encode(change, MAX_SETTINGS_GRANT_BYTES)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let host_revoked = host_state(&transaction, owner)?;
        if let Some((receipt, original)) = read_receipt(&transaction, owner, change.request_id)? {
            if original.change != *change
                || encode(&original.change, MAX_SETTINGS_GRANT_BYTES)? != canonical_change
            {
                return conflict();
            }
            return Ok(SettingsGrantCommit {
                receipt,
                replayed: true,
            });
        }
        let current = read_head(&transaction, owner, change.peer_keys.device_id)?;
        let expected = current.as_ref().map(|grant| SettingsGrantReference {
            grant_id: grant.change.target.grant_id,
            revision: grant.revision,
        });
        if expected != change.expected {
            return conflict();
        }
        if current
            .as_ref()
            .is_some_and(|grant| grant.change.peer_keys != change.peer_keys)
        {
            return invalid("Settings grant head pin cannot change");
        }
        let reducing =
            current.is_some() && matches!(change.target.state, SettingsGrantState::Revoked);
        let peer_revoked = peer_state(&transaction, &change.peer_keys)?;
        if !reducing && (host_revoked || peer_revoked) {
            return invalid("Settings grants require an active Host and peer pin");
        }
        if now < change.target.issued_at || now >= change.target.expires_at {
            return invalid("Settings grant is not current at admission");
        }
        if current.is_none() || change.rotate {
            let used: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings_grant_versions WHERE host_id=?1 AND grant_id=?2)",
                params![owner.host_id.to_string(), change.target.grant_id.to_string()],
                |row| row.get(0),
            )?;
            if used {
                return conflict();
            }
        }
        let revision = expected
            .map_or(Some(1), |expected| expected.revision.checked_add(1))
            .filter(|revision| *revision <= i64::MAX as u64)
            .ok_or_else(|| {
                StorageError::InvalidInput("Settings grant revision exhausted".into())
            })?;
        let version = SettingsGrantVersion {
            version: 1,
            host_id: owner.host_id,
            owner_principal_id: owner.principal_id,
            revision,
            change: change.clone(),
        };
        version.validate().map_err(contract_error)?;
        let encoded = encode(&version, MAX_SETTINGS_GRANT_BYTES)?;
        let had_reserve = current
            .as_ref()
            .is_some_and(|grant| active(&grant.change.target.state));
        let needs_reserve = active(&change.target.state);
        if had_reserve && !needs_reserve && encoded.len() > SETTINGS_GRANT_REVOKE_RESERVE_BYTES {
            return invalid("Settings grant revocation exceeds its reserved bytes");
        }
        let receipt = SettingsGrantReceipt {
            version: 1,
            host_id: owner.host_id,
            device_id: change.peer_keys.device_id,
            owner_request_id: change.request_id,
            grant_id: change.target.grant_id,
            revision,
            request_sha256: change_digest(&canonical_change),
            committed_at: now,
        };
        let encoded_receipt = encode(&receipt, MAX_SETTINGS_GRANT_RECEIPT_BYTES)?;
        check_capacity(
            &transaction,
            current.is_none(),
            had_reserve,
            needs_reserve,
            encoded.len(),
            encoded_receipt.len(),
        )?;
        transaction.execute(
            "INSERT INTO settings_grant_versions(host_id,device_id,revision,grant_id,owner_request_id,record,receipt) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![owner.host_id.to_string(), change.peer_keys.device_id.to_string(), revision as i64, change.target.grant_id.to_string(), change.request_id.to_string(), encoded, encoded_receipt],
        )?;
        transaction.commit()?;
        Ok(SettingsGrantCommit {
            receipt,
            replayed: false,
        })
    }

    pub(crate) fn settings_grant_head(
        &self,
        owner: SettingsGrantOwner,
        device_id: DeviceId,
    ) -> StorageResult<Option<SettingsGrantHead>> {
        validate_owner(owner)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let host_revoked = host_state(&transaction, owner)?;
        let head = read_head(&transaction, owner, device_id)?
            .map(|grant| -> StorageResult<SettingsGrantHead> {
                let peer_revoked = peer_state(&transaction, &grant.change.peer_keys)?;
                Ok(SettingsGrantHead {
                    grant,
                    peer_revoked,
                    host_revoked,
                })
            })
            .transpose()?;
        transaction.commit()?;
        Ok(head)
    }

    pub(crate) fn settings_grant_version(
        &self,
        owner: SettingsGrantOwner,
        device_id: DeviceId,
        revision: u64,
    ) -> StorageResult<Option<SettingsGrantVersion>> {
        validate_owner(owner)?;
        if revision == 0 || revision > i64::MAX as u64 {
            return invalid("invalid Settings grant revision");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        host_state(&transaction, owner)?;
        let encoded: Option<String> = transaction
            .query_row(
                "SELECT record FROM settings_grant_versions WHERE host_id=?1 AND device_id=?2 AND revision=?3",
                params![owner.host_id.to_string(), device_id.to_string(), revision as i64],
                |row| bounded_text(row, 0, MAX_SETTINGS_GRANT_BYTES),
            )
            .optional()?;
        let version = encoded
            .map(|encoded| {
                let version = decode_version(&encoded, owner)?;
                if version.revision != revision || version.change.peer_keys.device_id != device_id {
                    return invalid("Settings grant version identity mismatch");
                }
                Ok(version)
            })
            .transpose()?;
        transaction.commit()?;
        Ok(version)
    }

    pub(crate) fn settings_grant_receipt(
        &self,
        owner: SettingsGrantOwner,
        request_id: CommandId,
    ) -> StorageResult<Option<SettingsGrantReceipt>> {
        validate_owner(owner)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        host_state(&transaction, owner)?;
        let receipt = read_receipt(&transaction, owner, request_id)?.map(|(receipt, _)| receipt);
        transaction.commit()?;
        Ok(receipt)
    }
}

fn check_capacity(
    connection: &Connection,
    new_head: bool,
    had_reserve: bool,
    needs_reserve: bool,
    bytes: usize,
    receipt_bytes: usize,
) -> StorageResult<()> {
    let (versions, version_bytes, actual_receipt_bytes): (i64, i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0),coalesce(sum(length(CAST(receipt AS BLOB))),0) FROM settings_grant_versions",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let (heads, reserves): (i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(revocation_reserved),0) FROM settings_grant_heads",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let reserves = reserves - i64::from(had_reserve) + i64::from(needs_reserve);
    let total_versions = versions
        .checked_add(1)
        .and_then(|count| count.checked_add(reserves));
    let total_bytes = i64::try_from(bytes)
        .ok()
        .and_then(|bytes| version_bytes.checked_add(bytes))
        .and_then(|bytes| {
            reserves
                .checked_mul(SETTINGS_GRANT_REVOKE_RESERVE_BYTES as i64)
                .and_then(|reserved| bytes.checked_add(reserved))
        });
    let total_receipt_bytes = i64::try_from(receipt_bytes)
        .ok()
        .and_then(|bytes| actual_receipt_bytes.checked_add(bytes))
        .and_then(|bytes| {
            reserves
                .checked_mul(MAX_SETTINGS_GRANT_RECEIPT_BYTES as i64)
                .and_then(|reserved| bytes.checked_add(reserved))
        });
    if reserves < 0
        || heads + i64::from(new_head) > MAX_GRANT_HEADS
        || total_versions.is_none_or(|count| count > MAX_GRANT_VERSIONS)
        || total_bytes.is_none_or(|bytes| bytes > MAX_GRANT_VERSION_BYTES)
        || total_receipt_bytes.is_none_or(|bytes| {
            bytes > MAX_GRANT_VERSIONS * MAX_SETTINGS_GRANT_RECEIPT_BYTES as i64
        })
    {
        return Err(StorageError::TransportCapacity(
            "Settings grant history and revocation reserve",
        ));
    }
    Ok(())
}

fn read_head(
    connection: &Connection,
    owner: SettingsGrantOwner,
    device_id: DeviceId,
) -> StorageResult<Option<SettingsGrantVersion>> {
    let row: Option<(String, String, i64, i64)> = connection
        .query_row(
            "SELECT v.record,h.grant_id,h.revision,h.revocation_reserved FROM settings_grant_heads h LEFT JOIN settings_grant_versions v ON v.host_id=h.host_id AND v.device_id=h.device_id AND v.revision=h.revision WHERE h.host_id=?1 AND h.device_id=?2",
            params![owner.host_id.to_string(), device_id.to_string()],
            |row| Ok((bounded_text(row, 0, MAX_SETTINGS_GRANT_BYTES)?, bounded_text(row, 1, 36)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((encoded, grant_id, revision, reserved)) = row else {
        return Ok(None);
    };
    let version = decode_version(&encoded, owner)?;
    if version.change.peer_keys.device_id != device_id
        || version.change.target.grant_id.to_string() != grant_id
        || i64::try_from(version.revision).ok() != Some(revision)
        || reserved != i64::from(active(&version.change.target.state))
    {
        return invalid("Settings grant head identity mismatch");
    }
    Ok(Some(version))
}

fn read_receipt(
    connection: &Connection,
    owner: SettingsGrantOwner,
    request_id: CommandId,
) -> StorageResult<Option<(SettingsGrantReceipt, SettingsGrantVersion)>> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT record,receipt FROM settings_grant_versions WHERE host_id=?1 AND owner_request_id=?2",
            params![owner.host_id.to_string(), request_id.to_string()],
            |row| Ok((bounded_text(row, 0, MAX_SETTINGS_GRANT_BYTES)?, bounded_text(row, 1, MAX_SETTINGS_GRANT_RECEIPT_BYTES)?)),
        )
        .optional()?;
    let Some((encoded, receipt)) = row else {
        return Ok(None);
    };
    let version = decode_version(&encoded, owner)?;
    let decoded: SettingsGrantReceipt = serde_json::from_str(&receipt)?;
    if encode(&decoded, MAX_SETTINGS_GRANT_RECEIPT_BYTES)? != receipt
        || decoded.version != 1
        || decoded.host_id != owner.host_id
        || decoded.device_id != version.change.peer_keys.device_id
        || decoded.owner_request_id != request_id
        || version.change.request_id != request_id
        || decoded.grant_id != version.change.target.grant_id
        || decoded.revision != version.revision
        || decoded.request_sha256
            != change_digest(&encode(&version.change, MAX_SETTINGS_GRANT_BYTES)?)
        || decoded.committed_at < version.change.target.issued_at
        || decoded.committed_at >= version.change.target.expires_at
    {
        return invalid("Settings grant receipt identity mismatch");
    }
    Ok(Some((decoded, version)))
}

fn decode_version(encoded: &str, owner: SettingsGrantOwner) -> StorageResult<SettingsGrantVersion> {
    let version: SettingsGrantVersion = serde_json::from_str(encoded)?;
    version.validate().map_err(contract_error)?;
    if encode(&version, MAX_SETTINGS_GRANT_BYTES)? != encoded
        || version.host_id != owner.host_id
        || version.owner_principal_id != owner.principal_id
    {
        return invalid("Settings grant stored record mismatch");
    }
    Ok(version)
}

fn host_state(connection: &Connection, owner: SettingsGrantOwner) -> StorageResult<bool> {
    let (id, configuration, revoked): (String, String, Option<i64>) = connection.query_row(
        "SELECT device_id,configuration,revoked_at FROM transport_endpoint WHERE singleton=1",
        [],
        |row| {
            Ok((
                bounded_text(row, 0, 36)?,
                bounded_text(row, 1, 16384)?,
                row.get(2)?,
            ))
        },
    )?;
    let configuration: EndpointConfiguration = serde_json::from_str(&configuration)?;
    if configuration.version != 1
        || id != owner.host_id.to_string()
        || configuration.keys.device_id != owner.host_id
        || configuration.role.role != "host"
        || configuration.keys.validate().is_err()
    {
        return invalid("Settings grant requires the authoritative Host endpoint");
    }
    Ok(revoked.is_some())
}

fn peer_state(connection: &Connection, expected: &DevicePublicKeys) -> StorageResult<bool> {
    let (identity, revoked): (String, Option<i64>) = connection.query_row(
        "SELECT public_identity,revoked_at FROM devices WHERE id=?1",
        [expected.device_id.to_string()],
        |row| Ok((bounded_text(row, 0, 16384)?, row.get(1)?)),
    )?;
    let peer: PinnedPeer = serde_json::from_str(&identity)?;
    if peer.version != 1 || peer.keys != *expected || peer.keys.validate().is_err() {
        return invalid("Settings grant does not match the complete pinned peer");
    }
    peer.access.validate().map_err(|_| {
        StorageError::InvalidInput("invalid Settings grant peer registration".into())
    })?;
    Ok(revoked.is_some())
}

fn bounded_text(row: &Row<'_>, index: usize, maximum: usize) -> rusqlite::Result<String> {
    let ValueRef::Text(bytes) = row.get_ref(index)? else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(rusqlite::Error::InvalidQuery);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn encode(value: &impl Serialize, maximum: usize) -> StorageResult<String> {
    let encoded = serde_json::to_string(value)?;
    if encoded.len() > maximum {
        return invalid("Settings grant encoded record exceeds its bound");
    }
    Ok(encoded)
}

fn change_digest(canonical: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"everything.settings-grant.owner-change.v1\0");
    digest.update(canonical.as_bytes());
    format!("{:x}", digest.finalize())
}

fn validate_owner(owner: SettingsGrantOwner) -> StorageResult<()> {
    owner.validate().map_err(contract_error)
}

fn contract_error(_: crate::host::settings::contract::SettingsContractError) -> StorageError {
    StorageError::InvalidInput("invalid Settings grant contract".into())
}

fn conflict<T>() -> StorageResult<T> {
    Err(StorageError::StateConflict {
        entity: "Settings grant original request or head",
        id: "original owner CAS".into(),
    })
}

fn active(state: &SettingsGrantState) -> bool {
    matches!(state, SettingsGrantState::Active { .. })
}

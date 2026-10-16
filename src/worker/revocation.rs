use rusqlite::{Connection, OptionalExtension};

use crate::crypto::{DevicePublicKeys, EncryptedEnvelope};
use crate::protocol::{CommandId, DeviceId};

use super::{WORKER_PURPOSE, WorkerEndpoint, WorkerError, WorkerMessage, WorkerPacket, WorkerRole};

const MAX_REVOCATION_BYTES: i64 = 4 * 1024;

pub(super) fn retained_revocation(
    endpoint: &WorkerEndpoint,
    peer: DeviceId,
    request_id: CommandId,
) -> Result<bool, WorkerError> {
    let (message_id, packet_bytes, envelope_json, expires_at, recorded_at) = {
        let store = endpoint.agent.store();
        let connection = store.connection()?;
        let mut statement = connection.prepare(
            "SELECT message_id,peer_id,length(packet_json),length(CAST(envelope_json AS BLOB)),packet_json,envelope_json,expires_at,recorded_at,length(CAST(message_id AS BLOB)),length(CAST(peer_id AS BLOB)) FROM worker_messages WHERE direction='outbound' AND json_extract(CAST(packet_json AS TEXT),'$.body.request_id')=?1 LIMIT 2",
        )?;
        let mut rows = statement.query([request_id.to_string()])?;
        let Some(row) = rows.next()? else {
            return Ok(false);
        };
        let message_length: i64 = row.get(8)?;
        let peer_length: i64 = row.get(9)?;
        if message_length != 36 || peer_length != 36 {
            return Err(WorkerError::Conflict("retained revocation identity size"));
        }
        let packet_length: i64 = row.get(2)?;
        let envelope_length: i64 = row.get(3)?;
        if !(1..=MAX_REVOCATION_BYTES).contains(&packet_length)
            || !(1..=MAX_REVOCATION_BYTES).contains(&envelope_length)
            || packet_length + envelope_length > MAX_REVOCATION_BYTES
        {
            return Err(WorkerError::Conflict("retained revocation size"));
        }
        let message_id: String = row.get(0)?;
        let saved_peer: String = row.get(1)?;
        if message_id.len() != 36 || saved_peer != peer.to_string() {
            return Err(WorkerError::Conflict(
                "retained revocation peer or identity",
            ));
        }
        let packet_bytes: Vec<u8> = row.get(4)?;
        let envelope_json: String = row.get(5)?;
        let expires_at: i64 = row.get(6)?;
        let recorded_at: i64 = row.get(7)?;
        if recorded_at < 0 || expires_at <= recorded_at {
            return Err(WorkerError::Conflict("retained revocation timestamp"));
        }
        if rows.next()?.is_some() {
            return Err(WorkerError::Conflict(
                "ambiguous retained revocation request",
            ));
        }
        drop(rows);
        drop(statement);
        validate_endpoint(&connection, endpoint)?;
        let peer_state: Option<(i64, i64, Option<i64>)> = connection
            .query_row(
                "SELECT length(CAST(keys_json AS BLOB)),pinned_at,revoked_at FROM worker_peers WHERE device_id=?1",
                [peer.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((key_length, pinned_at, revoked_at)) = peer_state else {
            return Err(WorkerError::UntrustedPeer);
        };
        if !(1..=2048).contains(&key_length)
            || pinned_at < 0
            || pinned_at > recorded_at
            || revoked_at != Some(recorded_at)
        {
            return Err(WorkerError::Conflict("retained revocation trust state"));
        }
        (
            message_id,
            packet_bytes,
            envelope_json,
            expires_at,
            recorded_at,
        )
    };
    let peer_keys = endpoint.repository.delivery_peer(peer, true)?;
    if peer_keys.device_id != peer {
        return Err(WorkerError::Conflict("retained revocation peer keys"));
    }
    let packet = WorkerPacket::parse(&packet_bytes)?;
    let WorkerMessage::Revoke {
        request_id: original_request,
        revoked_at,
    } = &packet.body
    else {
        return Err(WorkerError::Conflict("worker request identity was reused"));
    };
    let local = endpoint.public_keys();
    if *original_request != request_id
        || revoked_at.get() != recorded_at as u64
        || packet.purpose != WORKER_PURPOSE
        || packet.sender_role != endpoint.role()
        || packet.sender != local.device_id
        || packet.recipient != peer
        || packet.to_bytes()? != packet_bytes
    {
        return Err(WorkerError::Conflict("retained revocation packet binding"));
    }
    let envelope: EncryptedEnvelope = serde_json::from_str(&envelope_json)?;
    if envelope.header.message_id.to_string() != message_id
        || envelope.header.sender != local.device_id
        || envelope.header.recipient != peer
        || envelope.header.created_at_ms != recorded_at as u64
        || envelope.header.expires_at_ms != expires_at as u64
        || serde_json::to_string(&envelope)? != envelope_json
    {
        return Err(WorkerError::Conflict(
            "retained revocation envelope binding",
        ));
    }
    envelope.verify_authenticated_sender(&local, recorded_at as u64)?;
    Ok(true)
}

fn validate_endpoint(
    connection: &Connection,
    endpoint: &WorkerEndpoint,
) -> Result<(), WorkerError> {
    let mut statement = connection.prepare(
        "SELECT length(CAST(identity_json AS BLOB)),length(CAST(origin AS BLOB)),identity_json,role,origin FROM worker_endpoint WHERE singleton=1",
    )?;
    let mut rows = statement.query([])?;
    let row = rows
        .next()?
        .ok_or(WorkerError::Conflict("retained worker endpoint is missing"))?;
    let identity_length: i64 = row.get(0)?;
    let origin_length: i64 = row.get(1)?;
    if !(1..=2048).contains(&identity_length) || !(1..=512).contains(&origin_length) {
        return Err(WorkerError::Conflict("retained worker endpoint size"));
    }
    let identity_json: String = row.get(2)?;
    let role: String = row.get(3)?;
    let origin: String = row.get(4)?;
    let keys: DevicePublicKeys = serde_json::from_str(&identity_json)?;
    keys.validate()?;
    let expected_role = match endpoint.role() {
        WorkerRole::Coordinator => "coordinator",
        WorkerRole::Executor => "executor",
    };
    if keys != endpoint.public_keys()
        || serde_json::to_string(&keys)? != identity_json
        || role != expected_role
        || origin != endpoint.origin().as_str()
    {
        return Err(WorkerError::Conflict("retained worker endpoint binding"));
    }
    Ok(())
}

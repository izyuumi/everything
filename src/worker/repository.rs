use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{
    crypto::{DevicePublicKeys, EncryptedEnvelope},
    protocol::{DeviceId, MessageId, UnixMillis},
    relay_protocol::RelayOrigin,
    storage::Store,
};

use super::{
    ApprovedDelegation, WorkerError, WorkerJobBinding, WorkerManifest, WorkerMessage,
    WorkerModelBinding, WorkerPacket, WorkerRejection, WorkerResult, WorkerRole,
};

const MAX_PEERS: i64 = 16;
const MAX_JOBS: i64 = 256;
const MAX_MESSAGES: i64 = 8_192;
const MAX_LEDGER_BYTES: i64 = 64 * 1_024 * 1_024;
const REVOCATION_RESERVE_BYTES: i64 = 64 * 1_024;
const MAX_REVOCATION_RECORD_BYTES: usize = 4 * 1_024;
const JOB_RESERVE_BYTES: i64 = 2 * 491_520 + 32_768 + REVOCATION_RESERVE_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPeerState {
    pub keys: DevicePublicKeys,
    pub pinned_at: UnixMillis,
    pub revoked_at: Option<UnixMillis>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationState {
    Offered,
    Accepted,
    Running,
    CancelRequested,
    ResultReady,
    Acknowledged,
    Rejected,
    Revoked,
}

impl DelegationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Acknowledged | Self::Rejected | Self::Revoked)
    }

    fn parse(value: &str) -> Result<Self, WorkerError> {
        match value {
            "offered" => Ok(Self::Offered),
            "accepted" => Ok(Self::Accepted),
            "running" => Ok(Self::Running),
            "cancel_requested" => Ok(Self::CancelRequested),
            "result_ready" => Ok(Self::ResultReady),
            "acknowledged" => Ok(Self::Acknowledged),
            "rejected" => Ok(Self::Rejected),
            "revoked" => Ok(Self::Revoked),
            _ => Err(WorkerError::Conflict("stored delegation state")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelegationSnapshot {
    pub binding: WorkerJobBinding,
    pub peer: DeviceId,
    pub manifest: Option<WorkerManifest>,
    pub state: DelegationState,
    pub cancellation_requested: bool,
    pub cancellation_command_id: Option<crate::protocol::CommandId>,
    pub cancellation_acknowledged: bool,
    pub rejection: Option<WorkerRejection>,
    pub result: Option<WorkerResult>,
    pub result_digest: Option<String>,
    pub quarantined: bool,
    pub receipt_id: Option<String>,
}

pub(super) struct OutboundMessage {
    pub packet: WorkerPacket,
    pub envelope: EncryptedEnvelope,
}

pub(super) struct RetainedWorkerMessage {
    pub packet: WorkerPacket,
    pub envelope: EncryptedEnvelope,
    pub recorded_at: UnixMillis,
    pub direction: String,
    pub peer: DeviceId,
}

#[derive(Clone)]
pub(super) struct Repository {
    store: Store,
}

impl Repository {
    pub fn validate_executor_store(store: &Store) -> Result<(), WorkerError> {
        let connection = store.connection()?;
        let foreign: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM transport_endpoint) OR EXISTS(SELECT 1 FROM devices) OR EXISTS(SELECT 1 FROM host_commands) OR EXISTS(SELECT 1 FROM scheduled_jobs) OR EXISTS(SELECT 1 FROM scheduled_runs) OR EXISTS(SELECT 1 FROM remote_commands) OR EXISTS(SELECT 1 FROM replica_events) OR EXISTS(SELECT 1 FROM conversations c WHERE NOT EXISTS(SELECT 1 FROM worker_native_admissions a JOIN worker_delegations d ON d.delegation_id = a.delegation_id WHERE json_extract(d.manifest_json, '$.ids.conversation_id') = c.id AND json_extract(d.manifest_json, '$.ids.task_id') = a.task_id)) OR EXISTS(SELECT 1 FROM tasks t WHERE NOT EXISTS(SELECT 1 FROM worker_native_admissions a JOIN worker_delegations d ON d.delegation_id = a.delegation_id WHERE a.task_id = t.id AND json_extract(d.manifest_json, '$.ids.conversation_id') = t.conversation_id)) OR EXISTS(SELECT 1 FROM tool_calls t WHERE NOT EXISTS(SELECT 1 FROM worker_native_admissions a JOIN worker_delegations d ON d.delegation_id = a.delegation_id WHERE a.task_id = t.task_id AND json_extract(d.manifest_json, '$.ids.conversation_id') = t.conversation_id)) OR EXISTS(SELECT 1 FROM permissions p WHERE p.conversation_id IS NULL OR NOT EXISTS(SELECT 1 FROM worker_native_admissions a JOIN worker_delegations d ON d.delegation_id = a.delegation_id WHERE json_extract(d.manifest_json, '$.ids.conversation_id') = p.conversation_id))",
            [],
            |row| row.get(0),
        )?;
        if foreign {
            return Err(WorkerError::Conflict(
                "executor requires dedicated worker state",
            ));
        }
        Ok(())
    }

    pub fn validate_executor_state(&self) -> Result<(), WorkerError> {
        Self::validate_executor_store(&self.store)?;
        self.verify_owned_native_tasks()
    }

    pub fn peers(&self) -> Result<Vec<WorkerPeerState>, WorkerError> {
        let connection = self.store.connection()?;
        let mut statement = connection.prepare("SELECT device_id, length(CAST(keys_json AS BLOB)), keys_json, pinned_at, revoked_at FROM worker_peers ORDER BY device_id LIMIT 17")?;
        let mut rows = statement.query([])?;
        let mut peers = Vec::new();
        while let Some(row) = rows.next()? {
            if peers.len() == MAX_PEERS as usize {
                return Err(WorkerError::Capacity("pinned worker peers"));
            }
            let length: i64 = row.get(1)?;
            if !(1..=2048).contains(&length) {
                return Err(WorkerError::Conflict("stored worker peer size"));
            }
            let encoded: String = row.get(2)?;
            let keys: DevicePublicKeys = serde_json::from_str(&encoded)?;
            keys.validate()?;
            let id: String = row.get(0)?;
            let pinned_at: i64 = row.get(3)?;
            let revoked_at: Option<i64> = row.get(4)?;
            if keys.device_id.to_string() != id
                || pinned_at < 0
                || revoked_at.is_some_and(|revoked| revoked < pinned_at)
            {
                return Err(WorkerError::Conflict("stored worker peer identity or time"));
            }
            peers.push(WorkerPeerState {
                keys,
                pinned_at: UnixMillis::try_from(pinned_at as u64)?,
                revoked_at: revoked_at
                    .map(|value| UnixMillis::try_from(value as u64))
                    .transpose()?,
            });
        }
        Ok(peers)
    }

    pub fn open(
        store: Store,
        identity: &DevicePublicKeys,
        role: WorkerRole,
        origin: &RelayOrigin,
    ) -> Result<Self, WorkerError> {
        let repository = Self { store };
        let mut connection = repository.store.connection()?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'worker_endpoint')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(WorkerError::SchemaUnavailable);
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let shared_identity: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM transport_endpoint WHERE device_id = ?1)",
            [identity.device_id.to_string()],
            |row| row.get(0),
        )?;
        if shared_identity {
            return Err(WorkerError::Conflict(
                "ordinary device identity cannot be a worker endpoint",
            ));
        }
        let expected = (
            serde_json::to_string(identity)?,
            role_name(role).to_owned(),
            origin.as_str().to_owned(),
        );
        let existing: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT identity_json, role, origin FROM worker_endpoint WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != expected {
                return Err(WorkerError::Conflict(
                    "endpoint identity, role or relay origin",
                ));
            }
        } else {
            transaction.execute(
                "INSERT INTO worker_endpoint(singleton, identity_json, role, origin) VALUES(1, ?1, ?2, ?3)",
                params![expected.0, expected.1, expected.2],
            )?;
        }
        transaction.commit()?;
        drop(connection);
        Ok(repository)
    }

    pub fn pin(&self, keys: &DevicePublicKeys, now: UnixMillis) -> Result<(), WorkerError> {
        keys.validate()?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ordinary_peer: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id = ?1)",
            [keys.device_id.to_string()],
            |row| row.get(0),
        )?;
        if ordinary_peer {
            return Err(WorkerError::Conflict(
                "ordinary device trust cannot be reused for workers",
            ));
        }
        let existing: Option<(String, Option<i64>)> = transaction
            .query_row(
                "SELECT keys_json, revoked_at FROM worker_peers WHERE device_id = ?1",
                [keys.device_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let encoded = serde_json::to_string(keys)?;
        if let Some((existing, revoked)) = existing {
            if revoked.is_some() {
                return Err(WorkerError::UntrustedPeer);
            }
            if existing != encoded {
                return Err(WorkerError::Conflict("peer public keys"));
            }
        } else {
            let count: i64 =
                transaction.query_row("SELECT count(*) FROM worker_peers", [], |row| row.get(0))?;
            if count >= MAX_PEERS {
                return Err(WorkerError::Capacity("pinned worker peers"));
            }
            if ledger_bytes(&transaction)? + encoded.len() as i64 + REVOCATION_RESERVE_BYTES
                > MAX_LEDGER_BYTES
            {
                return Err(WorkerError::Capacity("durable worker ledger"));
            }
            transaction.execute(
                "INSERT INTO worker_peers(device_id, keys_json, pinned_at) VALUES(?1, ?2, ?3)",
                params![keys.device_id.to_string(), encoded, now.get()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn peer(&self, peer: DeviceId) -> Result<DevicePublicKeys, WorkerError> {
        peer_keys(&*self.store.connection()?, peer)
    }

    pub fn native_uncertain(&self, task: crate::protocol::TaskId) -> Result<bool, WorkerError> {
        Ok(self.store.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id = ?1 AND state IN ('running', 'unknown'))", [task.to_string()], |row| row.get(0))?)
    }

    pub fn native_submission(
        &self,
        manifest: &WorkerManifest,
    ) -> Result<Option<crate::scheduler::TaskSubmission>, WorkerError> {
        let connection = self.store.connection()?;
        native_submission(&connection, manifest)
    }

    pub fn reserve_native(
        &self,
        manifest: &WorkerManifest,
        submission: &crate::scheduler::TaskSubmission,
    ) -> Result<(), WorkerError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fingerprint = manifest.fingerprint()?;
        let job = require_job(&transaction, &WorkerJobBinding::from_manifest(manifest)?)?;
        peer_keys(&transaction, job.peer)?;
        if job.cancellation_requested
            || !matches!(
                job.state,
                DelegationState::Accepted | DelegationState::Running
            )
        {
            return Err(WorkerError::Conflict(
                "native admission lacks current worker authority",
            ));
        }
        let encoded = serde_json::to_string(submission)?;
        if encoded.len() > 32768 {
            return Err(WorkerError::Capacity("worker native policy"));
        }
        if let Some(existing) = native_submission(&transaction, manifest)? {
            if serde_json::to_string(&existing)? != encoded {
                return Err(WorkerError::Conflict("worker native execution policy"));
            }
        } else {
            if ledger_bytes(&transaction)? + encoded.len() as i64 + REVOCATION_RESERVE_BYTES
                > MAX_LEDGER_BYTES
            {
                return Err(WorkerError::Capacity("durable worker ledger"));
            }
            let collision: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1) OR EXISTS(SELECT 1 FROM conversations WHERE id = ?2) OR EXISTS(SELECT 1 FROM commands WHERE id IN (?3, ?4))", params![manifest.ids.task_id.to_string(), manifest.ids.conversation_id.to_string(), manifest.ids.create_command_id.to_string(), manifest.ids.input_command_id.to_string()], |row| row.get(0))?;
            if collision {
                return Err(WorkerError::Conflict("preexisting unowned native identity"));
            }
            transaction.execute("INSERT INTO worker_native_admissions(task_id, delegation_id, manifest_digest, submission_json) VALUES(?1, ?2, ?3, ?4)", params![manifest.ids.task_id.to_string(), manifest.delegation_id.to_string(), fingerprint, encoded])?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn verify_owned_native_tasks(&self) -> Result<(), WorkerError> {
        let tasks = self.store.tasks(None, None, 257)?;
        if tasks.len() > 256 {
            return Err(WorkerError::Capacity("worker native tasks"));
        }
        let connection = self.store.connection()?;
        for task in tasks {
            let delegation: Option<String> = connection
                .query_row(
                    "SELECT delegation_id FROM worker_native_admissions WHERE task_id = ?1",
                    [&task.id],
                    |row| row.get(0),
                )
                .optional()?;
            let id =
                delegation.ok_or(WorkerError::Conflict("native task has no worker admission"))?;
            let id = uuid::Uuid::parse_str(&id)
                .map_err(|_| WorkerError::Conflict("stored native delegation"))?;
            let job = snapshot(&connection, id)?
                .ok_or(WorkerError::Conflict("native admission has no worker job"))?;
            let manifest = job
                .manifest
                .ok_or(WorkerError::Conflict("native admission has no manifest"))?;
            let submission = native_submission(&connection, &manifest)?
                .ok_or(WorkerError::Conflict("native admission disappeared"))?;
            if task.id != submission.task_id.to_string()
                || task.conversation_id != submission.conversation_id.to_string()
                || task.parent_task_id.is_some()
            {
                return Err(WorkerError::Conflict(
                    "native row differs from owned worker admission",
                ));
            }
        }
        Ok(())
    }

    pub fn delivery_peer(
        &self,
        peer: DeviceId,
        revocation: bool,
    ) -> Result<DevicePublicKeys, WorkerError> {
        if !revocation {
            return self.peer(peer);
        }
        let connection = self.store.connection()?;
        let encoded: String = connection
            .query_row(
                "SELECT keys_json FROM worker_peers WHERE device_id = ?1",
                [peer.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(WorkerError::UntrustedPeer)?;
        let keys: DevicePublicKeys = serde_json::from_str(&encoded)?;
        keys.validate()?;
        Ok(keys)
    }

    pub fn bind_profile(&self, profile: &WorkerModelBinding) -> Result<(), WorkerError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encoded = serde_json::to_string(profile)?;
        if encoded.len() > 32768 {
            return Err(WorkerError::Capacity("worker native model profile"));
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT profile_json FROM worker_profile WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != encoded {
                return Err(WorkerError::Conflict("worker-owned native model profile"));
            }
        } else {
            if ledger_bytes(&transaction)? + encoded.len() as i64 + REVOCATION_RESERVE_BYTES
                > MAX_LEDGER_BYTES
            {
                return Err(WorkerError::Capacity("durable worker ledger"));
            }
            transaction.execute(
                "INSERT INTO worker_profile(singleton, profile_json) VALUES(1, ?1)",
                [encoded],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        delegation: uuid::Uuid,
    ) -> Result<Option<DelegationSnapshot>, WorkerError> {
        snapshot(&*self.store.connection()?, delegation)
    }

    pub fn jobs(&self) -> Result<Vec<DelegationSnapshot>, WorkerError> {
        let connection = self.store.connection()?;
        let mut statement = connection.prepare("SELECT delegation_id FROM worker_delegations ORDER BY updated_at, delegation_id LIMIT 257")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > MAX_JOBS as usize {
            return Err(WorkerError::Capacity("stored worker jobs"));
        }
        ids.into_iter()
            .map(|id| {
                let id = uuid::Uuid::parse_str(&id)
                    .map_err(|_| WorkerError::Conflict("stored delegation ID"))?;
                snapshot(&connection, id)?
                    .ok_or(WorkerError::Conflict("stored delegation disappeared"))
            })
            .collect()
    }

    pub fn record_message(
        &self,
        packet: &WorkerPacket,
        envelope: &EncryptedEnvelope,
        inbound: bool,
        now: UnixMillis,
    ) -> Result<bool, WorkerError> {
        packet.validate()?;
        let bytes = packet.to_bytes()?;
        let encoded_envelope = serde_json::to_string(envelope)?;
        let revocation = matches!(packet.body, WorkerMessage::Revoke { .. });
        if revocation && bytes.len() + encoded_envelope.len() > MAX_REVOCATION_RECORD_BYTES {
            return Err(WorkerError::Invalid("oversized worker revocation envelope"));
        }
        let peer = if inbound {
            packet.sender
        } else {
            packet.recipient
        };
        if envelope.header.sender != packet.sender || envelope.header.recipient != packet.recipient
        {
            return Err(WorkerError::Conflict("envelope endpoint binding"));
        }
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, String, String)> = transaction.query_row(
            "SELECT packet_json, envelope_json, direction FROM worker_messages WHERE message_id = ?1",
            [envelope.header.message_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let direction = if inbound { "inbound" } else { "outbound" };
        if let Some((saved_bytes, saved_envelope, saved_direction)) = existing {
            if saved_bytes != bytes
                || saved_envelope != encoded_envelope
                || saved_direction != direction
            {
                return Err(WorkerError::Conflict("message replay contents"));
            }
            return Ok(false);
        }
        peer_keys(&transaction, peer)?;
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM worker_messages", [], |row| row.get(0))?;
        let size = ledger_bytes(&transaction)?;
        let message_limit = if revocation {
            MAX_MESSAGES
        } else {
            MAX_MESSAGES - MAX_PEERS
        };
        let reserved_bytes = if revocation { 0 } else { JOB_RESERVE_BYTES };
        if count >= message_limit
            || size + bytes.len() as i64 + encoded_envelope.len() as i64 + reserved_bytes
                > MAX_LEDGER_BYTES
        {
            return Err(WorkerError::Capacity("durable worker ledger"));
        }
        let terminal_source = if inbound
            && packet.sender_role == WorkerRole::Executor
            && matches!(
                packet.body,
                WorkerMessage::Result { .. } | WorkerMessage::Rejected { .. }
            ) {
            let coordinator: bool = transaction.query_row(
                "SELECT role='coordinator' FROM worker_endpoint WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            if coordinator {
                let binding = packet
                    .body
                    .binding()?
                    .ok_or(WorkerError::Conflict("terminal source has no binding"))?;
                let prior = require_job(&transaction, &binding)?;
                if prior.result.is_none() && prior.rejection.is_none() {
                    Some(binding.delegation_id)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        apply_packet(&transaction, packet, peer, now)?;
        transaction.execute(
            "INSERT INTO worker_messages(message_id, peer_id, direction, packet_json, envelope_json, expires_at, recorded_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![envelope.header.message_id.to_string(), peer.to_string(), direction, bytes, encoded_envelope, envelope.header.expires_at_ms, now.get()],
        )?;
        if let Some(delegation) = terminal_source {
            transaction.execute(
                "INSERT INTO worker_terminal_sources(delegation_id,message_id) VALUES(?1,?2)",
                params![
                    delegation.to_string(),
                    envelope.header.message_id.to_string()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(true)
    }

    pub fn terminal_source(
        &self,
        delegation: uuid::Uuid,
    ) -> Result<RetainedWorkerMessage, WorkerError> {
        let connection = self.store.connection()?;
        let id = bounded_message_id(&connection, "SELECT length(CAST(message_id AS BLOB)),message_id FROM worker_terminal_sources WHERE delegation_id=?1 LIMIT 2", &delegation.to_string())?.ok_or(WorkerError::Conflict("original terminal source unavailable; legacy outcome requires inspection"))?;
        retained_message(&connection, &id)
    }

    pub fn original_offer(
        &self,
        delegation: uuid::Uuid,
    ) -> Result<Option<RetainedWorkerMessage>, WorkerError> {
        let connection = self.store.connection()?;
        bounded_message_id(&connection, "SELECT length(CAST(message_id AS BLOB)),message_id FROM worker_messages WHERE direction='outbound' AND json_extract(CAST(packet_json AS TEXT),'$.body.kind')='offer' AND json_extract(CAST(packet_json AS TEXT),'$.body.manifest.delegation_id')=?1 LIMIT 2", &delegation.to_string())?.map(|id| retained_message(&connection, &id)).transpose()
    }

    pub fn original_acknowledgement(
        &self,
        delegation: uuid::Uuid,
    ) -> Result<Option<RetainedWorkerMessage>, WorkerError> {
        let connection = self.store.connection()?;
        bounded_message_id(&connection, "SELECT length(CAST(message_id AS BLOB)),message_id FROM worker_messages WHERE direction='outbound' AND json_extract(CAST(packet_json AS TEXT),'$.body.kind')='result_acknowledged' AND json_extract(CAST(packet_json AS TEXT),'$.body.binding.delegation_id')=?1 LIMIT 2", &delegation.to_string())?.map(|id| retained_message(&connection, &id)).transpose()
    }

    pub fn stage_manifest(
        &self,
        approved: &ApprovedDelegation,
        not_before: UnixMillis,
    ) -> Result<(), WorkerError> {
        let manifest = &approved.manifest;
        let binding = WorkerJobBinding::from_manifest(manifest)?;
        let encoded = serde_json::to_string(manifest)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = UnixMillis::now()?;
        if now < not_before {
            return Err(WorkerError::Expired);
        }
        approved.validate_for_submission(now)?;
        peer_keys(&transaction, manifest.executor)?;
        let existing = checked_job(&transaction, &binding, manifest.executor)?;
        if existing.as_ref().is_some_and(|job| {
            job.state != DelegationState::Offered
                || job.cancellation_requested
                || job.result.is_some()
                || job.rejection.is_some()
        }) {
            return Err(WorkerError::Conflict("staged job is no longer unstarted"));
        }
        let additional = if existing.as_ref().is_some_and(|job| job.manifest.is_some()) {
            0
        } else {
            encoded.len() as i64
        };
        if ledger_bytes(&transaction)? + additional + JOB_RESERVE_BYTES > MAX_LEDGER_BYTES {
            return Err(WorkerError::Capacity("durable worker ledger"));
        }
        retain_manifest(&transaction, manifest, manifest.executor, now, existing)?;
        let committed_at = UnixMillis::now()?;
        if committed_at < now {
            return Err(WorkerError::Expired);
        }
        approved.validate_for_submission(committed_at)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn outbox(&self, now: UnixMillis) -> Result<Vec<OutboundMessage>, WorkerError> {
        let connection = self.store.connection()?;
        let mut statement = connection.prepare(
            "SELECT packet_json, envelope_json FROM worker_messages WHERE direction = 'outbound' AND relay_accepted = 0 AND suppressed = 0 AND expires_at > ?1 ORDER BY recorded_at, message_id LIMIT 32",
        )?;
        let rows = statement
            .query_map([now.get()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(packet, envelope)| {
                Ok(OutboundMessage {
                    packet: WorkerPacket::parse(&packet)?,
                    envelope: serde_json::from_str(&envelope)?,
                })
            })
            .collect()
    }

    pub fn relay_accepted(&self, message: MessageId) -> Result<(), WorkerError> {
        self.store.connection()?.execute(
            "UPDATE worker_messages SET relay_accepted = 1 WHERE message_id = ?1 AND direction = 'outbound'",
            [message.to_string()],
        )?;
        Ok(())
    }

    pub fn suppress(&self, message: MessageId) -> Result<(), WorkerError> {
        self.store.connection()?.execute("UPDATE worker_messages SET suppressed = 1 WHERE message_id = ?1 AND direction = 'outbound'", [message.to_string()])?;
        Ok(())
    }

    pub fn mark_running(
        &self,
        binding: &WorkerJobBinding,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = require_job(&transaction, binding)?;
        peer_keys(&transaction, job.peer)?;
        if !matches!(
            job.state,
            DelegationState::Accepted | DelegationState::Running
        ) || job.cancellation_requested
        {
            return Err(WorkerError::Conflict(
                "job is not admitted for native execution",
            ));
        }
        transaction.execute("UPDATE worker_delegations SET state = 'running', updated_at = ?2 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), now.get()])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_quarantined(
        &self,
        binding: &WorkerJobBinding,
        digest: &str,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = require_job(&transaction, binding)?;
        if job.result_digest.as_deref() != Some(digest) {
            return Err(WorkerError::Conflict("quarantine result digest"));
        }
        transaction.execute("UPDATE worker_delegations SET quarantine_digest = ?2, updated_at = ?3 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), digest, now.get()])?;
        transaction.commit()?;
        Ok(())
    }
}

fn peer_keys(connection: &Connection, peer: DeviceId) -> Result<DevicePublicKeys, WorkerError> {
    let row: Option<(String, Option<i64>)> = connection
        .query_row(
            "SELECT keys_json, revoked_at FROM worker_peers WHERE device_id = ?1",
            [peer.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        Some((encoded, None)) => {
            let keys: DevicePublicKeys = serde_json::from_str(&encoded)?;
            keys.validate()?;
            Ok(keys)
        }
        _ => Err(WorkerError::UntrustedPeer),
    }
}

fn ledger_bytes(connection: &Connection) -> Result<i64, WorkerError> {
    Ok(connection.query_row("SELECT (SELECT coalesce(sum(length(packet_json) + length(CAST(envelope_json AS BLOB))), 0) FROM worker_messages) + (SELECT coalesce(sum(coalesce(length(CAST(manifest_json AS BLOB)), 0) + coalesce(length(CAST(result_json AS BLOB)), 0)), 0) FROM worker_delegations) + (SELECT coalesce(sum(length(CAST(submission_json AS BLOB))), 0) FROM worker_native_admissions) + (SELECT coalesce(sum(length(CAST(profile_json AS BLOB))), 0) FROM worker_profile) + (SELECT coalesce(sum(length(CAST(keys_json AS BLOB))), 0) FROM worker_peers) + (SELECT coalesce(sum(length(CAST(identity_json AS BLOB)) + length(CAST(origin AS BLOB))), 0) FROM worker_endpoint)", [], |row| row.get(0))?)
}

fn revoke(connection: &Connection, peer: DeviceId, now: UnixMillis) -> Result<(), WorkerError> {
    let changed = connection.execute(
        "UPDATE worker_peers SET revoked_at = coalesce(revoked_at, ?2) WHERE device_id = ?1",
        params![peer.to_string(), now.get()],
    )?;
    if changed == 0 {
        return Err(WorkerError::UntrustedPeer);
    }
    connection.execute("UPDATE worker_delegations SET state = 'revoked', updated_at = ?2 WHERE peer_id = ?1 AND state NOT IN ('acknowledged', 'rejected')", params![peer.to_string(), now.get()])?;
    Ok(())
}

fn snapshot(
    connection: &Connection,
    id: uuid::Uuid,
) -> Result<Option<DelegationSnapshot>, WorkerError> {
    type SavedJob = (
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
    );
    let row: Option<SavedJob> = connection.query_row(
        "SELECT attempt_id, peer_id, manifest_digest, manifest_json, state, cancel_command, rejection_json, result_json, result_digest, quarantine_digest, receipt_id, cancellation_acknowledged FROM worker_delegations WHERE delegation_id = ?1",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?, row.get(11)?)),
    ).optional()?;
    row.map(|row| {
        Ok(DelegationSnapshot {
            binding: WorkerJobBinding {
                delegation_id: id,
                attempt_id: uuid::Uuid::parse_str(&row.0)
                    .map_err(|_| WorkerError::Conflict("stored attempt ID"))?,
                manifest_digest: row.2,
            },
            peer: row
                .1
                .parse()
                .map_err(|_| WorkerError::Conflict("stored peer ID"))?,
            manifest: row
                .3
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            state: DelegationState::parse(&row.4)?,
            cancellation_requested: row.5.is_some(),
            cancellation_command_id: row
                .5
                .map(|value| {
                    value
                        .parse()
                        .map_err(|_| WorkerError::Conflict("stored cancellation command"))
                })
                .transpose()?,
            cancellation_acknowledged: row.11,
            rejection: row
                .6
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            result: row
                .7
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            result_digest: row.8,
            quarantined: row.9.is_some(),
            receipt_id: row.10,
        })
    })
    .transpose()
}

fn require_job(
    connection: &Connection,
    binding: &WorkerJobBinding,
) -> Result<DelegationSnapshot, WorkerError> {
    let job = snapshot(connection, binding.delegation_id)?
        .ok_or(WorkerError::Conflict("unknown delegation"))?;
    if job.binding != *binding {
        return Err(WorkerError::Conflict(
            "delegation attempt or manifest digest",
        ));
    }
    Ok(job)
}

fn checked_job(
    connection: &Connection,
    binding: &WorkerJobBinding,
    peer: DeviceId,
) -> Result<Option<DelegationSnapshot>, WorkerError> {
    let existing = snapshot(connection, binding.delegation_id)?;
    if let Some(job) = &existing {
        if job.binding != *binding || job.peer != peer {
            return Err(WorkerError::Conflict("delegation identity"));
        }
    } else {
        let count: i64 =
            connection.query_row("SELECT count(*) FROM worker_delegations", [], |row| {
                row.get(0)
            })?;
        if count >= MAX_JOBS {
            return Err(WorkerError::Capacity("durable worker jobs"));
        }
    }
    Ok(existing)
}

fn retain_manifest(
    connection: &Connection,
    manifest: &WorkerManifest,
    peer: DeviceId,
    now: UnixMillis,
    existing: Option<DelegationSnapshot>,
) -> Result<(), WorkerError> {
    if manifest.deadline <= now || now < manifest.created_at {
        return Err(WorkerError::Expired);
    }
    let binding = WorkerJobBinding::from_manifest(manifest)?;
    let encoded = serde_json::to_string(manifest)?;
    ensure_native_bindings_unique(connection, manifest)?;
    if let Some(job) = existing {
        if let Some(saved) = job.manifest {
            if saved.fingerprint()? != binding.manifest_digest
                || serde_json::to_string(&saved)? != encoded
            {
                return Err(WorkerError::Conflict("manifest replay"));
            }
        } else {
            connection.execute("UPDATE worker_delegations SET manifest_json = ?2, updated_at = ?3 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), encoded, now.get()])?;
        }
    } else {
        connection.execute("INSERT INTO worker_delegations(delegation_id, attempt_id, peer_id, manifest_digest, manifest_json, state, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, 'offered', ?6)", params![binding.delegation_id.to_string(), binding.attempt_id.to_string(), peer.to_string(), binding.manifest_digest, encoded, now.get()])?;
    }
    Ok(())
}

fn apply_packet(
    connection: &Connection,
    packet: &WorkerPacket,
    peer: DeviceId,
    now: UnixMillis,
) -> Result<(), WorkerError> {
    if let WorkerMessage::Revoke { revoked_at, .. } = &packet.body {
        if revoked_at.get() > now.get().saturating_add(120_000) {
            return Err(WorkerError::Invalid("future peer revocation"));
        }
        return revoke(connection, peer, now);
    }
    let binding = packet
        .body
        .binding()?
        .ok_or(WorkerError::Invalid("job message binding"))?;
    let existing = checked_job(connection, &binding, peer)?;
    match &packet.body {
        WorkerMessage::Offer { manifest } => {
            retain_manifest(connection, manifest, peer, now, existing)?;
        }
        WorkerMessage::Cancel { request_id, .. } => {
            if let Some(job) = existing {
                if job
                    .cancellation_command_id
                    .is_some_and(|saved| saved != *request_id)
                {
                    return Err(WorkerError::Conflict("immutable cancellation request"));
                }
                let next = if job.result.is_some() || job.state.is_terminal() {
                    state_name(job.state)
                } else {
                    "cancel_requested"
                };
                connection.execute("UPDATE worker_delegations SET cancel_command = coalesce(cancel_command, ?2), state = ?3, updated_at = ?4 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), request_id.to_string(), next, now.get()])?;
            } else {
                connection.execute("INSERT INTO worker_delegations(delegation_id, attempt_id, peer_id, manifest_digest, state, cancel_command, updated_at) VALUES(?1, ?2, ?3, ?4, 'cancel_requested', ?5, ?6)", params![binding.delegation_id.to_string(), binding.attempt_id.to_string(), peer.to_string(), binding.manifest_digest, request_id.to_string(), now.get()])?;
            }
        }
        WorkerMessage::Accepted { .. } => {
            let job = require_job(connection, &binding)?;
            if job.state == DelegationState::Offered && !job.cancellation_requested {
                connection.execute("UPDATE worker_delegations SET state = 'accepted', updated_at = ?2 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), now.get()])?;
            }
        }
        WorkerMessage::Rejected { reason, .. } => {
            let job = require_job(connection, &binding)?;
            let encoded = serde_json::to_string(reason)?;
            if job.result.is_some() || job.rejection.as_ref().is_some_and(|saved| saved != reason) {
                return Err(WorkerError::Conflict("terminal worker rejection"));
            }
            connection.execute("UPDATE worker_delegations SET state = 'rejected', rejection_json = ?2, updated_at = ?3 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), encoded, now.get()])?;
        }
        WorkerMessage::CancellationAccepted { request_id, .. } => {
            let job = require_job(connection, &binding)?;
            if job.cancellation_command_id != Some(*request_id) {
                return Err(WorkerError::Conflict("unsolicited cancellation receipt"));
            }
            connection.execute("UPDATE worker_delegations SET cancellation_acknowledged = 1, updated_at = ?2 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), now.get()])?;
        }
        WorkerMessage::Result { result } => {
            let job = require_job(connection, &binding)?;
            let manifest = job
                .manifest
                .ok_or(WorkerError::Conflict("result has no immutable manifest"))?;
            result.validate(&manifest)?;
            let fingerprint = result.fingerprint()?;
            if job.rejection.is_some()
                || job
                    .result_digest
                    .as_ref()
                    .is_some_and(|saved| saved != &fingerprint)
            {
                return Err(WorkerError::Conflict("terminal worker result"));
            }
            let next = if job.receipt_id.is_some() {
                "acknowledged"
            } else if job.state == DelegationState::Revoked {
                "revoked"
            } else {
                "result_ready"
            };
            connection.execute("UPDATE worker_delegations SET result_json = ?2, result_digest = ?3, state = ?4, updated_at = ?5 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), serde_json::to_string(result)?, fingerprint, next, now.get()])?;
        }
        WorkerMessage::ResultAcknowledged {
            result_digest,
            receipt_id,
            ..
        } => {
            let job = require_job(connection, &binding)?;
            if job.result_digest.as_ref() != Some(result_digest)
                || job
                    .receipt_id
                    .as_ref()
                    .is_some_and(|saved| saved != &receipt_id.to_string())
            {
                return Err(WorkerError::Conflict("result acknowledgment identity"));
            }
            if packet.sender_role == WorkerRole::Coordinator
                && packet.sender == peer
                && job.result.is_none()
            {
                return Err(WorkerError::Conflict("result acknowledgment before result"));
            }
            connection.execute("UPDATE worker_delegations SET receipt_id = ?2, state = 'acknowledged', updated_at = ?3 WHERE delegation_id = ?1", params![binding.delegation_id.to_string(), receipt_id.to_string(), now.get()])?;
        }
        WorkerMessage::Revoke { .. } => return Err(WorkerError::Invalid("revocation dispatch")),
    }
    Ok(())
}

fn role_name(role: WorkerRole) -> &'static str {
    match role {
        WorkerRole::Coordinator => "coordinator",
        WorkerRole::Executor => "executor",
    }
}

fn ensure_native_bindings_unique(
    connection: &Connection,
    manifest: &WorkerManifest,
) -> Result<(), WorkerError> {
    let mut statement = connection.prepare("SELECT manifest_json FROM worker_delegations WHERE delegation_id != ?1 AND manifest_json IS NOT NULL LIMIT 257")?;
    let rows = statement
        .query_map([manifest.delegation_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > MAX_JOBS as usize {
        return Err(WorkerError::Capacity("stored worker manifests"));
    }
    for encoded in rows {
        let existing: WorkerManifest = serde_json::from_str(&encoded)?;
        let same_command = [
            existing.ids.create_command_id,
            existing.ids.input_command_id,
        ]
        .into_iter()
        .any(|id| id == manifest.ids.create_command_id || id == manifest.ids.input_command_id);
        if existing.ids.task_id == manifest.ids.task_id
            || existing.ids.conversation_id == manifest.ids.conversation_id
            || same_command
        {
            return Err(WorkerError::Conflict(
                "native identity reused by another delegation",
            ));
        }
    }
    Ok(())
}

fn native_submission(
    connection: &Connection,
    manifest: &WorkerManifest,
) -> Result<Option<crate::scheduler::TaskSubmission>, WorkerError> {
    let row: Option<(String, String, String)> = connection.query_row("SELECT delegation_id, manifest_digest, submission_json FROM worker_native_admissions WHERE task_id = ?1", [manifest.ids.task_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    row.map(|(delegation, digest, encoded)| {
        if delegation != manifest.delegation_id.to_string() || digest != manifest.fingerprint()? {
            return Err(WorkerError::Conflict("native worker admission identity"));
        }
        let submission: crate::scheduler::TaskSubmission = serde_json::from_str(&encoded)?;
        submission.execution.validate()?;
        let mut limits = manifest.limits.clone();
        if manifest.allowed_tools.is_empty() {
            limits.max_tool_calls = 0;
        }
        if submission.task_id != manifest.ids.task_id
            || submission.conversation_id != manifest.ids.conversation_id
            || submission.command_id != manifest.ids.input_command_id
            || submission.accepted_at != manifest.created_at
            || submission.execution.mode != manifest.mode
            || submission.execution.tools_enabled == manifest.allowed_tools.is_empty()
            || submission.execution.allowed_tools.as_ref() != Some(&manifest.allowed_tools)
            || serde_json::to_vec(&submission.execution.budget)?
                != serde_json::to_vec(&manifest.budget)?
            || submission.execution.limits != limits
        {
            return Err(WorkerError::Conflict("native worker admission policy"));
        }
        Ok(submission)
    })
    .transpose()
}

fn state_name(state: DelegationState) -> &'static str {
    match state {
        DelegationState::Offered => "offered",
        DelegationState::Accepted => "accepted",
        DelegationState::Running => "running",
        DelegationState::CancelRequested => "cancel_requested",
        DelegationState::ResultReady => "result_ready",
        DelegationState::Acknowledged => "acknowledged",
        DelegationState::Rejected => "rejected",
        DelegationState::Revoked => "revoked",
    }
}

fn bounded_message_id(
    connection: &Connection,
    sql: &str,
    key: &str,
) -> Result<Option<String>, WorkerError> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query([key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let length: i64 = row.get(0)?;
    if length != 36 {
        return Err(WorkerError::Conflict(
            "retained worker message identity size",
        ));
    }
    let id: String = row.get(1)?;
    if rows.next()?.is_some() {
        return Err(WorkerError::Conflict("ambiguous original worker message"));
    }
    Ok(Some(id))
}

fn retained_message(
    connection: &Connection,
    id: &str,
) -> Result<RetainedWorkerMessage, WorkerError> {
    let mut statement = connection.prepare("SELECT length(packet_json),length(CAST(envelope_json AS BLOB)),length(CAST(peer_id AS BLOB)),packet_json,envelope_json,recorded_at,expires_at,direction,peer_id FROM worker_messages WHERE message_id=?1")?;
    let mut rows = statement.query([id])?;
    let row = rows
        .next()?
        .ok_or(WorkerError::Conflict("retained worker message disappeared"))?;
    let packet_length: i64 = row.get(0)?;
    let envelope_length: i64 = row.get(1)?;
    let peer_length: i64 = row.get(2)?;
    if !(1..=524_288).contains(&packet_length)
        || !(1..=786_432).contains(&envelope_length)
        || peer_length != 36
    {
        return Err(WorkerError::Capacity("retained worker message size"));
    }
    let packet_bytes: Vec<u8> = row.get(3)?;
    let envelope_json: String = row.get(4)?;
    let recorded_at: i64 = row.get(5)?;
    let expires_at: i64 = row.get(6)?;
    let direction: String = row.get(7)?;
    let peer: String = row.get(8)?;
    if recorded_at < 0 || expires_at <= recorded_at {
        return Err(WorkerError::Conflict("retained worker message time"));
    }
    let packet = WorkerPacket::parse(&packet_bytes)?;
    let envelope: EncryptedEnvelope = serde_json::from_str(&envelope_json)?;
    if packet.to_bytes()? != packet_bytes
        || serde_json::to_string(&envelope)? != envelope_json
        || envelope.header.message_id.to_string() != id
        || envelope.header.expires_at_ms != expires_at as u64
        || envelope.header.sender != packet.sender
        || envelope.header.recipient != packet.recipient
    {
        return Err(WorkerError::Conflict(
            "retained worker message canonical binding",
        ));
    }
    Ok(RetainedWorkerMessage {
        packet,
        envelope,
        recorded_at: UnixMillis::try_from(recorded_at as u64)?,
        direction,
        peer: peer
            .parse()
            .map_err(|_| WorkerError::Conflict("retained worker message peer"))?,
    })
}

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    AuthenticatedSettingsAccessSource, AuthenticatedSettingsCommandSource,
    AuthenticatedSettingsReadSource, BindingHistoryState, BindingIntent, BindingLifetime,
    OwnedCommittedSettingsBind, OwnedCommittedSettingsRead, SettingsAccessStatusIntent,
    SettingsConnectionIdentity, SettingsHostIdentity, SettingsOriginalInspectionIntent,
    SettingsParentReference, SettingsReadRequest, SettingsSnapshotReadIntent,
};
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::Store;

use super::super::grants::SettingsGrantOwner;
use super::super::service::{
    SettingsReadQualificationError, SettingsReadQueryDisposition, SettingsServiceError,
};
use super::Authority;

const MAX_PAIRED_PARENTS: usize = 32;
const MAX_PEER_CONNECTION_PARENTS: usize = 4;
const MAX_ACCESS_QUERIES: usize = 512;
const MAX_PEER_ACCESS_QUERIES: usize = 64;

#[derive(Default)]
pub(super) struct PairedParents {
    host: Option<Arc<SettingsHostIdentity>>,
    entries: BTreeMap<(Uuid, DeviceId, Uuid), ParentEntry>,
    access_connection: Option<Arc<SettingsConnectionIdentity>>,
    access_queries: BTreeMap<(DeviceId, Uuid), AccessQuery>,
}

struct ParentEntry {
    record: Arc<ParentRecord>,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct ParentRecord {
    id: Uuid,
    intent: BindingIntent,
    peer: DevicePublicKeys,
    connection: Arc<SettingsConnectionIdentity>,
    creation: Arc<()>,
    lifetime: BindingLifetime,
    cancellation: CancellationToken,
}

pub(crate) struct PairedSettingsParent {
    authority: Arc<Authority>,
    record: Arc<ParentRecord>,
}

impl fmt::Debug for PairedSettingsParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedSettingsParent([restricted])")
    }
}

impl PairedSettingsParent {
    pub(crate) fn id(&self) -> Uuid {
        self.record.id
    }

    pub(crate) fn expires_at(&self) -> UnixMillis {
        self.record.lifetime.expires_at
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.record.cancellation.child_token()
    }
}

impl PairedParents {
    pub(super) fn close(&self) {
        for entry in self.entries.values() {
            entry.record.cancellation.cancel();
        }
    }
}

impl Authority {
    pub(in crate::host::settings) fn attach_paired_transport(
        &self,
        host: Arc<SettingsHostIdentity>,
    ) -> Result<(), SettingsServiceError> {
        if host.host_id() != self.host_id || !host.is_live() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        match &sessions.paired.host {
            Some(original) if !Arc::ptr_eq(original, &host) => Err(SettingsServiceError::Conflict),
            Some(_) => Ok(()),
            None => {
                sessions.paired.host = Some(host);
                Ok(())
            }
        }
    }

    pub(in crate::host::settings) fn install_paired_parent(
        self: &Arc<Self>,
        request: &OwnedCommittedSettingsBind,
        stop: &CancellationToken,
    ) -> Result<PairedSettingsParent, SettingsServiceError> {
        let source = request.committed_source();
        let authenticated = source.authority_source();
        let connection = authenticated.connection();
        let intent = request.binding_intent();
        let lifetime = source.lifetime();
        let snapshot = request
            .binding_snapshot()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if snapshot.intent != *intent
            || snapshot.state != BindingHistoryState::Committed
            || snapshot.lifetime.is_none_or(|observed| {
                observed.expires_at != lifetime.expires_at || observed.deadline != lifetime.deadline
            })
            || !connection.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed || stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        if !connection.is_live()
            || wall.get() < intent.issued_at
            || wall < source.reply_reservation().created_at
            || wall >= lifetime.expires_at
            || monotonic >= lifetime.deadline
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if sessions
            .paired
            .host
            .as_ref()
            .is_none_or(|host| !Arc::ptr_eq(host, connection.host_identity()) || !host.is_live())
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let key = (
            connection.connection_epoch(),
            authenticated.verified_peer_keys().device_id,
            intent.binding_request_id,
        );
        if let Some(entry) = sessions.paired.entries.get_mut(&key) {
            let original = &entry.record;
            if original.intent != *intent
                || original.peer != *authenticated.verified_peer_keys()
                || !Arc::ptr_eq(&original.connection, connection)
                || !Arc::ptr_eq(&original.creation, source.creation_identity())
                || original.lifetime.expires_at != lifetime.expires_at
                || original.lifetime.deadline != lifetime.deadline
                || original.cancellation.is_cancelled()
                || wall < entry.observed_wall
                || monotonic < entry.observed_monotonic
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            entry.observed_wall = wall;
            entry.observed_monotonic = monotonic;
            return Ok(PairedSettingsParent {
                authority: Arc::clone(self),
                record: Arc::clone(original),
            });
        }
        let bind_deadline = if lifetime.expires_at.get() > intent.expires_at {
            lifetime
                .deadline
                .checked_sub(std::time::Duration::from_millis(
                    lifetime.expires_at.get() - intent.expires_at,
                ))
                .ok_or(SettingsServiceError::Expired)?
        } else {
            lifetime.deadline
        };
        if wall.get() >= intent.expires_at || monotonic >= bind_deadline {
            return Err(SettingsServiceError::Expired);
        }
        if sessions.paired.entries.len() >= MAX_PAIRED_PARENTS
            || sessions
                .paired
                .entries
                .values()
                .filter(|entry| {
                    entry.record.peer.device_id == key.1
                        && Arc::ptr_eq(&entry.record.connection, connection)
                })
                .count()
                >= MAX_PEER_CONNECTION_PARENTS
        {
            return Err(SettingsServiceError::Capacity);
        }
        let mut random = [0; 16];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| SettingsServiceError::Entropy)?;
        let id = uuid::Builder::from_random_bytes(random).into_uuid();
        if sessions
            .paired
            .entries
            .values()
            .any(|entry| entry.record.id == id)
        {
            return Err(SettingsServiceError::Conflict);
        }
        let final_wall = UnixMillis::now()?;
        let final_monotonic = Instant::now();
        if stop.is_cancelled() || !connection.is_live() {
            return Err(SettingsServiceError::Stopped);
        }
        if final_wall < wall
            || final_wall.get() >= intent.expires_at
            || final_wall >= lifetime.expires_at
            || final_monotonic < monotonic
            || final_monotonic >= bind_deadline
        {
            return Err(SettingsServiceError::Expired);
        }
        let record = Arc::new(ParentRecord {
            id,
            intent: intent.clone(),
            peer: authenticated.verified_peer_keys().clone(),
            connection: Arc::clone(connection),
            creation: Arc::clone(source.creation_identity()),
            lifetime,
            cancellation: connection.cancellation(),
        });
        sessions.paired.entries.insert(
            key,
            ParentEntry {
                record: Arc::clone(&record),
                observed_wall: final_wall,
                observed_monotonic: final_monotonic,
            },
        );
        Ok(PairedSettingsParent {
            authority: Arc::clone(self),
            record,
        })
    }

    pub(in crate::host::settings) fn close_paired_parent(
        self: &Arc<Self>,
        parent: &PairedSettingsParent,
    ) -> Result<(), SettingsServiceError> {
        if !Arc::ptr_eq(self, &parent.authority) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let key = (
            parent.record.connection.connection_epoch(),
            parent.record.peer.device_id,
            parent.record.intent.binding_request_id,
        );
        if sessions
            .paired
            .entries
            .get(&key)
            .is_none_or(|entry| !Arc::ptr_eq(&entry.record, &parent.record))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        parent.record.cancellation.cancel();
        Ok(())
    }
}

impl Authority {
    pub(in crate::host::settings) fn paired_receipt_owner(
        &self,
        host: &Arc<SettingsHostIdentity>,
    ) -> Result<SettingsGrantOwner, SettingsServiceError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        if host.host_id() != self.host_id
            || !host.is_live()
            || sessions
                .paired
                .host
                .as_ref()
                .is_none_or(|registered| !Arc::ptr_eq(registered, host))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(SettingsGrantOwner {
            host_id: self.host_id,
            principal_id: self.owner_principal,
        })
    }

    pub(in crate::host::settings) fn check_paired_command(
        &self,
        host: &Arc<SettingsHostIdentity>,
        store: &Store,
        source: &AuthenticatedSettingsCommandSource,
        stop: &CancellationToken,
    ) -> Result<(SettingsGrantOwner, UnixMillis, Instant), SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed || stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        if sessions
            .paired
            .host
            .as_ref()
            .is_none_or(|registered| !Arc::ptr_eq(registered, host))
            || !Arc::ptr_eq(source.connection().host_identity(), host)
            || host.host_id() != self.host_id
            || !host.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let (wall, monotonic) = source.check_precommit(store)?;
        let intent = source.intent();
        if intent.host_id != self.host_id {
            return Err(SettingsServiceError::OutsideScope);
        }
        let entry = sessions
            .paired
            .entries
            .values_mut()
            .find(|entry| entry.record.id == intent.parent.parent_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        if record.intent.host_id != intent.host_id
            || record.intent.generation != intent.parent.generation
            || record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.connection, source.connection())
            || !record.connection.is_live()
            || record.cancellation.is_cancelled()
            || intent.expires_at > record.lifetime.expires_at
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= record.lifetime.expires_at || monotonic >= record.lifetime.deadline {
            return Err(SettingsServiceError::Expired);
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        Ok((
            SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            wall,
            monotonic,
        ))
    }
}

pub(super) struct PairedUpdateProof {
    record: Arc<ParentRecord>,
    original: crate::storage::settings_originals::RemoteAcceptedIdentity,
    invocation: Arc<()>,
    deadline: Instant,
    floor: (UnixMillis, Instant),
}

impl Authority {
    pub(in crate::host::settings) fn paired_update_caller(
        self: &Arc<Self>,
        committed: &crate::devices::CommittedSettingsCommandSource<'_>,
    ) -> Result<super::SettingsCaller, SettingsServiceError> {
        let crate::storage::settings_originals::RemoteOriginalIdentity::Accepted(original) =
            &committed.original().identity
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let source = committed.authority_source();
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let record = sessions
            .paired
            .entries
            .values()
            .find(|entry| entry.record.id == original.remote_parent_id)
            .map(|entry| Arc::clone(&entry.record))
            .ok_or(SettingsServiceError::OutsideScope)?;
        if record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.connection, source.connection())
            || original.host_id != self.host_id
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        drop(sessions);
        let caller = super::SettingsCaller {
            authority: Arc::clone(self),
            source: super::CallerSource::PairedSettings {
                request_id: original.request_id,
                actor_device_id: original.peer_keys.device_id,
                remote_parent_id: original.remote_parent_id,
                grant_id: original.grant_id,
                grant_revision: original.grant_revision,
                host_epoch: original.host_epoch,
                connection_epoch: original.connection_epoch,
            },
            agent_identity: None,
            paired_update: Some(Box::new(PairedUpdateProof {
                record,
                original: original.clone(),
                invocation: Arc::clone(source.invocation_identity()),
                deadline: committed.original_deadline(),
                floor: committed.receipt_floor(),
            })),
        };
        self.check_paired_update(&caller)?;
        Ok(caller)
    }

    pub(in crate::host::settings) fn paired_update_identity<'caller>(
        &self,
        caller: &'caller super::SettingsCaller,
    ) -> Result<
        &'caller crate::storage::settings_originals::RemoteAcceptedIdentity,
        SettingsServiceError,
    > {
        self.check_paired_update(caller)?;
        Ok(&caller
            .paired_update
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?
            .original)
    }

    pub(in crate::host::settings) fn paired_update_matches(
        &self,
        caller: &super::SettingsCaller,
        committed: &crate::devices::CommittedSettingsCommandSource<'_>,
    ) -> Result<(), SettingsServiceError> {
        self.check_paired_update(caller)?;
        let proof = caller
            .paired_update
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?;
        if !Arc::ptr_eq(
            &proof.invocation,
            committed.authority_source().invocation_identity(),
        ) || proof.deadline != committed.original_deadline()
            || proof.floor != committed.receipt_floor()
            || committed.original().identity
                != crate::storage::settings_originals::RemoteOriginalIdentity::Accepted(
                    proof.original.clone(),
                )
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    pub(in crate::host::settings) fn paired_update_cancellation(
        &self,
        caller: &super::SettingsCaller,
    ) -> Result<CancellationToken, SettingsServiceError> {
        self.check_paired_update(caller)?;
        Ok(caller
            .paired_update
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?
            .record
            .cancellation
            .child_token())
    }

    pub(in crate::host::settings) fn check_paired_update(
        &self,
        caller: &super::SettingsCaller,
    ) -> Result<(UnixMillis, Instant), SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let proof = caller
            .paired_update
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?;
        let original = &proof.original;
        let super::CallerSource::PairedSettings {
            request_id,
            actor_device_id,
            remote_parent_id,
            grant_id,
            grant_revision,
            host_epoch,
            connection_epoch,
        } = caller.source
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if request_id != original.request_id
            || actor_device_id != original.peer_keys.device_id
            || remote_parent_id != original.remote_parent_id
            || grant_id != original.grant_id
            || grant_revision != original.grant_revision
            || host_epoch != original.host_epoch
            || connection_epoch != original.connection_epoch
            || original.host_id != self.host_id
            || original.owner_principal_id != self.owner_principal
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        let host = sessions
            .paired
            .host
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?;
        if !Arc::ptr_eq(host, proof.record.connection.host_identity()) || !host.is_live() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let entry = sessions
            .paired
            .entries
            .values_mut()
            .find(|entry| Arc::ptr_eq(&entry.record, &proof.record))
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        let generation = &record.intent.generation;
        if record.id != original.remote_parent_id
            || record.peer != original.peer_keys
            || generation.grant_id != original.grant_id
            || generation.grant_revision != original.grant_revision
            || generation.host_epoch != original.host_epoch
            || generation.connection_epoch != original.connection_epoch
            || !record.connection.is_live()
            || record.cancellation.is_cancelled()
            || original.expires_at > record.lifetime.expires_at
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
            || wall < proof.floor.0
            || monotonic < proof.floor.1
            || wall < original.admitted_at
            || wall < original.issued_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= original.expires_at
            || monotonic >= proof.deadline
            || wall >= record.lifetime.expires_at
            || monotonic >= record.lifetime.deadline
        {
            return Err(SettingsServiceError::Expired);
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        Ok((wall, monotonic))
    }
}

impl Authority {
    pub(in crate::host::settings) fn observe_paired_reply(
        &self,
        host: &Arc<SettingsHostIdentity>,
        source: &AuthenticatedSettingsCommandSource,
        floor: (UnixMillis, Instant),
        deadline: Instant,
        stop: &CancellationToken,
    ) -> Result<(SettingsGrantOwner, UnixMillis, Instant), SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed || stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        let connection = source.connection();
        let intent = source.intent();
        if sessions
            .paired
            .host
            .as_ref()
            .is_none_or(|registered| !Arc::ptr_eq(registered, host))
            || !Arc::ptr_eq(connection.host_identity(), host)
            || host.host_id() != self.host_id
            || intent.host_id != self.host_id
            || intent.parent.generation.host_epoch != host.host_epoch()
            || intent.parent.generation.connection_epoch != connection.connection_epoch()
            || !host.is_live()
            || !connection.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let entry = sessions
            .paired
            .entries
            .values_mut()
            .find(|entry| entry.record.id == intent.parent.parent_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        if record.intent.host_id != intent.host_id
            || record.intent.generation != intent.parent.generation
            || record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.connection, connection)
            || record.cancellation.is_cancelled()
            || intent.expires_at > record.lifetime.expires_at
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
            || wall < floor.0
            || monotonic < floor.1
            || wall < intent.issued_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= intent.expires_at
            || monotonic >= deadline
            || wall >= record.lifetime.expires_at
            || monotonic >= record.lifetime.deadline
        {
            return Err(SettingsServiceError::Expired);
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        Ok((
            SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            wall,
            monotonic,
        ))
    }
}

impl Authority {
    pub(in crate::host::settings) fn observe_paired_bind_reply(
        &self,
        host: &Arc<SettingsHostIdentity>,
        request: &OwnedCommittedSettingsBind,
        parent: &PairedSettingsParent,
        stop: &CancellationToken,
    ) -> Result<(SettingsGrantOwner, UnixMillis, Instant), SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&parent.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let committed = request.committed_source();
        let source = committed.authority_source();
        let connection = source.connection();
        let lifetime = committed.lifetime();
        let intent = request.binding_intent();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed || stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        if sessions
            .paired
            .host
            .as_ref()
            .is_none_or(|registered| !Arc::ptr_eq(registered, host))
            || !Arc::ptr_eq(connection.host_identity(), host)
            || host.host_id() != self.host_id
            || intent.host_id != self.host_id
            || intent.generation.host_epoch != host.host_epoch()
            || intent.generation.connection_epoch != connection.connection_epoch()
            || !host.is_live()
            || !connection.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let entry = sessions
            .paired
            .entries
            .values_mut()
            .find(|entry| Arc::ptr_eq(&entry.record, &parent.record))
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        if record.intent != *intent
            || record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.creation, committed.creation_identity())
            || !Arc::ptr_eq(&record.connection, connection)
            || record.lifetime.expires_at != lifetime.expires_at
            || record.lifetime.deadline != lifetime.deadline
            || record.cancellation.is_cancelled()
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
            || wall < committed.reply_reservation().created_at
            || wall.get() < intent.issued_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= record.lifetime.expires_at || monotonic >= record.lifetime.deadline {
            return Err(SettingsServiceError::Expired);
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        Ok((
            SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            wall,
            monotonic,
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
enum SettingsQueryIntent {
    AccessStatus(SettingsAccessStatusIntent),
    Snapshot(SettingsSnapshotReadIntent),
    InspectOriginal(SettingsOriginalInspectionIntent),
}

impl SettingsQueryIntent {
    fn from_read(request: &SettingsReadRequest) -> Self {
        match request {
            SettingsReadRequest::Snapshot(intent) => Self::Snapshot(intent.clone()),
            SettingsReadRequest::InspectOriginal(intent) => Self::InspectOriginal(intent.clone()),
        }
    }
}

struct AccessQuery {
    identity: Arc<()>,
    intent: SettingsQueryIntent,
    peer: DevicePublicKeys,
    floor: (UnixMillis, Instant),
    deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

pub(in crate::host::settings) struct AccessQueryObservation {
    pub(in crate::host::settings) owner: SettingsGrantOwner,
    pub(in crate::host::settings) query: Arc<()>,
    pub(in crate::host::settings) floor: (UnixMillis, Instant),
    pub(in crate::host::settings) deadline: Instant,
    pub(in crate::host::settings) wall: UnixMillis,
    pub(in crate::host::settings) monotonic: Instant,
}

impl Authority {
    pub(in crate::host::settings) fn register_access_query(
        &self,
        host: &Arc<SettingsHostIdentity>,
        source: &AuthenticatedSettingsAccessSource,
        stop: &CancellationToken,
    ) -> Result<(), SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_access_host(self, &sessions, host, source, stop)?;
        let paired = &mut sessions.paired;
        if let Some(original) = &paired.access_connection {
            if !Arc::ptr_eq(original, source.connection()) {
                if original.is_live()
                    || !Arc::ptr_eq(original.host_identity(), host)
                    || original.connection_epoch() == source.connection().connection_epoch()
                {
                    return Err(SettingsServiceError::Conflict);
                }
                paired.access_queries.clear();
                paired.access_connection = Some(Arc::clone(source.connection()));
            }
        } else {
            if !paired.access_queries.is_empty() {
                return Err(SettingsServiceError::CorruptJournal);
            }
            paired.access_connection = Some(Arc::clone(source.connection()));
        }
        let intent = source.intent();
        let peer = source.verified_peer_keys();
        let key = (peer.device_id, intent.query_id);
        if let Some(original) = paired.access_queries.get(&key) {
            if original.intent != SettingsQueryIntent::AccessStatus(intent.clone())
                || original.peer != *peer
            {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(());
        }
        if paired.access_queries.len() >= MAX_ACCESS_QUERIES
            || paired
                .access_queries
                .values()
                .filter(|query| query.peer == *peer)
                .count()
                >= MAX_PEER_ACCESS_QUERIES
        {
            return Err(SettingsServiceError::Capacity);
        }
        let floor = source.receipt_floor();
        paired.access_queries.insert(
            key,
            AccessQuery {
                identity: Arc::new(()),
                intent: SettingsQueryIntent::AccessStatus(intent.clone()),
                peer: peer.clone(),
                floor,
                deadline: source.original_deadline(),
                observed_wall: floor.0,
                observed_monotonic: floor.1,
            },
        );
        Ok(())
    }

    pub(in crate::host::settings) fn observe_access_query(
        &self,
        host: &Arc<SettingsHostIdentity>,
        source: &AuthenticatedSettingsAccessSource,
        stop: &CancellationToken,
    ) -> Result<AccessQueryObservation, SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_access_host(self, &sessions, host, source, stop)?;
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        let paired = &mut sessions.paired;
        if paired
            .access_connection
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, source.connection()))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let intent = source.intent();
        let peer = source.verified_peer_keys();
        let query = paired
            .access_queries
            .get_mut(&(peer.device_id, intent.query_id))
            .ok_or(SettingsServiceError::NotFound)?;
        if query.intent != SettingsQueryIntent::AccessStatus(intent.clone()) || query.peer != *peer
        {
            return Err(SettingsServiceError::Conflict);
        }
        let floor = source.receipt_floor();
        if wall < query.floor.0
            || monotonic < query.floor.1
            || wall < floor.0
            || monotonic < floor.1
            || wall < query.observed_wall
            || monotonic < query.observed_monotonic
            || wall < intent.issued_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= intent.expires_at
            || monotonic >= query.deadline
            || monotonic >= source.original_deadline()
        {
            return Err(SettingsServiceError::Expired);
        }
        query.observed_wall = wall;
        query.observed_monotonic = monotonic;
        Ok(AccessQueryObservation {
            owner: SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            query: Arc::clone(&query.identity),
            floor: query.floor,
            deadline: query.deadline,
            wall,
            monotonic,
        })
    }
}

fn check_access_host(
    authority: &Authority,
    sessions: &super::Sessions,
    host: &Arc<SettingsHostIdentity>,
    source: &AuthenticatedSettingsAccessSource,
    stop: &CancellationToken,
) -> Result<(), SettingsServiceError> {
    if sessions.closed || stop.is_cancelled() {
        return Err(SettingsServiceError::Stopped);
    }
    if sessions
        .paired
        .host
        .as_ref()
        .is_none_or(|registered| !Arc::ptr_eq(registered, host))
        || !Arc::ptr_eq(source.connection().host_identity(), host)
        || host.host_id() != authority.host_id
        || source.intent().host_id != authority.host_id
        || !host.is_live()
        || !source.connection().is_live()
    {
        return Err(SettingsServiceError::OutsideScope);
    }
    Ok(())
}

pub(in crate::host::settings) struct ReadQueryRegistration {
    pub(in crate::host::settings) disposition: SettingsReadQueryDisposition,
    pub(in crate::host::settings) query: Arc<()>,
}

pub(in crate::host::settings) struct ReadQueryObservation {
    pub(in crate::host::settings) owner: SettingsGrantOwner,
    pub(in crate::host::settings) query: Arc<()>,
    pub(in crate::host::settings) floor: (UnixMillis, Instant),
    pub(in crate::host::settings) deadline: Instant,
    pub(in crate::host::settings) parent_lifetime: BindingLifetime,
    pub(in crate::host::settings) wall: UnixMillis,
    pub(in crate::host::settings) monotonic: Instant,
}

impl Authority {
    pub(in crate::host::settings) fn register_read_query(
        &self,
        host: &Arc<SettingsHostIdentity>,
        store: &Store,
        source: &AuthenticatedSettingsReadSource,
        stop: &CancellationToken,
    ) -> Result<ReadQueryRegistration, SettingsReadQualificationError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_read_host(self, &sessions, host, source, stop)?;
        let (wall, monotonic) = source.check_precommit(store)?;
        let paired = &mut sessions.paired;
        if let Some(original) = &paired.access_connection {
            if !Arc::ptr_eq(original, source.connection()) {
                if original.is_live()
                    || !Arc::ptr_eq(original.host_identity(), host)
                    || original.connection_epoch() == source.connection().connection_epoch()
                {
                    return Err(SettingsServiceError::Conflict.into());
                }
                paired.access_queries.clear();
                paired.access_connection = Some(Arc::clone(source.connection()));
            }
        } else {
            if !paired.access_queries.is_empty() {
                return Err(SettingsServiceError::CorruptJournal.into());
            }
            paired.access_connection = Some(Arc::clone(source.connection()));
        }
        let (_, query_id, _, issued_at, expires_at) = read_request_parts(source.request());
        let peer = source.verified_peer_keys();
        let key = (peer.device_id, query_id);
        let intent = SettingsQueryIntent::from_read(source.request());
        if let Some(original) = paired.access_queries.get_mut(&key) {
            if original.intent != intent || original.peer != *peer {
                return Err(SettingsServiceError::Conflict.into());
            }
            let floor = source.receipt_floor();
            if wall < original.floor.0
                || monotonic < original.floor.1
                || wall < original.observed_wall
                || monotonic < original.observed_monotonic
                || wall < floor.0
                || monotonic < floor.1
                || wall < issued_at
            {
                return Err(SettingsServiceError::OutsideScope.into());
            }
            if wall >= expires_at
                || monotonic >= original.deadline
                || monotonic >= source.original_deadline()
            {
                return Err(SettingsServiceError::Expired.into());
            }
            original.observed_wall = wall;
            original.observed_monotonic = monotonic;
            return Ok(ReadQueryRegistration {
                disposition: SettingsReadQueryDisposition::Known,
                query: Arc::clone(&original.identity),
            });
        }
        if paired.access_queries.len() >= MAX_ACCESS_QUERIES
            || paired
                .access_queries
                .values()
                .filter(|query| query.peer == *peer)
                .count()
                >= MAX_PEER_ACCESS_QUERIES
        {
            return Err(SettingsServiceError::Capacity.into());
        }
        let floor = source.receipt_floor();
        let identity = Arc::new(());
        paired.access_queries.insert(
            key,
            AccessQuery {
                identity: Arc::clone(&identity),
                intent,
                peer: peer.clone(),
                floor,
                deadline: source.original_deadline(),
                observed_wall: wall,
                observed_monotonic: monotonic,
            },
        );
        Ok(ReadQueryRegistration {
            disposition: SettingsReadQueryDisposition::First,
            query: identity,
        })
    }

    pub(in crate::host::settings) fn observe_read_query(
        &self,
        host: &Arc<SettingsHostIdentity>,
        store: &Store,
        source: &AuthenticatedSettingsReadSource,
        stop: &CancellationToken,
    ) -> Result<ReadQueryObservation, SettingsReadQualificationError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_read_host(self, &sessions, host, source, stop)?;
        let (wall, monotonic) = source.check_precommit(store)?;
        let (host_id, query_id, parent, issued_at, expires_at) =
            read_request_parts(source.request());
        let paired = &mut sessions.paired;
        if paired
            .access_connection
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, source.connection()))
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let entry = paired
            .entries
            .values_mut()
            .find(|entry| entry.record.id == parent.parent_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        if record.intent.host_id != host_id
            || record.intent.generation != parent.generation
            || record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.connection, source.connection())
            || !record.connection.is_live()
            || record.cancellation.is_cancelled()
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let query = paired
            .access_queries
            .get_mut(&(source.verified_peer_keys().device_id, query_id))
            .ok_or(SettingsServiceError::NotFound)?;
        if query.intent != SettingsQueryIntent::from_read(source.request())
            || query.peer != *source.verified_peer_keys()
        {
            return Err(SettingsServiceError::Conflict.into());
        }
        let floor = source.receipt_floor();
        if wall < query.floor.0
            || monotonic < query.floor.1
            || wall < floor.0
            || monotonic < floor.1
            || wall < query.observed_wall
            || monotonic < query.observed_monotonic
            || wall < issued_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if wall >= expires_at
            || monotonic >= query.deadline
            || monotonic >= source.original_deadline()
            || wall >= record.lifetime.expires_at
            || monotonic >= record.lifetime.deadline
        {
            return Err(SettingsServiceError::Expired.into());
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        query.observed_wall = wall;
        query.observed_monotonic = monotonic;
        Ok(ReadQueryObservation {
            owner: SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            query: Arc::clone(&query.identity),
            floor: query.floor,
            deadline: query.deadline,
            parent_lifetime: record.lifetime,
            wall,
            monotonic,
        })
    }
}

fn check_read_host(
    authority: &Authority,
    sessions: &super::Sessions,
    host: &Arc<SettingsHostIdentity>,
    source: &AuthenticatedSettingsReadSource,
    stop: &CancellationToken,
) -> Result<(), SettingsServiceError> {
    if sessions.closed || stop.is_cancelled() {
        return Err(SettingsServiceError::Stopped);
    }
    let (host_id, _, _, _, _) = read_request_parts(source.request());
    if sessions
        .paired
        .host
        .as_ref()
        .is_none_or(|registered| !Arc::ptr_eq(registered, host))
        || !Arc::ptr_eq(source.connection().host_identity(), host)
        || host.host_id() != authority.host_id
        || host_id != authority.host_id
        || !host.is_live()
        || !source.connection().is_live()
    {
        return Err(SettingsServiceError::OutsideScope);
    }
    Ok(())
}

fn read_request_parts(
    request: &SettingsReadRequest,
) -> (
    DeviceId,
    Uuid,
    &SettingsParentReference,
    UnixMillis,
    UnixMillis,
) {
    match request {
        SettingsReadRequest::Snapshot(intent) => (
            intent.host_id,
            intent.query_id,
            &intent.parent,
            intent.issued_at,
            intent.expires_at,
        ),
        SettingsReadRequest::InspectOriginal(intent) => (
            intent.host_id,
            intent.query_id,
            &intent.parent,
            intent.issued_at,
            intent.expires_at,
        ),
    }
}

impl Authority {
    pub(in crate::host::settings) fn observe_committed_read_query(
        &self,
        host: &Arc<SettingsHostIdentity>,
        request: &OwnedCommittedSettingsRead,
        stop: &CancellationToken,
    ) -> Result<ReadQueryObservation, SettingsServiceError> {
        let source = request.verified_request().authority_source();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_read_host(self, &sessions, host, source, stop)?;
        let wall = UnixMillis::now()?;
        let monotonic = Instant::now();
        let (host_id, query_id, parent, issued_at, expires_at) =
            read_request_parts(source.request());
        let paired = &mut sessions.paired;
        if paired
            .access_connection
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, source.connection()))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let entry = paired
            .entries
            .values_mut()
            .find(|entry| entry.record.id == parent.parent_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        let record = &entry.record;
        if record.intent.host_id != host_id
            || record.intent.generation != parent.generation
            || record.peer != *source.verified_peer_keys()
            || !Arc::ptr_eq(&record.connection, source.connection())
            || !record.connection.is_live()
            || record.cancellation.is_cancelled()
            || wall < entry.observed_wall
            || monotonic < entry.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let query = paired
            .access_queries
            .get_mut(&(source.verified_peer_keys().device_id, query_id))
            .ok_or(SettingsServiceError::NotFound)?;
        if query.intent != SettingsQueryIntent::from_read(source.request())
            || query.peer != *source.verified_peer_keys()
        {
            return Err(SettingsServiceError::Conflict);
        }
        let floor = source.receipt_floor();
        if query.floor != floor
            || wall < query.floor.0
            || monotonic < query.floor.1
            || wall < query.observed_wall
            || monotonic < query.observed_monotonic
            || wall < issued_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if wall >= expires_at
            || monotonic >= query.deadline
            || monotonic >= source.original_deadline()
            || wall >= record.lifetime.expires_at
            || monotonic >= record.lifetime.deadline
        {
            return Err(SettingsServiceError::Expired);
        }
        entry.observed_wall = wall;
        entry.observed_monotonic = monotonic;
        query.observed_wall = wall;
        query.observed_monotonic = monotonic;
        Ok(ReadQueryObservation {
            owner: SettingsGrantOwner {
                host_id: self.host_id,
                principal_id: self.owner_principal,
            },
            query: Arc::clone(&query.identity),
            floor: query.floor,
            deadline: query.deadline,
            parent_lifetime: record.lifetime,
            wall,
            monotonic,
        })
    }
}

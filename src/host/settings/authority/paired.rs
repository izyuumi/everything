use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    AuthenticatedSettingsCommandSource, BindingHistoryState, BindingIntent, BindingLifetime,
    OwnedCommittedSettingsBind, SettingsConnectionIdentity, SettingsHostIdentity,
};
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::Store;

use super::super::grants::SettingsGrantOwner;
use super::super::service::SettingsServiceError;
use super::Authority;

const MAX_PAIRED_PARENTS: usize = 32;
const MAX_PEER_CONNECTION_PARENTS: usize = 4;

#[derive(Default)]
pub(super) struct PairedParents {
    host: Option<Arc<SettingsHostIdentity>>,
    entries: BTreeMap<(Uuid, DeviceId, Uuid), ParentEntry>,
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

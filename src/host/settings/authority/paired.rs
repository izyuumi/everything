use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    AuthenticatedSettingsAccessSource, SettingsAccessStatusIntent, SettingsConnectionIdentity,
    SettingsHostIdentity,
};
use crate::protocol::{DeviceId, UnixMillis};

use super::super::grants::SettingsGrantOwner;
use super::super::service::SettingsServiceError;
use super::Authority;

const MAX_ACCESS_QUERIES: usize = 512;
const MAX_PEER_ACCESS_QUERIES: usize = 64;

#[derive(Default)]
pub(super) struct PairedTransport {
    host: Option<Arc<SettingsHostIdentity>>,
    access_connection: Option<Arc<SettingsConnectionIdentity>>,
    access_queries: BTreeMap<(DeviceId, Uuid), AccessQuery>,
}

impl PairedTransport {
    pub(super) fn close(&mut self) {
        self.access_queries.clear();
    }
}

struct AccessQuery {
    identity: Arc<()>,
    intent: SettingsAccessStatusIntent,
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
        let now = Instant::now();
        paired
            .access_queries
            .retain(|_, query| query.deadline > now);
        let intent = source.intent();
        let peer = source.verified_peer_keys();
        let key = (peer.device_id, intent.query_id);
        if let Some(original) = paired.access_queries.get(&key) {
            if original.intent != *intent || original.peer != *peer {
                return Err(SettingsServiceError::OutsideScope);
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
                intent: intent.clone(),
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
        let Some(query) = paired
            .access_queries
            .get_mut(&(peer.device_id, intent.query_id))
        else {
            return Err(if monotonic >= source.original_deadline() {
                SettingsServiceError::Expired
            } else {
                SettingsServiceError::NotFound
            });
        };
        if query.intent != *intent || query.peer != *peer {
            return Err(SettingsServiceError::OutsideScope);
        }
        let floor = source.receipt_floor();
        if wall < query.floor.0
            || monotonic < query.floor.1
            || wall < floor.0
            || monotonic < floor.1
            || wall < query.observed_wall
            || monotonic < query.observed_monotonic
            || wall
                .get()
                .saturating_add(crate::devices::SETTINGS_CLOCK_SKEW_MS)
                < intent.issued_at.get()
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

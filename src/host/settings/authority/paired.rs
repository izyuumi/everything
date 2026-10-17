use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rand_core::{OsRng, RngCore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    AuthenticatedSettingsAccessSource, SettingsAccessStatusIntent, SettingsConnectionIdentity,
    SettingsHostIdentity,
};
use crate::protocol::{DeviceId, UnixMillis};

use super::super::grants::{SettingsGrantOwner, SettingsGrantReference};
use super::super::service::SettingsServiceError;
use super::{Authority, CallerSource, SettingsAgentSession, SettingsCaller};

const MAX_ACCESS_QUERIES: usize = 512;
const MAX_PEER_ACCESS_QUERIES: usize = 64;
const MAX_PAIRED_SESSION_PARENTS: usize = 4;
const PAIRED_SESSION_LIFETIME_MS: u64 = 300_000;

pub(super) struct PairedSessionParent {
    peer: DevicePublicKeys,
    grant: SettingsGrantReference,
}

#[derive(Default)]
pub(super) struct PairedTransport {
    host: Option<Arc<SettingsHostIdentity>>,
    access_connection: Option<Arc<SettingsConnectionIdentity>>,
    access_queries: BTreeMap<(DeviceId, Uuid), AccessQuery>,
    caller: Option<Arc<PairedCallerLease>>,
}

pub(in crate::host::settings) struct PairedCallerLease {
    device_id: DeviceId,
    connection: Arc<SettingsConnectionIdentity>,
    expires_at: UnixMillis,
    deadline: Instant,
}

impl PairedTransport {
    pub(super) fn close(&mut self) {
        self.access_queries.clear();
        self.caller = None;
    }

    pub(super) fn check_caller_origin(
        &self,
        device_id: DeviceId,
    ) -> Result<(), SettingsServiceError> {
        let lease = self
            .caller
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?;
        if lease.device_id != device_id {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.check_lease(lease)
    }

    fn check_lease(&self, lease: &PairedCallerLease) -> Result<(), SettingsServiceError> {
        if !lease.connection.is_live()
            || self
                .host
                .as_ref()
                .is_none_or(|host| !Arc::ptr_eq(host, lease.connection.host_identity()))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if UnixMillis::now()? >= lease.expires_at || Instant::now() >= lease.deadline {
            return Err(SettingsServiceError::Expired);
        }
        Ok(())
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
    pub(in crate::host::settings) fn paired_caller(
        self: &Arc<Self>,
        host: &Arc<SettingsHostIdentity>,
        source: &AuthenticatedSettingsAccessSource,
        stop: &CancellationToken,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        check_access_host(self, &sessions, host, source, stop)?;
        if sessions
            .paired
            .access_connection
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, source.connection()))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let device_id = source.verified_peer_keys().device_id;
        let lease = Arc::new(PairedCallerLease {
            device_id,
            connection: Arc::clone(source.connection()),
            expires_at: source.intent().expires_at,
            deadline: source.original_deadline(),
        });
        sessions.paired.check_lease(&lease)?;
        sessions.paired.caller = Some(Arc::clone(&lease));
        Ok(SettingsCaller {
            authority: Arc::clone(self),
            source: CallerSource::PairedSettings { device_id },
            agent_identity: None,
            paired: Some(lease),
        })
    }

    pub(in crate::host::settings) fn retire_paired_caller(&self, caller: &SettingsCaller) {
        if let Ok(mut sessions) = self.sessions.lock()
            && let (Some(current), Some(lease)) = (&sessions.paired.caller, &caller.paired)
            && Arc::ptr_eq(current, lease)
        {
            sessions.paired.caller = None;
        }
    }

    pub(in crate::host::settings) fn validate_paired_update(
        &self,
        caller: &SettingsCaller,
    ) -> Result<(), SettingsServiceError> {
        self.paired_lifetime(caller).map(|_| ())
    }

    pub(in crate::host::settings) fn paired_lifetime(
        &self,
        caller: &SettingsCaller,
    ) -> Result<(UnixMillis, Instant), SettingsServiceError> {
        let CallerSource::PairedSettings { device_id } = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let lease = caller
            .paired
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        if lease.device_id != device_id
            || sessions
                .paired
                .caller
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, lease))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        sessions.paired.check_lease(lease)?;
        Ok((lease.expires_at, lease.deadline))
    }

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

impl Authority {
    pub(in crate::host::settings) fn paired_session_parent(
        &self,
        peer: &DevicePublicKeys,
        grant: SettingsGrantReference,
        grant_expires_at: UnixMillis,
    ) -> Result<Uuid, SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        let now = UnixMillis::now()?;
        if let Some((id, session)) = sessions.values.iter().find(|(_, session)| {
            session
                .device
                .as_ref()
                .is_some_and(|parent| parent.peer.device_id == peer.device_id)
        }) && !session.closed
            && session.live(now)
            && session
                .device
                .as_ref()
                .is_some_and(|parent| parent.peer == *peer && parent.grant == grant)
            && sessions
                .agents
                .values()
                .any(|agent| agent.native_session_id == *id && !agent.cancellation.is_cancelled())
        {
            return Ok(*id);
        }
        retire_device(&mut sessions, peer.device_id);
        sessions.values.retain(|_, session| {
            session.admitted.load(Ordering::Acquire) != 0 || (!session.closed && session.live(now))
        });
        if sessions.values.len() >= super::MAX_NATIVE_SESSIONS
            || sessions
                .values
                .values()
                .filter(|session| session.paired())
                .count()
                >= MAX_PAIRED_SESSION_PARENTS
        {
            return Err(SettingsServiceError::Capacity);
        }
        let remaining = grant_expires_at
            .get()
            .checked_sub(now.get())
            .filter(|remaining| *remaining > 0)
            .ok_or(SettingsServiceError::Expired)?
            .min(PAIRED_SESSION_LIFETIME_MS);
        let expires_at = UnixMillis::try_from(
            now.get()
                .checked_add(remaining)
                .ok_or(SettingsServiceError::Invalid("session expiry"))?,
        )?;
        let mut secret_hash = [0; 32];
        OsRng
            .try_fill_bytes(&mut secret_hash)
            .map_err(|_| SettingsServiceError::Entropy)?;
        let id = Uuid::new_v4();
        match sessions.values.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(super::Session {
                    secret_hash,
                    expires_at,
                    deadline: Instant::now() + Duration::from_millis(remaining),
                    closed: false,
                    cancellation: CancellationToken::new(),
                    admitted: Arc::new(AtomicUsize::new(0)),
                    device: Some(PairedSessionParent {
                        peer: peer.clone(),
                        grant,
                    }),
                });
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(SettingsServiceError::Conflict);
            }
        }
        Ok(id)
    }

    pub(in crate::host::settings) fn current_paired_session_parent(
        &self,
        device_id: DeviceId,
        grant: SettingsGrantReference,
    ) -> Result<Uuid, SettingsServiceError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        sessions
            .values
            .iter()
            .find(|(_, session)| {
                session.device.as_ref().is_some_and(|parent| {
                    parent.peer.device_id == device_id && parent.grant == grant
                })
            })
            .map(|(id, _)| *id)
            .ok_or(SettingsServiceError::NotFound)
    }

    pub(in crate::host::settings) fn begin_paired_agent(
        self: &Arc<Self>,
        parent: Uuid,
    ) -> Result<SettingsAgentSession, SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if !sessions
            .values
            .get(&parent)
            .ok_or(SettingsServiceError::OutsideScope)?
            .paired()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        sessions
            .agents
            .retain(|_, session| !session.cancellation.is_cancelled());
        if sessions
            .agents
            .values()
            .any(|agent| agent.native_session_id == parent)
        {
            return Err(SettingsServiceError::Capacity);
        }
        self.insert_agent(&mut sessions, parent)
    }

    pub(in crate::host::settings) fn paired_agent_grant(
        &self,
        caller: &SettingsCaller,
    ) -> Result<Option<(DevicePublicKeys, SettingsGrantReference)>, SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let CallerSource::SettingsAgent {
            native_session_id, ..
        } = caller.source
        else {
            return Ok(None);
        };
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        Ok(sessions
            .values
            .get(&native_session_id)
            .ok_or(SettingsServiceError::OutsideScope)?
            .device
            .as_ref()
            .map(|parent| (parent.peer.clone(), parent.grant)))
    }

    pub(in crate::host::settings) fn paired_agent_session(
        &self,
        session: &SettingsAgentSession,
    ) -> Result<bool, SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&session.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        Ok(sessions
            .values
            .get(&session.native_session_id)
            .ok_or(SettingsServiceError::OutsideScope)?
            .paired())
    }

    pub(in crate::host::settings) fn retire_paired_sessions(
        &self,
        device_id: DeviceId,
    ) -> Result<(), SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        retire_device(&mut sessions, device_id);
        Ok(())
    }
}

fn retire_device(sessions: &mut super::Sessions, device_id: DeviceId) {
    for session in sessions.values.values_mut() {
        if session
            .device
            .as_ref()
            .is_some_and(|parent| parent.peer.device_id == device_id)
        {
            session.closed = true;
            session.cancellation.cancel();
        }
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

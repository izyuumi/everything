use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::protocol::{CommandId, DeviceId, UnixMillis};

use super::contract::SettingsToolName;
use crate::transport::local::LocalCall;

use super::service::SettingsServiceError;

const MAX_NATIVE_SESSIONS: usize = 16;
const SESSION_LIFETIME: Duration = Duration::from_secs(300);

pub struct SettingsCaller {
    pub(super) authority: Arc<Authority>,
    pub(super) source: CallerSource,
    agent_identity: Option<Arc<()>>,
}

#[derive(Clone, Copy)]
pub(super) enum CallerSource {
    LocalOwner {
        request_id: CommandId,
    },
    NativeUi(Uuid),
    SettingsAgent {
        session_id: Uuid,
        native_session_id: Uuid,
        tool: SettingsToolName,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsOperationOrigin {
    LocalOwner {
        request_id: CommandId,
    },
    NativeUi {
        session_id: Uuid,
    },
    SettingsAgent {
        session_id: Uuid,
        native_session_id: Uuid,
    },
    PairedSettings {
        actor_device_id: DeviceId,
        remote_parent_id: Uuid,
        grant_id: Uuid,
        grant_revision: u64,
        host_epoch: Uuid,
        connection_epoch: Uuid,
    },
    PairedSettingsAgent {
        actor_device_id: DeviceId,
        remote_parent_id: Uuid,
        grant_id: Uuid,
        grant_revision: u64,
        host_epoch: Uuid,
        connection_epoch: Uuid,
        session_id: Uuid,
    },
}

#[derive(Clone)]
pub(crate) struct SettingsAgentBinding {
    pub(crate) host_id: DeviceId,
    pub(crate) principal_id: Uuid,
    pub(crate) native_session_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) expires_at: UnixMillis,
    pub(crate) deadline: Instant,
    pub(crate) cancellation: CancellationToken,
}

pub struct SettingsAgentSession {
    authority: Arc<Authority>,
    id: Uuid,
    native_session_id: Uuid,
    identity: Arc<()>,
    cancellation: CancellationToken,
}

impl fmt::Debug for SettingsAgentSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAgentSession([restricted])")
    }
}

impl Drop for SettingsAgentSession {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl SettingsAgentSession {
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.child_token()
    }
}

impl fmt::Debug for SettingsCaller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsCaller([authenticated])")
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionProof {
    session_id: Uuid,
    secret: SessionSecret,
}

impl fmt::Debug for NativeSessionProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSessionProof([redacted])")
    }
}

impl NativeSessionProof {
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }
}

#[derive(Clone)]
struct SessionSecret(Arc<Zeroizing<String>>);

impl Serialize for SessionSecret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionSecret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = Zeroizing::new(String::deserialize(deserializer)?);
        if encoded.len() != 44 {
            return Err(serde::de::Error::custom("invalid native session proof"));
        }
        let decoded = Zeroizing::new(
            STANDARD
                .decode(encoded.as_bytes())
                .map_err(|_| serde::de::Error::custom("invalid native session proof"))?,
        );
        let canonical = Zeroizing::new(STANDARD.encode(&*decoded));
        if decoded.len() != 32 || canonical.as_str() != encoded.as_str() {
            return Err(serde::de::Error::custom("invalid native session proof"));
        }
        Ok(Self(Arc::new(encoded)))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSessionOpened {
    pub host_id: DeviceId,
    pub expires_at: UnixMillis,
    pub proof: NativeSessionProof,
}

impl fmt::Debug for NativeSessionOpened {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeSessionOpened([redacted])")
    }
}

pub(super) struct Authority {
    pub host_id: DeviceId,
    owner_uid: u32,
    owner_principal: Uuid,
    sessions: Mutex<Sessions>,
}

struct Sessions {
    closed: bool,
    values: BTreeMap<Uuid, Session>,
    agents: BTreeMap<Uuid, AgentSession>,
}

struct AgentSession {
    native_session_id: Uuid,
    identity: Arc<()>,
    cancellation: CancellationToken,
}

struct Session {
    secret_hash: [u8; 32],
    expires_at: UnixMillis,
    deadline: Instant,
    closed: bool,
    cancellation: CancellationToken,
    admitted: Arc<AtomicUsize>,
}

pub(super) struct SessionAdmission {
    admitted: Arc<AtomicUsize>,
    pub cancellation: CancellationToken,
}

impl SessionAdmission {
    pub fn retain(&self) -> Self {
        self.admitted.fetch_add(1, Ordering::AcqRel);
        Self {
            admitted: Arc::clone(&self.admitted),
            cancellation: self.cancellation.child_token(),
        }
    }
}

impl Drop for SessionAdmission {
    fn drop(&mut self) {
        self.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Authority {
    pub fn new(host_id: DeviceId) -> Arc<Self> {
        let owner_uid = rustix::process::geteuid().as_raw();
        Arc::new(Self {
            host_id,
            owner_uid,
            owner_principal: *host_id.as_uuid(),
            sessions: Mutex::new(Sessions {
                closed: false,
                values: BTreeMap::new(),
                agents: BTreeMap::new(),
            }),
        })
    }

    pub fn owner(
        self: &Arc<Self>,
        call: &LocalCall,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.check_channel(call)?;
        if self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .closed
        {
            return Err(SettingsServiceError::Stopped);
        }
        Ok(SettingsCaller {
            authority: Arc::clone(self),
            source: CallerSource::LocalOwner {
                request_id: call.request.request_id,
            },
            agent_identity: None,
        })
    }

    pub fn cleanup_owner(
        self: &Arc<Self>,
        call: &LocalCall,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.check_channel(call)?;
        Ok(SettingsCaller {
            authority: Arc::clone(self),
            source: CallerSource::LocalOwner {
                request_id: call.request.request_id,
            },
            agent_identity: None,
        })
    }

    pub fn validate_owner_cleanup(
        &self,
        caller: &SettingsCaller,
    ) -> Result<(), SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority))
            || !matches!(caller.source, CallerSource::LocalOwner { .. })
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    pub fn open(
        self: &Arc<Self>,
        call: &LocalCall,
    ) -> Result<NativeSessionOpened, SettingsServiceError> {
        self.check_channel(call)?;
        let now = UnixMillis::now()?;
        let expires_at = UnixMillis::try_from(
            now.get()
                .checked_add(300_000)
                .ok_or(SettingsServiceError::Invalid("session expiry"))?,
        )?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        sessions.values.retain(|_, session| {
            session.admitted.load(Ordering::Acquire) != 0 || (!session.closed && session.live(now))
        });
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        if sessions.values.len() >= MAX_NATIVE_SESSIONS {
            return Err(SettingsServiceError::Capacity);
        }
        let mut secret = Zeroizing::new([0; 32]);
        OsRng
            .try_fill_bytes(&mut *secret)
            .map_err(|_| SettingsServiceError::Entropy)?;
        let encoded = Zeroizing::new(STANDARD.encode(secret.as_slice()));
        let session_id = Uuid::new_v4();
        let session = Session {
            secret_hash: secret_hash(&encoded),
            expires_at,
            deadline: Instant::now() + SESSION_LIFETIME,
            closed: false,
            cancellation: CancellationToken::new(),
            admitted: Arc::new(AtomicUsize::new(0)),
        };
        match sessions.values.entry(session_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(session);
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(SettingsServiceError::Conflict);
            }
        }
        Ok(NativeSessionOpened {
            host_id: self.host_id,
            expires_at,
            proof: NativeSessionProof {
                session_id,
                secret: SessionSecret(Arc::new(encoded)),
            },
        })
    }

    pub fn native(
        self: &Arc<Self>,
        call: &LocalCall,
        proof: &NativeSessionProof,
        cleanup: bool,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.check_channel(call)?;
        let now = UnixMillis::now()?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let session = sessions
            .values
            .get(&proof.session_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if session.secret_hash != secret_hash(&proof.secret.0)
            || (!cleanup && (sessions.closed || session.closed || !session.live(now)))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(SettingsCaller {
            authority: Arc::clone(self),
            source: CallerSource::NativeUi(proof.session_id),
            agent_identity: None,
        })
    }

    pub fn validate(
        &self,
        caller: &SettingsCaller,
        cleanup: bool,
    ) -> Result<(), SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let now = UnixMillis::now()?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        match caller.source {
            CallerSource::LocalOwner { .. } if !sessions.closed && !cleanup => Ok(()),
            CallerSource::NativeUi(id) => {
                let session = sessions
                    .values
                    .get(&id)
                    .ok_or(SettingsServiceError::OutsideScope)?;
                if cleanup || !sessions.closed && !session.closed && session.live(now) {
                    Ok(())
                } else {
                    Err(SettingsServiceError::OutsideScope)
                }
            }
            CallerSource::SettingsAgent {
                session_id,
                native_session_id,
                ..
            } if !cleanup && !sessions.closed => {
                let agent = sessions
                    .agents
                    .get(&session_id)
                    .ok_or(SettingsServiceError::OutsideScope)?;
                let native = sessions
                    .values
                    .get(&native_session_id)
                    .ok_or(SettingsServiceError::OutsideScope)?;
                if agent.native_session_id != native_session_id
                    || caller
                        .agent_identity
                        .as_ref()
                        .is_none_or(|identity| !Arc::ptr_eq(identity, &agent.identity))
                    || agent.cancellation.is_cancelled()
                    || native.closed
                    || !native.live(now)
                {
                    return Err(SettingsServiceError::OutsideScope);
                }
                Ok(())
            }
            _ => Err(SettingsServiceError::OutsideScope),
        }
    }

    pub fn principal(&self) -> Uuid {
        self.owner_principal
    }

    pub fn session(&self, caller: &SettingsCaller) -> Result<Uuid, SettingsServiceError> {
        self.validate(caller, false)?;
        match caller.source {
            CallerSource::NativeUi(id) => Ok(id),
            CallerSource::SettingsAgent {
                native_session_id, ..
            } => Ok(native_session_id),
            CallerSource::LocalOwner { .. } => Err(SettingsServiceError::OutsideScope),
        }
    }

    pub fn begin_agent(
        self: &Arc<Self>,
        caller: &SettingsCaller,
    ) -> Result<SettingsAgentSession, SettingsServiceError> {
        self.validate(caller, false)?;
        let CallerSource::NativeUi(native_session_id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        sessions
            .agents
            .retain(|_, session| !session.cancellation.is_cancelled());
        if sessions.agents.len() >= MAX_NATIVE_SESSIONS {
            return Err(SettingsServiceError::Capacity);
        }
        let native = sessions
            .values
            .get(&native_session_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if sessions.closed || native.closed || !native.live(UnixMillis::now()?) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let cancellation = native.cancellation.child_token();
        let id = Uuid::new_v4();
        let identity = Arc::new(());
        match sessions.agents.entry(id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(AgentSession {
                    native_session_id,
                    identity: Arc::clone(&identity),
                    cancellation: cancellation.clone(),
                });
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(SettingsServiceError::Conflict);
            }
        }
        Ok(SettingsAgentSession {
            authority: Arc::clone(self),
            id,
            native_session_id,
            identity,
            cancellation,
        })
    }

    pub fn agent(
        self: &Arc<Self>,
        session: &SettingsAgentSession,
        tool: SettingsToolName,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        if !Arc::ptr_eq(self, &session.authority) || session.cancellation.is_cancelled() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let native_session_id = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .agents
            .get(&session.id)
            .ok_or(SettingsServiceError::OutsideScope)?
            .native_session_id;
        let caller = SettingsCaller {
            authority: Arc::clone(self),
            source: CallerSource::SettingsAgent {
                session_id: session.id,
                native_session_id,
                tool,
            },
            agent_identity: Some(Arc::clone(&session.identity)),
        };
        self.validate(&caller, false)?;
        Ok(caller)
    }

    pub(super) fn agent_binding(
        self: &Arc<Self>,
        session: &SettingsAgentSession,
    ) -> Result<SettingsAgentBinding, SettingsServiceError> {
        if !Arc::ptr_eq(self, &session.authority) || session.cancellation.is_cancelled() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let child = sessions
            .agents
            .get(&session.id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        let parent = sessions
            .values
            .get(&child.native_session_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if sessions.closed
            || child.native_session_id != session.native_session_id
            || !Arc::ptr_eq(&child.identity, &session.identity)
            || child.cancellation.is_cancelled()
            || parent.closed
            || !parent.live(UnixMillis::now()?)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(SettingsAgentBinding {
            host_id: self.host_id,
            principal_id: self.owner_principal,
            native_session_id: child.native_session_id,
            session_id: session.id,
            expires_at: parent.expires_at,
            deadline: parent.deadline,
            cancellation: child.cancellation.clone(),
        })
    }

    pub(super) fn close_agent(
        self: &Arc<Self>,
        session: &SettingsAgentSession,
    ) -> Result<(Uuid, Uuid), SettingsServiceError> {
        if !Arc::ptr_eq(self, &session.authority) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if let Some(child) = sessions.agents.get(&session.id) {
            if child.native_session_id != session.native_session_id
                || !Arc::ptr_eq(&child.identity, &session.identity)
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            child.cancellation.cancel();
        } else if !session.cancellation.is_cancelled() {
            return Err(SettingsServiceError::OutsideScope);
        }
        session.cancellation.cancel();
        Ok((session.id, session.native_session_id))
    }

    pub(super) fn agent_cancellation(
        &self,
        caller: &SettingsCaller,
    ) -> Result<Option<CancellationToken>, SettingsServiceError> {
        if !std::ptr::eq(self, Arc::as_ptr(&caller.authority)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let CallerSource::SettingsAgent {
            session_id,
            native_session_id,
            ..
        } = caller.source
        else {
            return Ok(None);
        };
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let child = sessions
            .agents
            .get(&session_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if child.native_session_id != native_session_id
            || caller
                .agent_identity
                .as_ref()
                .is_none_or(|identity| !Arc::ptr_eq(identity, &child.identity))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(Some(child.cancellation.clone()))
    }

    pub fn require_tool(
        &self,
        caller: &SettingsCaller,
        tool: SettingsToolName,
    ) -> Result<(), SettingsServiceError> {
        self.validate(caller, false)?;
        if let CallerSource::SettingsAgent { tool: actual, .. } = caller.source
            && actual != tool
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    pub fn origin(&self, caller: &SettingsCaller) -> SettingsOperationOrigin {
        match caller.source {
            CallerSource::LocalOwner { request_id } => {
                SettingsOperationOrigin::LocalOwner { request_id }
            }
            CallerSource::NativeUi(session_id) => SettingsOperationOrigin::NativeUi { session_id },
            CallerSource::SettingsAgent {
                session_id,
                native_session_id,
                ..
            } => SettingsOperationOrigin::SettingsAgent {
                session_id,
                native_session_id,
            },
        }
    }

    pub fn check_origin(
        &self,
        origin: &SettingsOperationOrigin,
    ) -> Result<(), SettingsServiceError> {
        let now = UnixMillis::now()?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if sessions.closed {
            return Err(SettingsServiceError::Stopped);
        }
        let native_id = match origin {
            SettingsOperationOrigin::NativeUi { session_id } => *session_id,
            SettingsOperationOrigin::SettingsAgent {
                session_id,
                native_session_id,
            } => {
                let agent = sessions
                    .agents
                    .get(session_id)
                    .ok_or(SettingsServiceError::OutsideScope)?;
                if agent.native_session_id != *native_session_id
                    || agent.cancellation.is_cancelled()
                {
                    return Err(SettingsServiceError::OutsideScope);
                }
                *native_session_id
            }
            SettingsOperationOrigin::LocalOwner { .. } => return Ok(()),
            SettingsOperationOrigin::PairedSettings { .. }
            | SettingsOperationOrigin::PairedSettingsAgent { .. } => {
                return Err(SettingsServiceError::OutsideScope);
            }
        };
        let native = sessions
            .values
            .get(&native_id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if native.closed || !native.live(now) {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    pub fn lifetime(
        &self,
        caller: &SettingsCaller,
    ) -> Result<(UnixMillis, Instant), SettingsServiceError> {
        self.validate(caller, false)?;
        let id = self.session(caller)?;
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let session = sessions
            .values
            .get(&id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        Ok((session.expires_at, session.deadline))
    }

    pub fn admit(
        self: &Arc<Self>,
        caller: &SettingsCaller,
    ) -> Result<SessionAdmission, SettingsServiceError> {
        self.validate(caller, false)?;
        let CallerSource::NativeUi(id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let session = sessions
            .values
            .get_mut(&id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        if session.closed || !session.live(UnixMillis::now()?) {
            return Err(SettingsServiceError::OutsideScope);
        }
        session.admitted.fetch_add(1, Ordering::AcqRel);
        Ok(SessionAdmission {
            admitted: Arc::clone(&session.admitted),
            cancellation: session.cancellation.child_token(),
        })
    }

    pub fn close_session(&self, caller: &SettingsCaller) -> Result<(), SettingsServiceError> {
        self.validate(caller, true)?;
        let CallerSource::NativeUi(id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let session = sessions
            .values
            .get_mut(&id)
            .ok_or(SettingsServiceError::OutsideScope)?;
        session.closed = true;
        session.cancellation.cancel();
        Ok(())
    }

    pub fn close(&self) -> Result<(), SettingsServiceError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        sessions.closed = true;
        for session in sessions.values.values_mut() {
            session.closed = true;
            session.cancellation.cancel();
        }
        Ok(())
    }

    fn check_channel(&self, call: &LocalCall) -> Result<(), SettingsServiceError> {
        if call.peer_uid != self.owner_uid || call.cancellation.is_cancelled() {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }
}

impl Session {
    fn live(&self, now: UnixMillis) -> bool {
        now.get() < self.expires_at.get() && Instant::now() < self.deadline
    }
}

fn secret_hash(secret: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"everything.settings.native-session.v1\0");
    digest.update(secret.as_bytes());
    digest.finalize().into()
}

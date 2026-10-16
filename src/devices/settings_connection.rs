use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rand_core::{OsRng, RngCore};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{DeviceEndpoint, DeviceError, EndpointRole};
use crate::agent::Agent;
use crate::protocol::DeviceId;
use crate::transport::remote::RelayConnection;

pub(crate) struct SettingsHostIdentity {
    host_id: DeviceId,
    host_epoch: Uuid,
    closed: AtomicBool,
    cancellation: CancellationToken,
}

pub(crate) struct SettingsConnectionIdentity {
    host: Arc<SettingsHostIdentity>,
    connection_epoch: Uuid,
    retired: AtomicBool,
    cancellation: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct HostSettingsTransport {
    identity: Arc<SettingsHostIdentity>,
}

pub(crate) struct HostSettingsTransportOwner {
    identity: Arc<SettingsHostIdentity>,
    agent: Agent,
    endpoint: DeviceEndpoint,
    mutation: Arc<Mutex<()>>,
}

pub(super) struct SettingsConnectionOwner<'owner> {
    identity: Arc<SettingsConnectionIdentity>,
    _owner: &'owner mut HostSettingsTransportOwner,
}

impl SettingsHostIdentity {
    pub(crate) fn host_id(&self) -> DeviceId {
        self.host_id
    }

    pub(crate) fn host_epoch(&self) -> Uuid {
        self.host_epoch
    }

    pub(crate) fn is_live(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancellation.cancel();
    }
}

impl SettingsConnectionIdentity {
    pub(crate) fn host_identity(&self) -> &Arc<SettingsHostIdentity> {
        &self.host
    }

    pub(crate) fn connection_epoch(&self) -> Uuid {
        self.connection_epoch
    }

    pub(crate) fn is_live(&self) -> bool {
        self.host.is_live()
            && !self.retired.load(Ordering::Acquire)
            && !self.cancellation.is_cancelled()
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.child_token()
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.cancellation.cancel();
    }
}

impl HostSettingsTransport {
    pub(crate) fn host_identity(&self) -> &Arc<SettingsHostIdentity> {
        &self.identity
    }
}

impl HostSettingsTransportOwner {
    #[cfg(unix)]
    pub(crate) fn from_host(
        agent: Agent,
        endpoint: DeviceEndpoint,
        mutation: Arc<Mutex<()>>,
    ) -> Result<(HostSettingsTransport, Self), DeviceError> {
        if endpoint.role() != &EndpointRole::Host
            || !endpoint.shares_store(&agent.store())
            || agent.state_lease().is_none()
        {
            return Err(DeviceError::Invalid(
                "settings transport requires its original leased Host context",
            ));
        }
        let identity = Arc::new(SettingsHostIdentity {
            host_id: endpoint.public_keys().device_id,
            host_epoch: new_epoch()?,
            closed: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        });
        Ok((
            HostSettingsTransport {
                identity: Arc::clone(&identity),
            },
            Self {
                identity,
                agent,
                endpoint,
                mutation,
            },
        ))
    }

    pub(super) fn connected_relay<'owner>(
        &'owner mut self,
        endpoint: &DeviceEndpoint,
        _connection: &RelayConnection,
    ) -> Result<SettingsConnectionOwner<'owner>, DeviceError> {
        if !self.identity.is_live() || !Arc::ptr_eq(&self.endpoint.inner, &endpoint.inner) {
            self.identity.close();
            return Err(DeviceError::Invalid(
                "settings transport connection owner does not match endpoint",
            ));
        }
        let connection_epoch = match new_epoch() {
            Ok(epoch) => epoch,
            Err(error) => {
                self.identity.close();
                return Err(error);
            }
        };
        let identity = Arc::new(SettingsConnectionIdentity {
            host: Arc::clone(&self.identity),
            connection_epoch,
            retired: AtomicBool::new(false),
            cancellation: self.identity.cancellation.child_token(),
        });
        Ok(SettingsConnectionOwner {
            identity,
            _owner: self,
        })
    }
}

impl SettingsConnectionOwner<'_> {
    pub(super) fn identity(&self) -> &Arc<SettingsConnectionIdentity> {
        &self.identity
    }
}

impl Drop for SettingsConnectionOwner<'_> {
    fn drop(&mut self) {
        self.identity.retire();
    }
}

impl Drop for HostSettingsTransportOwner {
    fn drop(&mut self) {
        self.identity.close();
    }
}

impl fmt::Debug for SettingsHostIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsHostIdentity([local lifetime])")
    }
}

impl fmt::Debug for SettingsConnectionIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsConnectionIdentity([local lifetime])")
    }
}

impl fmt::Debug for HostSettingsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostSettingsTransport([local lifetime])")
    }
}

impl fmt::Debug for HostSettingsTransportOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HostSettingsTransportOwner([local lifetime])")
    }
}

impl fmt::Debug for SettingsConnectionOwner<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsConnectionOwner([local lifetime])")
    }
}

fn new_epoch() -> Result<Uuid, DeviceError> {
    let mut bytes = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| DeviceError::Invalid("settings connection randomness is unavailable"))?;
    Ok(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

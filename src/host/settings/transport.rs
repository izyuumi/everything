use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::Agent;
#[cfg(unix)]
use crate::devices::SettingsHostIdentity;
use crate::devices::{
    DeviceEndpoint, DeviceError, HostSettingsTransport, HostSettingsTransportOwner,
};

#[cfg(unix)]
use super::service::{SettingsReceiptAuthority, SettingsService, SettingsServiceError};

#[cfg(unix)]
pub(crate) struct SettingsRelayCapability {
    service: SettingsService,
    transport: HostSettingsTransport,
}

#[cfg(unix)]
impl SettingsRelayCapability {
    pub(in crate::host) fn new(service: SettingsService, transport: HostSettingsTransport) -> Self {
        Self { service, transport }
    }

    pub(crate) fn matches_host(&self, identity: &Arc<SettingsHostIdentity>) -> bool {
        Arc::ptr_eq(self.transport.host_identity(), identity)
    }

    pub(crate) fn receipt_authority(
        &self,
    ) -> Result<SettingsReceiptAuthority, SettingsServiceError> {
        self.service.paired_receipt_authority(&self.transport)
    }
}

pub(in crate::host) fn host_settings_transport(
    agent: Agent,
    endpoint: DeviceEndpoint,
    mutation: Arc<Mutex<()>>,
) -> Result<(HostSettingsTransport, HostSettingsTransportOwner), DeviceError> {
    HostSettingsTransportOwner::from_host(agent, endpoint, mutation)
}

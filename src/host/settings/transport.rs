use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::Agent;
use crate::devices::{
    DeviceEndpoint, DeviceError, HostSettingsTransport, HostSettingsTransportOwner,
};

pub(in crate::host) fn host_settings_transport(
    agent: Agent,
    endpoint: DeviceEndpoint,
    mutation: Arc<Mutex<()>>,
) -> Result<(HostSettingsTransport, HostSettingsTransportOwner), DeviceError> {
    HostSettingsTransportOwner::from_host(agent, endpoint, mutation)
}

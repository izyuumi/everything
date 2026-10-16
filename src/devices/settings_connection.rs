use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rand_core::{OsRng, RngCore};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(unix)]
use super::settings_ingress::{
    ReceiveContext,
    work::{
        OwnedQueuedSettingsReply, PairedResultAdmissionFailure, PendingSettingsReplyBuild,
        SettingsReceiveWork, SettingsReceiveWorkEvent,
    },
};
use super::{DeviceEndpoint, DeviceError, EndpointRole};
#[cfg(unix)]
use super::{
    PendingSettingsReceipt, SettingsReceiveDrain, SettingsReceiveError, SettingsReceiveWitness,
};
use crate::agent::Agent;
#[cfg(unix)]
use crate::crypto::EncryptedEnvelope;
#[cfg(unix)]
use crate::host::settings::agent::{PairedResultHandoff, SettingsAgentRuntime};
use crate::protocol::DeviceId;
#[cfg(unix)]
use crate::protocol::MessageId;
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
    #[cfg(unix)]
    receive: SettingsReceiveWitness,
}

pub(crate) struct HostSettingsTransportOwner {
    identity: Arc<SettingsHostIdentity>,
    agent: Agent,
    endpoint: DeviceEndpoint,
    mutation: Arc<Mutex<()>>,
    #[cfg(unix)]
    receive: SettingsReceiveWork,
}

pub(super) struct SettingsConnectionOwner<'owner> {
    identity: Arc<SettingsConnectionIdentity>,
    owner: &'owner mut HostSettingsTransportOwner,
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
    pub(crate) fn close_intake(&self) {
        self.identity.close();
    }

    #[cfg(unix)]
    pub(crate) fn receive_witness(&self) -> SettingsReceiveWitness {
        self.receive.clone()
    }

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
        let receive = SettingsReceiveWork::new();
        let witness = receive.witness();
        Ok((
            HostSettingsTransport {
                identity: Arc::clone(&identity),
                receive: witness,
            },
            Self {
                identity,
                agent,
                endpoint,
                mutation,
                receive,
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
        #[cfg(unix)]
        {
            let context = Arc::new(ReceiveContext {
                agent: self.agent.clone(),
                endpoint: self.endpoint.clone(),
                mutation: Arc::clone(&self.mutation),
            });
            if self
                .receive
                .connect(Arc::clone(&identity), context)
                .is_err()
            {
                identity.retire();
                self.identity.close();
                self.receive.latch_failure();
                return Err(DeviceError::Worker);
            }
        }
        Ok(SettingsConnectionOwner {
            identity,
            owner: self,
        })
    }
}

impl SettingsConnectionOwner<'_> {
    #[cfg(unix)]
    pub(super) fn try_receive(
        &mut self,
        envelope: EncryptedEnvelope,
        receipt_authority: crate::host::settings::service::SettingsReceiptAuthority,
    ) -> Result<PendingSettingsReceipt, SettingsReceiveError> {
        let context = self.owner.receive.context()?;
        self.owner.receive.admit(
            context,
            Arc::clone(&self.identity),
            envelope,
            receipt_authority,
        )
    }

    #[cfg(unix)]
    pub(super) async fn next_receive_event(
        &mut self,
    ) -> Result<Option<SettingsReceiveWorkEvent>, SettingsReceiveError> {
        self.owner.receive.next().await
    }

    #[cfg(unix)]
    pub(super) fn try_adopt_paired_result(
        &mut self,
        handoff: PairedResultHandoff,
    ) -> Result<PendingSettingsReplyBuild, Box<PairedResultAdmissionFailure>> {
        self.owner
            .receive
            .try_adopt_paired_result(&self.identity, handoff)
    }

    #[cfg(unix)]
    pub(super) fn try_start_reply_build(
        &mut self,
        ticket: &PendingSettingsReplyBuild,
        authority: crate::host::settings::service::SettingsReceiptAuthority,
    ) -> Result<(), SettingsReceiveError> {
        self.owner
            .receive
            .try_start_reply_build(&self.identity, ticket, authority)
    }

    #[cfg(unix)]
    pub(super) fn try_queue_joined_reply(
        &mut self,
        ticket: &PendingSettingsReplyBuild,
    ) -> Result<MessageId, SettingsReceiveError> {
        self.owner
            .receive
            .try_queue_joined_reply(&self.identity, ticket)
    }

    #[cfg(unix)]
    pub(super) fn queued_reply(
        &self,
        message_id: MessageId,
    ) -> Result<Option<&OwnedQueuedSettingsReply>, SettingsReceiveError> {
        self.owner.receive.queued_reply(message_id)
    }

    #[cfg(unix)]
    pub(super) fn try_ack_paired_adoption(
        &mut self,
        runtime: &SettingsAgentRuntime,
    ) -> Result<bool, SettingsReceiveError> {
        self.owner.receive.try_ack_paired_adoption(runtime)
    }

    pub(super) fn identity(&self) -> &Arc<SettingsConnectionIdentity> {
        &self.identity
    }
}

impl Drop for SettingsConnectionOwner<'_> {
    fn drop(&mut self) {
        self.identity.retire();
        #[cfg(unix)]
        self.owner.receive.close_intake();
    }
}

#[cfg(unix)]
impl HostSettingsTransportOwner {
    pub(super) fn latch_failure(&mut self) {
        self.identity.close();
        self.receive.latch_failure();
    }

    pub(super) async fn drain_connection(
        &mut self,
    ) -> Result<SettingsReceiveDrain, SettingsReceiveError> {
        self.receive.close_and_wait().await
    }

    pub(super) async fn close_and_wait(
        &mut self,
    ) -> Result<SettingsReceiveDrain, SettingsReceiveError> {
        self.identity.close();
        self.receive.close_and_wait().await
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

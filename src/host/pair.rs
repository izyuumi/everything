use super::client::{HostClient, HostClientError};
use crate::{
    crypto::PairingInvitation,
    devices::{ConversationAccess, DeviceAccess, PairingDecision, PendingPairingRequest},
    ipc::{
        OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult, RelayConnectionState,
        RelayStatus,
    },
    protocol::{CommandId, MessageId},
    relay_protocol::RelayOrigin,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use qrcode::{QrCode, render::unicode::Dense1x2};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub const LINK_PREFIX: &str = "https://vorra.yumi.to/pair#";
const LINK_VERSION: u8 = 1;
const MAX_HOST_NAME_BYTES: usize = 64;

#[derive(Debug, Error)]
pub enum PairError {
    #[error(transparent)]
    Host(#[from] HostClientError),
    #[error(
        "this host has no relay; run `vorra relay on --origin wss://relay.example.com` and restart the host"
    )]
    NoRelay,
    #[error("unexpected owner IPC response")]
    Protocol,
    #[error("pairing link encoding failed")]
    Encoding,
    #[error("the host name must be 1–64 bytes without control characters")]
    HostName,
}

pub struct Invitation {
    pub pairing_id: MessageId,
    pub expires_at_ms: u64,
    pub link: Zeroizing<String>,
    pub qr: Zeroizing<String>,
}

pub struct Relay {
    pub origin: RelayOrigin,
    pub connected: bool,
}

pub fn full_access() -> DeviceAccess {
    DeviceAccess {
        conversations: ConversationAccess::All,
        submit_commands: true,
        resolve_permissions: true,
    }
}

pub fn short_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..17).unwrap_or(fingerprint)
}

pub fn host_name() -> String {
    let name = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    let mut cleaned: String = name.chars().filter(|c| !c.is_control()).collect();
    while cleaned.len() > MAX_HOST_NAME_BYTES {
        cleaned.pop();
    }
    if cleaned.trim().is_empty() {
        "Vorra host".to_owned()
    } else {
        cleaned
    }
}

pub async fn relay(
    client: &HostClient,
    cancellation: CancellationToken,
) -> Result<Relay, PairError> {
    let OwnerQueryResult::Transport {
        relay: RelayStatus { origin, connection },
    } = client.query(OwnerQuery::Transport, cancellation).await?
    else {
        return Err(PairError::Protocol);
    };
    Ok(Relay {
        origin: origin.ok_or(PairError::NoRelay)?,
        connected: matches!(connection, RelayConnectionState::Connected),
    })
}

pub async fn begin(
    client: &HostClient,
    origin: &RelayOrigin,
    name: &str,
    lifetime_ms: u64,
    cancellation: CancellationToken,
) -> Result<Invitation, PairError> {
    let OwnerOperationResult::PairingInvitation {
        pairing_id,
        expires_at_ms,
        invitation,
        ..
    } = client
        .operate(
            OwnerOperation::BeginPairing { lifetime_ms },
            CommandId::new(),
            cancellation,
        )
        .await?
    else {
        return Err(PairError::Protocol);
    };
    let parsed: PairingInvitation =
        serde_json::from_slice(invitation.expose()).map_err(|_| PairError::Protocol)?;
    if parsed.pairing_id != pairing_id {
        return Err(PairError::Protocol);
    }
    let link = link(&parsed, origin, name)?;
    let qr = QrCode::new(link.as_bytes()).map_err(|_| PairError::Encoding)?;
    let qr = Zeroizing::new(
        qr.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .build(),
    );
    Ok(Invitation {
        pairing_id,
        expires_at_ms,
        link,
        qr,
    })
}

fn link(
    invitation: &PairingInvitation,
    origin: &RelayOrigin,
    name: &str,
) -> Result<Zeroizing<String>, PairError> {
    let origin = origin.as_str().as_bytes();
    let origin_length = u16::try_from(origin.len()).map_err(|_| PairError::Encoding)?;
    let name_length = u8::try_from(name.len()).map_err(|_| PairError::HostName)?;
    if name.trim().is_empty()
        || name.len() > MAX_HOST_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(PairError::HostName);
    }
    let mut payload = Zeroizing::new(vec![LINK_VERSION]);
    payload.extend_from_slice(&invitation.compact_bytes());
    payload.extend_from_slice(&origin_length.to_be_bytes());
    payload.extend_from_slice(origin);
    payload.push(name_length);
    payload.extend_from_slice(name.as_bytes());
    let mut link = Zeroizing::new(String::from(LINK_PREFIX));
    URL_SAFE_NO_PAD.encode_string(payload.as_slice(), &mut link);
    Ok(link)
}

pub async fn requests(
    client: &HostClient,
    pairing_id: MessageId,
    cancellation: CancellationToken,
) -> Result<Vec<PendingPairingRequest>, PairError> {
    match client
        .query(
            OwnerQuery::PairingRequests {
                pairing_id: Some(pairing_id),
            },
            cancellation,
        )
        .await?
    {
        OwnerQueryResult::PairingRequests { requests } => Ok(requests),
        _ => Err(PairError::Protocol),
    }
}

pub async fn decide(
    client: &HostClient,
    request: &PendingPairingRequest,
    approve: bool,
    cancellation: CancellationToken,
) -> Result<PairingDecision, PairError> {
    match client
        .operate(
            OwnerOperation::DecidePairing {
                pairing_id: request.pairing_id,
                device_id: request.device_id,
                approve,
                access: full_access(),
            },
            CommandId::new(),
            cancellation,
        )
        .await?
    {
        OwnerOperationResult::PairingDecided { decision } => Ok(decision),
        _ => Err(PairError::Protocol),
    }
}

pub async fn cancel(
    client: &HostClient,
    pairing_id: MessageId,
    cancellation: CancellationToken,
) -> Result<(), PairError> {
    client
        .operate(
            OwnerOperation::CancelPairing { pairing_id },
            CommandId::new(),
            cancellation,
        )
        .await?;
    Ok(())
}

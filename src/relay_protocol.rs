use crate::{
    crypto::{EncryptedEnvelope, PairingAcceptance, PairingRequest as SignedPairingRequest},
    protocol::{DeviceId, MessageId},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::{fmt, str::FromStr};
use thiserror::Error;
use url::{Host, Url};

pub const RELAY_SCHEMA_VERSION: u16 = 1;
pub const MAX_RELAY_FRAME_BYTES: usize = 2 * 1_024 * 1_024;
pub const AUTH_CHALLENGE_LIFETIME_MS: u64 = 20_000;
const MAX_ORIGIN_BYTES: usize = 512;
const CHALLENGE_DOMAIN: &[u8] = b"everything.relay.challenge.v1\0";

#[derive(Debug, Error)]
pub enum RelayProtocolError {
    #[error("unsupported relay protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid relay origin; use wss, or ws on a loopback host, without credentials or path")]
    InvalidOrigin,
    #[error("relay challenge origin does not match the configured relay")]
    OriginMismatch,
    #[error("relay challenge device does not match the local identity")]
    DeviceMismatch,
    #[error("relay challenge has expired or has an invalid lifetime")]
    InvalidChallengeTime,
    #[error("operating system randomness is unavailable")]
    RandomnessUnavailable,
    #[error("relay frame exceeds the permitted size")]
    FrameTooLarge,
    #[error("invalid relay JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(transparent)]
pub struct RelayOrigin(String);

impl RelayOrigin {
    pub fn parse(value: &str) -> Result<Self, RelayProtocolError> {
        if value.len() > MAX_ORIGIN_BYTES {
            return Err(RelayProtocolError::InvalidOrigin);
        }
        let parsed = Url::parse(value).map_err(|_| RelayProtocolError::InvalidOrigin)?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path() != "/"
            || parsed.host().is_none()
        {
            return Err(RelayProtocolError::InvalidOrigin);
        }
        let loopback = match parsed.host() {
            Some(Host::Domain(host)) => host == "localhost",
            Some(Host::Ipv4(host)) => host.is_loopback(),
            Some(Host::Ipv6(host)) => host.is_loopback(),
            None => false,
        };
        if parsed.scheme() != "wss" && !(parsed.scheme() == "ws" && loopback) {
            return Err(RelayProtocolError::InvalidOrigin);
        }
        let canonical = parsed.as_str();
        Ok(Self(
            canonical.strip_suffix('/').unwrap_or(canonical).to_owned(),
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn websocket_url(&self) -> String {
        format!("{}/ws", self.0)
    }
}

impl FromStr for RelayOrigin {
    type Err = RelayProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for RelayOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RelayOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayChallenge {
    #[serde(deserialize_with = "deserialize_version")]
    pub schema_version: u16,
    pub session_id: MessageId,
    pub device_id: DeviceId,
    pub origin: RelayOrigin,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(with = "fixed_bytes")]
    pub nonce: [u8; 32],
}

impl RelayChallenge {
    pub fn new(
        origin: RelayOrigin,
        device_id: DeviceId,
        now_ms: u64,
    ) -> Result<Self, RelayProtocolError> {
        let mut nonce = [0; 32];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| RelayProtocolError::RandomnessUnavailable)?;
        Ok(Self {
            schema_version: RELAY_SCHEMA_VERSION,
            session_id: MessageId::new(),
            device_id,
            origin,
            issued_at_ms: now_ms,
            expires_at_ms: now_ms
                .checked_add(AUTH_CHALLENGE_LIFETIME_MS)
                .ok_or(RelayProtocolError::InvalidChallengeTime)?,
            nonce,
        })
    }

    pub fn signing_bytes(
        &self,
        expected_origin: &RelayOrigin,
        expected_device: DeviceId,
        now_ms: u64,
    ) -> Result<Vec<u8>, RelayProtocolError> {
        validate_version(self.schema_version)?;
        if &self.origin != expected_origin {
            return Err(RelayProtocolError::OriginMismatch);
        }
        if self.device_id != expected_device {
            return Err(RelayProtocolError::DeviceMismatch);
        }
        if self.expires_at_ms <= now_ms
            || self.expires_at_ms <= self.issued_at_ms
            || self.expires_at_ms - self.issued_at_ms > AUTH_CHALLENGE_LIFETIME_MS
            || self.issued_at_ms > now_ms.saturating_add(120_000)
        {
            return Err(RelayProtocolError::InvalidChallengeTime);
        }
        let origin_length = u16::try_from(self.origin.as_str().len())
            .map_err(|_| RelayProtocolError::InvalidOrigin)?;
        let mut output = CHALLENGE_DOMAIN.to_vec();
        output.extend_from_slice(&self.schema_version.to_be_bytes());
        output.extend_from_slice(self.session_id.as_uuid().as_bytes());
        output.extend_from_slice(self.device_id.as_uuid().as_bytes());
        output.extend_from_slice(&origin_length.to_be_bytes());
        output.extend_from_slice(self.origin.as_str().as_bytes());
        output.extend_from_slice(&self.issued_at_ms.to_be_bytes());
        output.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        output.extend_from_slice(&self.nonce);
        Ok(output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayClientMessage {
    BeginAuthentication {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        device_id: DeviceId,
    },
    Authenticate {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        session_id: MessageId,
        #[serde(with = "fixed_bytes")]
        signature: [u8; 64],
    },
    Send {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        envelope: EncryptedEnvelope,
    },
    Acknowledge {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        message_id: MessageId,
    },
    Ping {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
    },
    PairingRequest {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        request: SignedPairingRequest,
        device_name: String,
        #[serde(with = "fixed_bytes")]
        name_signature: [u8; 64],
    },
    PairingDecision {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        pairing_id: MessageId,
        responder: DeviceId,
        acceptance: Option<PairingAcceptance>,
        reason: Option<RelayErrorCode>,
    },
}

impl RelayClientMessage {
    pub fn parse(bytes: &[u8]) -> Result<Self, RelayProtocolError> {
        check_frame_size(bytes)?;
        let frame: Self = serde_json::from_slice(bytes)?;
        frame.validate()?;
        Ok(frame)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, RelayProtocolError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        check_frame_size(&bytes)?;
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<(), RelayProtocolError> {
        let version = match self {
            Self::BeginAuthentication { schema_version, .. }
            | Self::Authenticate { schema_version, .. }
            | Self::Send { schema_version, .. }
            | Self::Acknowledge { schema_version, .. }
            | Self::Ping { schema_version }
            | Self::PairingRequest { schema_version, .. }
            | Self::PairingDecision { schema_version, .. } => *schema_version,
        };
        validate_version(version)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayServerMessage {
    Challenge {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        challenge: RelayChallenge,
    },
    Authenticated {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        device_id: DeviceId,
    },
    Accepted {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        message_id: MessageId,
        duplicate: bool,
    },
    Delivery {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        envelope: EncryptedEnvelope,
    },
    Acknowledged {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        message_id: MessageId,
    },
    Pong {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
    },
    Error {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        code: RelayErrorCode,
    },
    PairingPending {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
    },
    PairingRequest {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        request: SignedPairingRequest,
        device_name: String,
        #[serde(with = "fixed_bytes")]
        name_signature: [u8; 64],
    },
    PairingAccepted {
        #[serde(deserialize_with = "deserialize_version")]
        schema_version: u16,
        acceptance: PairingAcceptance,
    },
}

impl RelayServerMessage {
    pub fn parse(bytes: &[u8]) -> Result<Self, RelayProtocolError> {
        check_frame_size(bytes)?;
        let frame: Self = serde_json::from_slice(bytes)?;
        frame.validate()?;
        Ok(frame)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, RelayProtocolError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        check_frame_size(&bytes)?;
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<(), RelayProtocolError> {
        let version = match self {
            Self::Challenge { schema_version, .. }
            | Self::Authenticated { schema_version, .. }
            | Self::Accepted { schema_version, .. }
            | Self::Delivery { schema_version, .. }
            | Self::Acknowledged { schema_version, .. }
            | Self::Pong { schema_version }
            | Self::Error { schema_version, .. }
            | Self::PairingPending { schema_version }
            | Self::PairingRequest { schema_version, .. }
            | Self::PairingAccepted { schema_version, .. } => *schema_version,
        };
        validate_version(version)?;
        if let Self::Challenge { challenge, .. } = self {
            validate_version(challenge.schema_version)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayErrorCode {
    AuthenticationFailed,
    InvalidFrame,
    InvalidEnvelope,
    RecipientUnavailable,
    RateLimited,
    QueueFull,
    MessageConflict,
    SessionAlreadyActive,
    Internal,
    PairingUnavailable,
    PairingInUse,
    PairingRejected,
    PairingExpired,
}

fn deserialize_version<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u16::deserialize(deserializer)?;
    validate_version(version).map_err(D::Error::custom)?;
    Ok(version)
}

fn validate_version(version: u16) -> Result<(), RelayProtocolError> {
    if version != RELAY_SCHEMA_VERSION {
        return Err(RelayProtocolError::UnsupportedVersion(version));
    }
    Ok(())
}

fn check_frame_size(bytes: &[u8]) -> Result<(), RelayProtocolError> {
    if bytes.len() > MAX_RELAY_FRAME_BYTES {
        return Err(RelayProtocolError::FrameTooLarge);
    }
    Ok(())
}

mod fixed_bytes {
    use super::{Engine, STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S, const N: usize>(bytes: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = String::deserialize(deserializer)?;
        if bytes.len() != N.div_ceil(3) * 4 {
            return Err(D::Error::custom("invalid base64 fixed-byte length"));
        }
        STANDARD
            .decode(bytes)
            .map_err(D::Error::custom)?
            .try_into()
            .map_err(|_| D::Error::custom("invalid fixed-byte length"))
    }
}

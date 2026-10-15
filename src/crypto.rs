use crate::protocol::{DeviceId, MessageId};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

pub const ENVELOPE_VERSION: u16 = 1;
pub const MAX_PLAINTEXT_BYTES: usize = 1_048_576;
pub const MAX_CIPHERTEXT_BYTES: usize = MAX_PLAINTEXT_BYTES + 16;
pub const MAX_ENVELOPE_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const MAX_PAIRING_LIFETIME_MS: u64 = 10 * 60 * 1_000;
pub const MAX_AUTH_CHALLENGE_BYTES: usize = 4_096;
const CLOCK_SKEW_MS: u64 = 120_000;
const ENVELOPE_DOMAIN: &[u8] = b"everything.envelope.v1\0";
const KEY_DOMAIN: &[u8] = b"everything.envelope.key.v1\0";
const INVITATION_DOMAIN: &[u8] = b"everything.pairing.invitation.v1\0";
const REQUEST_DOMAIN: &[u8] = b"everything.pairing.request.v1\0";
const ACCEPTANCE_DOMAIN: &[u8] = b"everything.pairing.acceptance.v1\0";
const CHALLENGE_DOMAIN: &[u8] = b"everything.authentication.v1\0";

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("operating system randomness is unavailable")]
    RandomnessUnavailable,
    #[error("unsupported encrypted protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid device public key")]
    InvalidPublicKey,
    #[error("message sender or recipient does not match the trusted devices")]
    DeviceMismatch,
    #[error("signature authentication failed")]
    InvalidSignature,
    #[error("message or pairing invitation has expired")]
    Expired,
    #[error("invalid creation time or lifetime")]
    InvalidLifetime,
    #[error("payload exceeds the permitted size")]
    PayloadTooLarge,
    #[error("payload must not be empty")]
    EmptyPayload,
    #[error("key agreement failed")]
    KeyAgreementFailed,
    #[error("encryption failed")]
    EncryptionFailed,
    #[error("ciphertext authentication failed")]
    DecryptionFailed,
    #[error("message has already been received")]
    Replay,
    #[error("durable replay recording failed: {0}")]
    ReplayStorage(String),
    #[error("pairing request does not match the invitation")]
    PairingMismatch,
    #[error("pairing secret proof is invalid")]
    InvalidPairingProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePublicKeys {
    pub device_id: DeviceId,
    #[serde(with = "fixed_bytes")]
    pub signing_key: [u8; 32],
    #[serde(with = "fixed_bytes")]
    pub encryption_key: [u8; 32],
}

impl DevicePublicKeys {
    pub fn validate(&self) -> Result<(), CryptoError> {
        let key = self.verifying_key()?;
        if key.is_weak() || self.encryption_key.iter().all(|byte| *byte == 0) {
            return Err(CryptoError::InvalidPublicKey);
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.canonical_bytes());
        digest
            .chunks(4)
            .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
            .collect::<Vec<String>>()
            .join("-")
    }

    pub fn verify_challenge(
        &self,
        challenge: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), CryptoError> {
        self.validate()?;
        let bytes = challenge_bytes(challenge)?;
        self.verify(&bytes, signature)
    }

    fn verifying_key(&self) -> Result<VerifyingKey, CryptoError> {
        VerifyingKey::from_bytes(&self.signing_key).map_err(|_| CryptoError::InvalidPublicKey)
    }

    fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), CryptoError> {
        self.verifying_key()?
            .verify_strict(message, &Signature::from_bytes(signature))
            .map_err(|_| CryptoError::InvalidSignature)
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(80);
        output.extend_from_slice(self.device_id.as_uuid().as_bytes());
        output.extend_from_slice(&self.signing_key);
        output.extend_from_slice(&self.encryption_key);
        output
    }
}

pub struct DeviceIdentitySecrets {
    pub device_id: DeviceId,
    pub signing_seed: Zeroizing<[u8; 32]>,
    pub encryption_secret: Zeroizing<[u8; 32]>,
}

pub struct DeviceIdentity {
    device_id: DeviceId,
    signing_key: SigningKey,
    encryption_key: StaticSecret,
}

impl DeviceIdentity {
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self::from_secrets(DeviceIdentitySecrets {
            device_id: DeviceId::new(),
            signing_seed: Zeroizing::new(random_array()?),
            encryption_secret: Zeroizing::new(random_array()?),
        }))
    }

    pub fn from_secrets(secrets: DeviceIdentitySecrets) -> Self {
        Self {
            device_id: secrets.device_id,
            signing_key: SigningKey::from_bytes(&secrets.signing_seed),
            encryption_key: StaticSecret::from(*secrets.encryption_secret),
        }
    }

    pub fn export_secrets(&self) -> DeviceIdentitySecrets {
        DeviceIdentitySecrets {
            device_id: self.device_id,
            signing_seed: Zeroizing::new(self.signing_key.to_bytes()),
            encryption_secret: Zeroizing::new(self.encryption_key.to_bytes()),
        }
    }

    pub fn public_keys(&self) -> DevicePublicKeys {
        DevicePublicKeys {
            device_id: self.device_id,
            signing_key: self.signing_key.verifying_key().to_bytes(),
            encryption_key: PublicKey::from(&self.encryption_key).to_bytes(),
        }
    }

    pub fn sign_challenge(&self, challenge: &[u8]) -> Result<[u8; 64], CryptoError> {
        Ok(self.sign(&challenge_bytes(challenge)?))
    }

    pub fn seal(
        &self,
        recipient: &DevicePublicKeys,
        payload: &[u8],
        created_at_ms: u64,
        lifetime_ms: u64,
    ) -> Result<EncryptedEnvelope, CryptoError> {
        recipient.validate()?;
        validate_payload_size(payload)?;
        if recipient.device_id == self.device_id {
            return Err(CryptoError::DeviceMismatch);
        }
        let expires_at_ms = expiration(created_at_ms, lifetime_ms, MAX_ENVELOPE_LIFETIME_MS)?;
        let header = EnvelopeHeader {
            version: ENVELOPE_VERSION,
            message_id: MessageId::new(),
            sender: self.device_id,
            recipient: recipient.device_id,
            created_at_ms,
            expires_at_ms,
            nonce: random_array()?,
            key_salt: random_array()?,
        };
        let key = self.envelope_key(recipient, &header)?;
        let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| CryptoError::EncryptionFailed)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&header.nonce),
                Payload {
                    msg: payload,
                    aad: &header.canonical_bytes(),
                },
            )
            .map_err(|_| CryptoError::EncryptionFailed)?;
        let mut envelope = EncryptedEnvelope {
            header,
            ciphertext,
            signature: [0; 64],
        };
        envelope.signature = self.sign(&envelope.signed_bytes()?);
        Ok(envelope)
    }

    pub fn open(
        &self,
        sender: &DevicePublicKeys,
        envelope: &EncryptedEnvelope,
        now_ms: u64,
        replay_guard: &mut dyn ReplayGuard,
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        if envelope.header.recipient != self.device_id || sender.device_id == self.device_id {
            return Err(CryptoError::DeviceMismatch);
        }
        envelope.verify_authenticated_sender(sender, now_ms)?;
        let key = self.envelope_key(sender, &envelope.header)?;
        let cipher = ChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| CryptoError::DecryptionFailed)?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&envelope.header.nonce),
                    Payload {
                        msg: &envelope.ciphertext,
                        aad: &envelope.header.canonical_bytes(),
                    },
                )
                .map_err(|_| CryptoError::DecryptionFailed)?,
        );
        validate_payload_size(&plaintext)?;
        let accepted = replay_guard.record_if_new(
            ReplayRecord {
                message_id: envelope.header.message_id,
                sender: envelope.header.sender,
                recipient: envelope.header.recipient,
                expires_at_ms: envelope.header.expires_at_ms,
            },
            &plaintext,
            now_ms,
        )?;
        if !accepted {
            return Err(CryptoError::Replay);
        }
        Ok(plaintext)
    }

    pub fn begin_pairing(
        &self,
        created_at_ms: u64,
        lifetime_ms: u64,
    ) -> Result<PendingPairing, CryptoError> {
        let mut invitation = PairingInvitation {
            version: ENVELOPE_VERSION,
            pairing_id: MessageId::new(),
            inviter: self.public_keys(),
            created_at_ms,
            expires_at_ms: expiration(created_at_ms, lifetime_ms, MAX_PAIRING_LIFETIME_MS)?,
            secret: Zeroizing::new(random_array()?),
            signature: [0; 64],
        };
        invitation.signature = self.sign(&invitation.signed_bytes());
        Ok(PendingPairing { invitation })
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing_key.sign(message).to_bytes()
    }

    fn envelope_key(
        &self,
        peer: &DevicePublicKeys,
        header: &EnvelopeHeader,
    ) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
        let shared = self
            .encryption_key
            .diffie_hellman(&PublicKey::from(peer.encryption_key));
        if !shared.was_contributory() {
            return Err(CryptoError::KeyAgreementFailed);
        }
        let own = self.public_keys();
        let (sender, recipient) = if header.sender == self.device_id {
            (&own, peer)
        } else {
            (peer, &own)
        };
        let mut info = KEY_DOMAIN.to_vec();
        info.extend_from_slice(&sender.canonical_bytes());
        info.extend_from_slice(&recipient.canonical_bytes());
        let mut key = Zeroizing::new([0; 32]);
        Hkdf::<Sha256>::new(Some(&header.key_salt), shared.as_bytes())
            .expand(&info, key.as_mut())
            .map_err(|_| CryptoError::KeyAgreementFailed)?;
        Ok(key)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopeHeader {
    pub version: u16,
    pub message_id: MessageId,
    pub sender: DeviceId,
    pub recipient: DeviceId,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(with = "fixed_bytes")]
    pub nonce: [u8; 12],
    #[serde(with = "fixed_bytes")]
    pub key_salt: [u8; 32],
}

impl EnvelopeHeader {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut output = ENVELOPE_DOMAIN.to_vec();
        output.extend_from_slice(&self.version.to_be_bytes());
        output.extend_from_slice(self.message_id.as_uuid().as_bytes());
        output.extend_from_slice(self.sender.as_uuid().as_bytes());
        output.extend_from_slice(self.recipient.as_uuid().as_bytes());
        output.extend_from_slice(&self.created_at_ms.to_be_bytes());
        output.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        output.extend_from_slice(&self.nonce);
        output.extend_from_slice(&self.key_salt);
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedEnvelope {
    pub header: EnvelopeHeader,
    #[serde(with = "ciphertext_bytes")]
    pub ciphertext: Vec<u8>,
    #[serde(with = "fixed_bytes")]
    pub signature: [u8; 64],
}

impl EncryptedEnvelope {
    pub fn verify_authenticated_sender(
        &self,
        trusted_sender: &DevicePublicKeys,
        now_ms: u64,
    ) -> Result<(), CryptoError> {
        validate_version(self.header.version)?;
        if trusted_sender.device_id != self.header.sender
            || self.header.sender == self.header.recipient
        {
            return Err(CryptoError::DeviceMismatch);
        }
        validate_times(
            self.header.created_at_ms,
            self.header.expires_at_ms,
            now_ms,
            MAX_ENVELOPE_LIFETIME_MS,
        )?;
        trusted_sender.validate()?;
        trusted_sender.verify(&self.signed_bytes()?, &self.signature)
    }

    fn signed_bytes(&self) -> Result<Vec<u8>, CryptoError> {
        if self.ciphertext.len() <= 16 || self.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            return Err(CryptoError::PayloadTooLarge);
        }
        let mut output = self.header.canonical_bytes();
        let length =
            u32::try_from(self.ciphertext.len()).map_err(|_| CryptoError::PayloadTooLarge)?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(&self.ciphertext);
        Ok(output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayRecord {
    pub message_id: MessageId,
    pub sender: DeviceId,
    pub recipient: DeviceId,
    pub expires_at_ms: u64,
}

pub trait ReplayGuard {
    fn record_if_new(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingInvitation {
    pub version: u16,
    pub pairing_id: MessageId,
    pub inviter: DevicePublicKeys,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    #[serde(with = "secret_bytes")]
    secret: Zeroizing<[u8; 32]>,
    #[serde(with = "fixed_bytes")]
    pub signature: [u8; 64],
}

impl PairingInvitation {
    pub fn verify(&self, now_ms: u64) -> Result<(), CryptoError> {
        validate_version(self.version)?;
        validate_times(
            self.created_at_ms,
            self.expires_at_ms,
            now_ms,
            MAX_PAIRING_LIFETIME_MS,
        )?;
        self.inviter.validate()?;
        self.inviter.verify(&self.signed_bytes(), &self.signature)
    }

    pub fn create_request(
        self,
        responder: &DeviceIdentity,
        now_ms: u64,
    ) -> Result<(PairingRequest, PendingPairingCompletion), CryptoError> {
        self.verify(now_ms)?;
        if responder.device_id == self.inviter.device_id {
            return Err(CryptoError::DeviceMismatch);
        }
        let mut request = PairingRequest {
            version: ENVELOPE_VERSION,
            pairing_id: self.pairing_id,
            inviter: self.inviter.device_id,
            responder: responder.public_keys(),
            expires_at_ms: self.expires_at_ms,
            proof: [0; 32],
            signature: [0; 64],
        };
        let mut mac = pairing_mac(&self.secret)?;
        mac.update(&request.proof_bytes());
        request.proof.copy_from_slice(&mac.finalize().into_bytes());
        request.signature = responder.sign(&request.signed_bytes());
        let completion = PendingPairingCompletion {
            invitation: self,
            responder: responder.public_keys(),
        };
        Ok((request, completion))
    }

    fn signed_bytes(&self) -> Zeroizing<Vec<u8>> {
        let mut output = Zeroizing::new(INVITATION_DOMAIN.to_vec());
        output.extend_from_slice(&self.version.to_be_bytes());
        output.extend_from_slice(self.pairing_id.as_uuid().as_bytes());
        output.extend_from_slice(&self.inviter.canonical_bytes());
        output.extend_from_slice(&self.created_at_ms.to_be_bytes());
        output.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        output.extend_from_slice(self.secret.as_ref());
        output
    }
}

pub struct PendingPairing {
    invitation: PairingInvitation,
}

impl PendingPairing {
    pub fn invitation(&self) -> &PairingInvitation {
        &self.invitation
    }

    pub fn accept(
        self,
        inviter: &DeviceIdentity,
        request: &PairingRequest,
        now_ms: u64,
    ) -> Result<(DevicePublicKeys, PairingAcceptance), CryptoError> {
        self.invitation.verify(now_ms)?;
        if inviter.public_keys() != self.invitation.inviter
            || request.pairing_id != self.invitation.pairing_id
            || request.inviter != inviter.device_id
            || request.expires_at_ms != self.invitation.expires_at_ms
            || request.responder.device_id == inviter.device_id
        {
            return Err(CryptoError::PairingMismatch);
        }
        validate_version(request.version)?;
        request.responder.validate()?;
        request
            .responder
            .verify(&request.signed_bytes(), &request.signature)?;
        let mut mac = pairing_mac(&self.invitation.secret)?;
        mac.update(&request.proof_bytes());
        mac.verify_slice(&request.proof)
            .map_err(|_| CryptoError::InvalidPairingProof)?;
        let mut acceptance = PairingAcceptance {
            version: ENVELOPE_VERSION,
            pairing_id: request.pairing_id,
            inviter: self.invitation.inviter,
            responder: request.responder.clone(),
            expires_at_ms: request.expires_at_ms,
            signature: [0; 64],
        };
        acceptance.signature = inviter.sign(&acceptance.signed_bytes());
        Ok((request.responder.clone(), acceptance))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingRequest {
    pub version: u16,
    pub pairing_id: MessageId,
    pub inviter: DeviceId,
    pub responder: DevicePublicKeys,
    pub expires_at_ms: u64,
    #[serde(with = "fixed_bytes")]
    pub proof: [u8; 32],
    #[serde(with = "fixed_bytes")]
    pub signature: [u8; 64],
}

impl PairingRequest {
    fn proof_bytes(&self) -> Vec<u8> {
        let mut output = REQUEST_DOMAIN.to_vec();
        output.extend_from_slice(&self.version.to_be_bytes());
        output.extend_from_slice(self.pairing_id.as_uuid().as_bytes());
        output.extend_from_slice(self.inviter.as_uuid().as_bytes());
        output.extend_from_slice(&self.responder.canonical_bytes());
        output.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        output
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut output = self.proof_bytes();
        output.extend_from_slice(&self.proof);
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingAcceptance {
    pub version: u16,
    pub pairing_id: MessageId,
    pub inviter: DevicePublicKeys,
    pub responder: DevicePublicKeys,
    pub expires_at_ms: u64,
    #[serde(with = "fixed_bytes")]
    pub signature: [u8; 64],
}

impl PairingAcceptance {
    fn signed_bytes(&self) -> Vec<u8> {
        let mut output = ACCEPTANCE_DOMAIN.to_vec();
        output.extend_from_slice(&self.version.to_be_bytes());
        output.extend_from_slice(self.pairing_id.as_uuid().as_bytes());
        output.extend_from_slice(&self.inviter.canonical_bytes());
        output.extend_from_slice(&self.responder.canonical_bytes());
        output.extend_from_slice(&self.expires_at_ms.to_be_bytes());
        output
    }
}

pub struct PendingPairingCompletion {
    invitation: PairingInvitation,
    responder: DevicePublicKeys,
}

impl PendingPairingCompletion {
    pub fn finish(
        self,
        acceptance: &PairingAcceptance,
        now_ms: u64,
    ) -> Result<DevicePublicKeys, CryptoError> {
        self.invitation.verify(now_ms)?;
        validate_version(acceptance.version)?;
        if acceptance.pairing_id != self.invitation.pairing_id
            || acceptance.inviter != self.invitation.inviter
            || acceptance.responder != self.responder
            || acceptance.expires_at_ms != self.invitation.expires_at_ms
        {
            return Err(CryptoError::PairingMismatch);
        }
        self.invitation
            .inviter
            .verify(&acceptance.signed_bytes(), &acceptance.signature)?;
        Ok(self.invitation.inviter)
    }
}

fn random_array<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut bytes = [0; N];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| CryptoError::RandomnessUnavailable)?;
    Ok(bytes)
}

fn validate_payload_size(payload: &[u8]) -> Result<(), CryptoError> {
    if payload.is_empty() {
        return Err(CryptoError::EmptyPayload);
    }
    if payload.len() > MAX_PLAINTEXT_BYTES {
        return Err(CryptoError::PayloadTooLarge);
    }
    Ok(())
}

fn validate_version(version: u16) -> Result<(), CryptoError> {
    if version != ENVELOPE_VERSION {
        return Err(CryptoError::UnsupportedVersion(version));
    }
    Ok(())
}

fn expiration(created: u64, lifetime: u64, maximum: u64) -> Result<u64, CryptoError> {
    if lifetime == 0 || lifetime > maximum {
        return Err(CryptoError::InvalidLifetime);
    }
    created
        .checked_add(lifetime)
        .ok_or(CryptoError::InvalidLifetime)
}

fn validate_times(created: u64, expires: u64, now: u64, maximum: u64) -> Result<(), CryptoError> {
    if expires <= created
        || expires - created > maximum
        || created > now.saturating_add(CLOCK_SKEW_MS)
    {
        return Err(CryptoError::InvalidLifetime);
    }
    if expires <= now {
        return Err(CryptoError::Expired);
    }
    Ok(())
}

fn challenge_bytes(challenge: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if challenge.is_empty() {
        return Err(CryptoError::EmptyPayload);
    }
    if challenge.len() > MAX_AUTH_CHALLENGE_BYTES {
        return Err(CryptoError::PayloadTooLarge);
    }
    let mut output = CHALLENGE_DOMAIN.to_vec();
    output.extend_from_slice(challenge);
    Ok(output)
}

fn pairing_mac(secret: &[u8; 32]) -> Result<Hmac<Sha256>, CryptoError> {
    <Hmac<Sha256> as Mac>::new_from_slice(secret).map_err(|_| CryptoError::InvalidPairingProof)
}

mod fixed_bytes {
    use super::{Engine, STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S, const N: usize>(value: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() != N.div_ceil(3) * 4 {
            return Err(D::Error::custom("invalid base64 fixed-byte length"));
        }
        STANDARD
            .decode(value)
            .map_err(D::Error::custom)?
            .try_into()
            .map_err(|_| D::Error::custom("invalid decoded fixed-byte length"))
    }
}

mod secret_bytes {
    use super::{Engine, STANDARD, Zeroizing};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(value: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = Zeroizing::new(STANDARD.encode(value));
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Zeroizing<[u8; 32]>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = Zeroizing::new(String::deserialize(deserializer)?);
        if encoded.len() != 44 {
            return Err(D::Error::custom("invalid base64 secret length"));
        }
        let mut decoded = Zeroizing::new([0; 33]);
        let length = STANDARD
            .decode_slice(encoded.as_bytes(), decoded.as_mut())
            .map_err(D::Error::custom)?;
        if length != 32 {
            return Err(D::Error::custom("invalid decoded secret length"));
        }
        let mut secret = Zeroizing::new([0; 32]);
        secret.copy_from_slice(&decoded[..32]);
        Ok(secret)
    }
}

mod ciphertext_bytes {
    use super::{Engine, MAX_CIPHERTEXT_BYTES, STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() > MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4 {
            return Err(D::Error::custom("ciphertext exceeds the permitted size"));
        }
        let bytes = STANDARD.decode(value).map_err(D::Error::custom)?;
        if bytes.len() <= 16 || bytes.len() > MAX_CIPHERTEXT_BYTES {
            return Err(D::Error::custom("invalid ciphertext length"));
        }
        Ok(bytes)
    }
}

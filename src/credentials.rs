use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, Zeroizing};

const MAX_SECRET_BYTES: usize = 8_192;
const READ_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    #[error("invalid credential identifier")]
    InvalidKey,
    #[error("invalid credential value")]
    InvalidSecret,
    #[error("native credential store is unsupported on this platform")]
    Unsupported,
    #[error("native credential store is unavailable")]
    Unavailable,
    #[error("native credential store is busy")]
    Busy,
    #[error("native credential lookup timed out")]
    Timeout,
    #[error("native credential lookup was cancelled")]
    Cancelled,
    #[error("native credential worker failed")]
    Worker,
}

#[derive(Clone)]
pub struct CredentialKey {
    service: String,
    account: String,
}

impl CredentialKey {
    pub fn new(service: &str, account: &str) -> Result<Self, CredentialError> {
        for (value, limit) in [(service, 128), (account, 256)] {
            if value.is_empty()
                || value.len() > limit
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
            {
                return Err(CredentialError::InvalidKey);
            }
        }
        Ok(Self {
            service: service.into(),
            account: account.into(),
        })
    }
}

pub struct CredentialSecret(Zeroizing<Vec<u8>>);

impl CredentialSecret {
    pub fn new(bytes: Vec<u8>) -> Result<Self, CredentialError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.is_empty() || bytes.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::InvalidSecret);
        }
        Ok(Self(bytes))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone)]
pub struct NativeCredentialStore {
    admission: Arc<Semaphore>,
}

impl Default for NativeCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeCredentialStore {
    pub fn new() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn read(
        &self,
        key: CredentialKey,
        cancellation: CancellationToken,
    ) -> Result<Option<CredentialSecret>, CredentialError> {
        if cancellation.is_cancelled() {
            return Err(CredentialError::Cancelled);
        }
        let permit = self.permit()?;
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_native(&key)
        });
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(CredentialError::Cancelled),
            result = tokio::time::timeout(READ_TIMEOUT, worker) => {
                result.map_err(|_| CredentialError::Timeout)?
                    .map_err(|_| CredentialError::Worker)?
            }
        }
    }

    pub async fn store(
        &self,
        key: CredentialKey,
        secret: CredentialSecret,
    ) -> Result<(), CredentialError> {
        let permit = self.permit()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            write_native(&key, secret)
        })
        .await
        .map_err(|_| CredentialError::Worker)?
    }

    pub async fn delete(&self, key: CredentialKey) -> Result<(), CredentialError> {
        let permit = self.permit()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            delete_native(&key)
        })
        .await
        .map_err(|_| CredentialError::Worker)?
    }

    fn permit(&self) -> Result<OwnedSemaphorePermit, CredentialError> {
        Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| CredentialError::Busy)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_entry(key: &CredentialKey) -> Result<keyring::Entry, CredentialError> {
    #[cfg(target_os = "linux")]
    let credential =
        keyring::secret_service::SsCredential::new_with_target(None, &key.service, &key.account);
    #[cfg(target_os = "macos")]
    let credential =
        keyring::macos::MacCredential::new_with_target(None, &key.service, &key.account);
    credential
        .map(|credential| keyring::Entry::new_with_credential(Box::new(credential)))
        .map_err(native_error)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_error(error: keyring::Error) -> CredentialError {
    if let keyring::Error::BadEncoding(mut bytes) = error {
        bytes.zeroize();
    }
    CredentialError::Unavailable
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_native(key: &CredentialKey) -> Result<Option<CredentialSecret>, CredentialError> {
    match native_entry(key)?.get_secret() {
        Ok(bytes) => CredentialSecret::new(bytes).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(native_error(error)),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_native(key: &CredentialKey, secret: CredentialSecret) -> Result<(), CredentialError> {
    native_entry(key)?
        .set_secret(secret.expose())
        .map_err(native_error)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn delete_native(key: &CredentialKey) -> Result<(), CredentialError> {
    match native_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(native_error(error)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_native(_key: &CredentialKey) -> Result<Option<CredentialSecret>, CredentialError> {
    Err(CredentialError::Unsupported)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn write_native(_key: &CredentialKey, _secret: CredentialSecret) -> Result<(), CredentialError> {
    Err(CredentialError::Unsupported)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn delete_native(_key: &CredentialKey) -> Result<(), CredentialError> {
    Err(CredentialError::Unsupported)
}

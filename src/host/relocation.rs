use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::protocol::{ProtocolError, UnixMillis};
use crate::scheduling::ScheduleError;
use crate::scheduling::store::ScheduleRepository;
use crate::service::{ServiceError, StateLease};
use crate::storage::{StorageError, Store};

use super::settings::SettingsError;

#[derive(Debug, Error)]
pub enum RelocationError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("legacy state path relocation failed: {0}")]
    Io(#[from] io::Error),
    #[error("legacy state path relocation worker failed")]
    Worker,
}

pub async fn relocate_legacy_paths(home: &Path) -> Result<(), RelocationError> {
    let current = home.join(".vorra");
    let legacy = home.join(".everything");
    if !home.is_absolute() || !present(&current)? || present(&legacy)? {
        return Ok(());
    }
    let lease = match StateLease::acquire(&current).await {
        Ok(lease) => lease,
        Err(ServiceError::AlreadyRunning) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut sources = vec![legacy];
    if let Ok(physical) = home.canonicalize() {
        let physical = physical.join(".everything");
        if !sources.contains(&physical) {
            sources.push(physical);
        }
    }
    tokio::task::spawn_blocking(move || {
        super::settings::relocate_paths(&lease, &sources)?;
        if let Some(database) = lease.existing_database()? {
            let store = Store::open_validated(lease.path().join("vorra.sqlite3"), database)?;
            ScheduleRepository::new(store).relocate_paths(
                &sources,
                lease.path(),
                UnixMillis::now()?,
            )?;
        }
        Ok(())
    })
    .await
    .map_err(|_| RelocationError::Worker)?
}

pub(crate) fn relocate_value(value: &mut Value, sources: &[PathBuf], target: &Path) -> bool {
    match value {
        Value::String(text) => {
            let path = Path::new(text.as_str());
            let Some(relocated) = sources.iter().find_map(|source| {
                let rest = path.strip_prefix(source).ok()?;
                let relocated = if rest.as_os_str().is_empty() {
                    target.to_path_buf()
                } else {
                    target.join(rest)
                };
                relocated.to_str().map(str::to_owned)
            }) else {
                return false;
            };
            *text = relocated;
            true
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|item| relocate_value(item, sources, target))
            .fold(false, |changed, item| changed | item),
        Value::Object(fields) => fields
            .values_mut()
            .map(|item| relocate_value(item, sources, target))
            .fold(false, |changed, item| changed | item),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn present(path: &Path) -> Result<bool, io::Error> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

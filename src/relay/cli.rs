use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::{Args, Subcommand};
use serde::Serialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{RelayError, database::Database, server};
use crate::{
    crypto::DevicePublicKeys,
    host::{client::HostClient, settings::SettingsRepository},
    ipc::{
        OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult, RelayConnectionState,
        RelayStatus,
    },
    protocol::{CommandId, DeviceId},
    relay_protocol::RelayOrigin,
    service::{ServiceError, StateLease},
};

#[derive(Args)]
pub struct RelayArguments {
    #[command(subcommand)]
    pub command: RelayCommand,
}

#[derive(Args)]
pub struct DatabaseArgument {
    #[arg(
        long,
        value_name = "PATH",
        help = "Relay server SQLite database [default: <state-dir>/relay.sqlite3]"
    )]
    database: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum RelayCommand {
    #[command(about = "Run the blind relay server")]
    Serve {
        #[command(flatten)]
        database: DatabaseArgument,
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        #[arg(
            long,
            help = "Expected public WebSocket origin, wss://host or loopback ws://host"
        )]
        origin: RelayOrigin,
        #[arg(long, default_value_t = 64)]
        max_connections: usize,
    },
    #[command(about = "Enroll a device's public keys on the relay server")]
    Enroll {
        #[command(flatten)]
        database: DatabaseArgument,
        #[arg(long, help = "Local JSON file containing public device keys only")]
        keys: PathBuf,
    },
    #[command(about = "Revoke a device on the relay server and drop its queued traffic")]
    Revoke {
        #[command(flatten)]
        database: DatabaseArgument,
        device_id: DeviceId,
    },
    #[command(about = "List devices enrolled on the relay server")]
    Devices {
        #[command(flatten)]
        database: DatabaseArgument,
    },
    #[command(about = "Show this host's relay connection and the relay server database")]
    Status {
        #[command(flatten)]
        database: DatabaseArgument,
    },
    #[command(about = "Connect this host to a relay")]
    On {
        #[arg(
            long,
            help = "Relay WebSocket origin, wss://host or loopback ws://host"
        )]
        origin: Option<RelayOrigin>,
    },
    #[command(about = "Stop connecting this host to a relay")]
    Off,
}

#[derive(Debug, Error)]
pub enum RelayCommandError {
    #[error(transparent)]
    Relay(#[from] RelayError),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Settings(#[from] crate::host::settings::SettingsError),
    #[error(transparent)]
    HostClient(#[from] crate::host::client::HostClientError),
    #[error("no relay origin is configured; pass --origin wss://host")]
    OriginRequired,
    #[error("unexpected owner IPC response")]
    Protocol,
}

#[derive(Serialize)]
struct HostRelay {
    host_running: bool,
    origin: Option<RelayOrigin>,
    connection: RelayConnectionState,
}

#[derive(Serialize)]
struct RelayToggle {
    host_running: bool,
    origin: Option<RelayOrigin>,
    restart_required: bool,
}

#[derive(Serialize)]
struct ServerStatus {
    database: PathBuf,
    status: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct Status {
    host: HostRelay,
    server: ServerStatus,
}

pub async fn run(
    arguments: RelayArguments,
    state_directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<(), RelayCommandError> {
    let database_path = |argument: DatabaseArgument| {
        argument
            .database
            .unwrap_or_else(|| state_directory.join("relay.sqlite3"))
    };
    match arguments.command {
        RelayCommand::Serve {
            database,
            bind,
            origin,
            max_connections,
        } => {
            with_database(database_path(database), |database| async move {
                server::run(database, bind, origin, max_connections, cancellation).await
            })
            .await?;
        }
        RelayCommand::Enroll { database, keys } => {
            let keys = tokio::task::spawn_blocking(move || read_public_keys(keys))
                .await
                .map_err(|error| RelayError::Configuration(error.to_string()))??;
            with_database(database_path(database), |database| async move {
                let device_id = keys.device_id;
                let created = database.enroll(keys, server::now_ms()?).await?;
                println!(
                    "{device_id}: {}",
                    if created {
                        "enrolled"
                    } else {
                        "already enrolled with the same keys"
                    }
                );
                Ok(())
            })
            .await?;
        }
        RelayCommand::Revoke {
            database,
            device_id,
        } => {
            with_database(database_path(database), |database| async move {
                let changed = database.revoke(device_id, server::now_ms()?).await?;
                println!(
                    "{device_id}: {}",
                    if changed {
                        "revoked; queued traffic removed"
                    } else {
                        "absent or already revoked"
                    }
                );
                Ok(())
            })
            .await?;
        }
        RelayCommand::Devices { database } => {
            with_database(database_path(database), |database| async move {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&database.devices().await?)?
                );
                Ok(())
            })
            .await?;
        }
        RelayCommand::Status { database } => {
            let database = database_path(database);
            let status = if tokio::fs::try_exists(&database)
                .await
                .map_err(RelayError::from)?
            {
                Some(
                    with_database(database.clone(), |database| async move {
                        database.prune(server::now_ms()?).await?;
                        Ok(serde_json::to_value(database.status().await?)?)
                    })
                    .await?,
                )
            } else {
                None
            };
            let host = host_status(&state_directory, cancellation).await?;
            print_json(&Status {
                host,
                server: ServerStatus { database, status },
            })?;
        }
        RelayCommand::On { origin } => {
            print_json(&toggle(&state_directory, RelayTarget::On(origin), cancellation).await?)?;
        }
        RelayCommand::Off => {
            print_json(&toggle(&state_directory, RelayTarget::Off, cancellation).await?)?;
        }
    }
    Ok(())
}

async fn with_database<T, F, Fut>(path: PathBuf, operation: F) -> Result<T, RelayError>
where
    F: FnOnce(Database) -> Fut,
    Fut: Future<Output = Result<T, RelayError>>,
{
    let database = Database::open(path).await?;
    let result = operation(database.clone()).await;
    if let Err(error) = database.close().await {
        eprintln!("vorra: relay database cleanup failed: {error}");
        if result.is_ok() {
            return Err(error);
        }
    }
    result
}

async fn host_status(
    state_directory: &Path,
    cancellation: CancellationToken,
) -> Result<HostRelay, RelayCommandError> {
    match StateLease::acquire(state_directory).await {
        Ok(lease) => {
            let origin = offline_settings(lease).await?.snapshot()?.relay_origin;
            Ok(HostRelay {
                host_running: false,
                connection: if origin.is_some() {
                    RelayConnectionState::Stopped
                } else {
                    RelayConnectionState::Disabled
                },
                origin,
            })
        }
        Err(ServiceError::AlreadyRunning) => {
            match HostClient::new(state_directory)?
                .query(OwnerQuery::Transport, cancellation)
                .await?
            {
                OwnerQueryResult::Transport {
                    relay: RelayStatus { origin, connection },
                } => Ok(HostRelay {
                    host_running: true,
                    origin,
                    connection,
                }),
                _ => Err(RelayCommandError::Protocol),
            }
        }
        Err(error) => Err(error.into()),
    }
}

enum RelayTarget {
    Off,
    On(Option<RelayOrigin>),
}

async fn toggle(
    state_directory: &Path,
    relay: RelayTarget,
    cancellation: CancellationToken,
) -> Result<RelayToggle, RelayCommandError> {
    let target = |current: Option<RelayOrigin>| match relay {
        RelayTarget::Off => Ok(None),
        RelayTarget::On(Some(origin)) => Ok(Some(origin)),
        RelayTarget::On(None) => current.ok_or(RelayCommandError::OriginRequired).map(Some),
    };
    match StateLease::acquire(state_directory).await {
        Ok(lease) => {
            let repository = offline_settings(lease).await?;
            let mut settings = repository.snapshot()?;
            let origin = target(settings.relay_origin.clone())?;
            if settings.relay_origin != origin {
                let revision = settings.revision;
                settings.relay_origin = origin.clone();
                tokio::task::spawn_blocking(move || repository.replace(settings, revision))
                    .await
                    .map_err(|_| ServiceError::Worker)??;
            }
            Ok(RelayToggle {
                host_running: false,
                origin,
                restart_required: false,
            })
        }
        Err(ServiceError::AlreadyRunning) => {
            let client = HostClient::new(state_directory)?;
            let OwnerQueryResult::Settings { settings } = client
                .query(OwnerQuery::Settings, cancellation.clone())
                .await?
            else {
                return Err(RelayCommandError::Protocol);
            };
            let origin = target(settings.relay_origin.clone())?;
            if settings.relay_origin == origin {
                return Ok(RelayToggle {
                    host_running: true,
                    origin,
                    restart_required: false,
                });
            }
            let mut updated = *settings;
            let expected_revision = updated.revision;
            updated.relay_origin = origin.clone();
            match client
                .operate(
                    OwnerOperation::ReplaceSettings {
                        expected_revision,
                        settings: Box::new(updated),
                    },
                    CommandId::new(),
                    cancellation,
                )
                .await?
            {
                OwnerOperationResult::SettingsReceipt {
                    restart_required, ..
                } => Ok(RelayToggle {
                    host_running: true,
                    origin,
                    restart_required,
                }),
                _ => Err(RelayCommandError::Protocol),
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn offline_settings(lease: Arc<StateLease>) -> Result<SettingsRepository, RelayCommandError> {
    Ok(
        tokio::task::spawn_blocking(move || SettingsRepository::open(lease))
            .await
            .map_err(|_| ServiceError::Worker)??,
    )
}

fn print_json(value: &impl Serialize) -> Result<(), RelayError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn read_public_keys(path: PathBuf) -> Result<DevicePublicKeys, RelayError> {
    let file = std::fs::File::open(path)?;
    let mut encoded = Vec::new();
    file.take(4_097).read_to_end(&mut encoded)?;
    if encoded.len() > 4_096 {
        return Err(RelayError::Configuration(
            "public identity file exceeds 4096 bytes".to_owned(),
        ));
    }
    let keys: DevicePublicKeys = serde_json::from_slice(&encoded)?;
    keys.validate()?;
    Ok(keys)
}

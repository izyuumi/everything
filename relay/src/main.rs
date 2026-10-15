mod database;
mod error;
mod server;

use clap::{Parser, Subcommand};
use database::Database;
use error::RelayError;
use everything::{crypto::DevicePublicKeys, protocol::DeviceId, relay_protocol::RelayOrigin};
use std::{io::Read, net::SocketAddr, path::PathBuf, process::ExitCode};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about)]
struct Arguments {
    #[arg(long, help = "Dedicated relay SQLite database path")]
    database: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve {
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
    Enroll {
        #[arg(long, help = "Local JSON file containing public device keys only")]
        keys: PathBuf,
    },
    Revoke {
        device_id: DeviceId,
    },
    Devices,
    Status,
}

#[tokio::main]
async fn main() -> ExitCode {
    match execute(Arguments::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("everything-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn execute(arguments: Arguments) -> Result<(), RelayError> {
    let database = Database::open(arguments.database).await?;
    let result = execute_command(&database, arguments.command).await;
    let close_result = database.close().await;
    if let Err(error) = close_result {
        eprintln!("everything-relay: database cleanup failed: {error}");
        if result.is_ok() {
            return Err(error);
        }
    }
    result
}

async fn execute_command(database: &Database, command: Command) -> Result<(), RelayError> {
    match command {
        Command::Serve {
            bind,
            origin,
            max_connections,
        } => {
            let cancellation = CancellationToken::new();
            let run = server::run(
                database.clone(),
                bind,
                origin,
                max_connections,
                cancellation.clone(),
            );
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => result,
                signal = shutdown_signal() => {
                    cancellation.cancel();
                    let result = run.await;
                    signal?;
                    result
                }
            }
        }
        Command::Enroll { keys } => {
            let keys = tokio::task::spawn_blocking(move || read_public_keys(keys))
                .await
                .map_err(|error| RelayError::Configuration(error.to_string()))??;
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
        }
        Command::Revoke { device_id } => {
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
        }
        Command::Devices => {
            println!(
                "{}",
                serde_json::to_string_pretty(&database.devices().await?)?
            );
            Ok(())
        }
        Command::Status => {
            database.prune(server::now_ms()?).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&database.status().await?)?
            );
            Ok(())
        }
    }
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

async fn shutdown_signal() -> Result<(), RelayError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::{LaunchdService, ServiceError, ServiceStatus};

#[derive(Args)]
pub struct ServiceArguments {
    #[arg(long, value_name = "PATH")]
    pub launch_agents_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    Install,
    Start,
    Stop,
    Status,
    Enable,
    Disable,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceReport {
    Installed { path: PathBuf },
    Started,
    Stopped,
    Status { status: ServiceStatus },
    Enabled { enabled: bool },
}

pub async fn manage(
    arguments: ServiceArguments,
    state_directory: PathBuf,
    cancellation: CancellationToken,
) -> Result<ServiceReport, ServiceError> {
    let launch_agents = arguments
        .launch_agents_dir
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/LaunchAgents"))
        })
        .ok_or(ServiceError::UnsafePath("HOME is unavailable"))?;
    let executable = std::env::current_exe()?;
    let service = LaunchdService::new(executable, state_directory, launch_agents)?;
    match arguments.command {
        ServiceCommand::Install => Ok(ServiceReport::Installed {
            path: service.install(cancellation).await?,
        }),
        ServiceCommand::Start => {
            service.start(cancellation).await?;
            Ok(ServiceReport::Started)
        }
        ServiceCommand::Stop => {
            service.stop(cancellation).await?;
            Ok(ServiceReport::Stopped)
        }
        ServiceCommand::Status => Ok(ServiceReport::Status {
            status: service.status(cancellation).await?,
        }),
        ServiceCommand::Enable | ServiceCommand::Disable => {
            let enabled = matches!(arguments.command, ServiceCommand::Enable);
            service.set_enabled(enabled, cancellation).await?;
            Ok(ServiceReport::Enabled { enabled })
        }
    }
}

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::ipc::{
    CommandReceipt, OwnerCommand, OwnerOperation, OwnerOperationResult, OwnerQuery,
    OwnerQueryResult,
};
use crate::protocol::CommandId;
use crate::transport::local::{
    LocalClient, LocalError, LocalOptions, LocalRequest, LocalRequestPayload, LocalResponsePayload,
};

#[derive(Debug, Error)]
pub enum HostClientError {
    #[error(transparent)]
    Transport(#[from] LocalError),
    #[error("host rejected the operation: {0}")]
    Rejected(String),
    #[error("unexpected owner IPC response")]
    Protocol,
}

pub struct HostClient {
    state_directory: PathBuf,
    client: LocalClient,
}

impl HostClient {
    pub fn new(state_directory: impl AsRef<Path>) -> Result<Self, HostClientError> {
        Ok(Self {
            state_directory: state_directory.as_ref().into(),
            client: LocalClient::new(LocalOptions::default())?,
        })
    }

    pub async fn request(
        &self,
        payload: LocalRequestPayload,
        cancellation: CancellationToken,
    ) -> Result<LocalResponsePayload, HostClientError> {
        self.exact_request(LocalRequest::new(payload)?, cancellation)
            .await
    }

    pub async fn exact_request(
        &self,
        request: LocalRequest,
        cancellation: CancellationToken,
    ) -> Result<LocalResponsePayload, HostClientError> {
        match self
            .client
            .request(&self.state_directory, request, cancellation)
            .await?
            .payload
        {
            LocalResponsePayload::Rejected { error } => {
                Err(HostClientError::Rejected(error.message))
            }
            payload => Ok(payload),
        }
    }

    pub async fn query(
        &self,
        query: OwnerQuery,
        cancellation: CancellationToken,
    ) -> Result<OwnerQueryResult, HostClientError> {
        match self
            .request(LocalRequestPayload::Query { query }, cancellation)
            .await?
        {
            LocalResponsePayload::QueryResult { result } => Ok(result),
            _ => Err(HostClientError::Protocol),
        }
    }

    pub async fn operate(
        &self,
        operation: OwnerOperation,
        request_id: CommandId,
        cancellation: CancellationToken,
    ) -> Result<OwnerOperationResult, HostClientError> {
        let mut request = LocalRequest::new(LocalRequestPayload::OwnerOperation { operation })?;
        request.request_id = request_id;
        match self.exact_request(request, cancellation).await? {
            LocalResponsePayload::OwnerOperationResult { result } => Ok(result),
            _ => Err(HostClientError::Protocol),
        }
    }

    pub async fn native_settings(
        &self,
        input: super::settings::native::NativeSettingsInput,
        cancellation: CancellationToken,
    ) -> Result<super::settings::native::NativeSettingsResult, HostClientError> {
        let mut request = LocalRequest::new(LocalRequestPayload::NativeSettings {
            operation: input.operation,
        })?;
        request.request_id = input.request_id;
        match self.exact_request(request, cancellation).await? {
            LocalResponsePayload::NativeSettingsResult { result } => Ok(result),
            _ => Err(HostClientError::Protocol),
        }
    }

    pub async fn submit(
        &self,
        command: OwnerCommand,
        cancellation: CancellationToken,
    ) -> Result<CommandReceipt, HostClientError> {
        let command_id = command.command_id();
        match self
            .request(
                LocalRequestPayload::OwnerCommand { command },
                cancellation.clone(),
            )
            .await?
        {
            LocalResponsePayload::CommandAccepted { .. } => {}
            _ => return Err(HostClientError::Protocol),
        }
        self.await_command(command_id, cancellation).await
    }

    pub async fn submit_domain(
        &self,
        command: crate::protocol::CommandEnvelope,
        cancellation: CancellationToken,
    ) -> Result<CommandReceipt, HostClientError> {
        let command_id = command.command_id;
        match self
            .request(
                LocalRequestPayload::SubmitCommand { command },
                cancellation.clone(),
            )
            .await?
        {
            LocalResponsePayload::CommandAccepted { .. } => {
                self.await_command(command_id, cancellation).await
            }
            _ => Err(HostClientError::Protocol),
        }
    }

    pub async fn await_command(
        &self,
        command_id: CommandId,
        cancellation: CancellationToken,
    ) -> Result<CommandReceipt, HostClientError> {
        loop {
            match self
                .request(
                    LocalRequestPayload::WaitCommand {
                        command_id,
                        timeout_ms: 30_000,
                    },
                    cancellation.clone(),
                )
                .await?
            {
                LocalResponsePayload::CommandWaited {
                    receipt: Some(receipt),
                    timed_out: false,
                } => {
                    if receipt.phase == super::inbox::CommandPhase::Rejected {
                        return Err(HostClientError::Rejected(
                            receipt
                                .result
                                .as_ref()
                                .and_then(|value| value.get("message"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("command application was rejected")
                                .into(),
                        ));
                    }
                    return Ok(receipt);
                }
                LocalResponsePayload::CommandWaited { .. } => {}
                _ => return Err(HostClientError::Protocol),
            }
        }
    }
}

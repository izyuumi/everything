use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio_util::sync::CancellationToken;

use crate::host::runtime::{HostError, HostInner};

use super::service::SettingsServiceError;

impl HostInner {
    pub(in crate::host) async fn apply_settings(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), HostError> {
        let mut changes = self.settings_service.settings_changes();
        loop {
            if cancellation.is_cancelled() || self.draining.load(Ordering::Acquire) {
                return Ok(());
            }
            drop(changes.borrow_and_update());
            let pending = self.settings_service.pending_application_work().await?;
            for operation_id in pending {
                if cancellation.is_cancelled() || self.draining.load(Ordering::Acquire) {
                    return Ok(());
                }
                match self
                    .settings_service
                    .advance_application(operation_id)
                    .await
                {
                    Ok(_) => {}
                    Err(SettingsServiceError::WriteBlocked { request_id }) => {
                        tracing::warn!(
                            %request_id,
                            "settings application awaits original file reconciliation"
                        );
                        break;
                    }
                    Err(SettingsServiceError::Stopped)
                        if cancellation.is_cancelled() || self.draining.load(Ordering::Acquire) =>
                    {
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    changed.map_err(|_| HostError::Worker)?;
                }
            }
        }
    }
}

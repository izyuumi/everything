mod access;
mod pins;
mod remote;
mod replies;

pub(crate) use access::{
    QualifiedSettingsAccessReply, SettingsAccessQualificationError, SettingsAccessReplyFamily,
    SettingsAccessReservationSpec,
};
pub(crate) use remote::SettingsRemotePayload;
pub(crate) use replies::{
    SettingsQueuedReplyCheckError, SettingsReplyComparison, SettingsReplyProjectionError,
};

use std::fmt;
use std::sync::Arc;

use rusqlite::{Row, types::ValueRef};

use crate::devices::{HostSettingsTransport, SettingsHostIdentity};

use super::{ServiceCore, SettingsService, SettingsServiceError};

impl SettingsService {
    pub(crate) fn bind_paired_transport(
        &self,
        transport: &HostSettingsTransport,
    ) -> Result<(), SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        self.core
            .authority
            .attach_paired_transport(Arc::clone(transport.host_identity()))
    }

    pub(in crate::host) fn paired_receipt_authority(
        &self,
        transport: &HostSettingsTransport,
    ) -> Result<SettingsReceiptAuthority, SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        self.core
            .authority
            .paired_receipt_owner(transport.host_identity())?;
        Ok(SettingsReceiptAuthority {
            core: Arc::clone(&self.core),
            host: Arc::clone(transport.host_identity()),
        })
    }
}

#[derive(Clone)]
pub(crate) struct SettingsReceiptAuthority {
    core: Arc<ServiceCore>,
    host: Arc<SettingsHostIdentity>,
}

impl fmt::Debug for SettingsReceiptAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReceiptAuthority([restricted])")
    }
}

fn reply_row_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsServiceError> {
    let ValueRef::Text(bytes) = row.get_ref(column)? else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    if bytes.len() > maximum {
        return Err(SettingsServiceError::CorruptJournal);
    }
    std::str::from_utf8(bytes).map_err(|_| SettingsServiceError::CorruptJournal)
}

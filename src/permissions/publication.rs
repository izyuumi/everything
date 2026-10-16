use super::{
    ApprovalChoice, Authorizer, PermissionError, StoredRecord, approval_id_for_operation,
    decode_record, decode_request, decode_resolution, validate_identifier,
};
use crate::protocol::{ConversationId, OperationId, PermissionDecision, UnixMillis};
use crate::storage::{StorageError, Store};

const MAX_PAGE_SIZE: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDecisionPublication {
    conversation_id: ConversationId,
    operation_id: OperationId,
    resolution_id: String,
    decision: PermissionDecision,
    resolved_at: UnixMillis,
    source_digest: String,
}

impl PermissionDecisionPublication {
    pub fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn decision(&self) -> PermissionDecision {
        self.decision
    }

    pub fn resolved_at(&self) -> UnixMillis {
        self.resolved_at
    }

    pub(crate) fn resolution_id(&self) -> &str {
        &self.resolution_id
    }

    pub(crate) fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub(crate) fn revalidate(&self, store: &Store) -> Result<(), PermissionError> {
        if Self::from_store(store, &self.resolution_id)? != *self {
            return Err(PermissionError::ApprovalConflict(
                self.resolution_id.clone(),
            ));
        }
        Ok(())
    }

    fn from_store(store: &Store, resolution_id: &str) -> Result<Self, PermissionError> {
        validate_identifier(resolution_id)?;
        let row = store
            .permission(resolution_id)?
            .ok_or_else(|| PermissionError::ApprovalNotFound(resolution_id.into()))?;
        let StoredRecord::Resolution {
            approval_id,
            resolved_at,
            ..
        } = decode_record(&row)?.record
        else {
            return Err(PermissionError::MalformedRecord(row.id));
        };
        let request = store
            .permission(&approval_id)?
            .ok_or_else(|| PermissionError::ApprovalNotFound(approval_id.clone()))?;
        let request = decode_request(&request)?;
        let resolution = decode_resolution(&row, &request)?;
        let malformed = || PermissionError::MalformedRecord(resolution_id.into());
        Ok(Self {
            source_digest: super::digest(&(&request, &resolution, resolved_at))?,
            conversation_id: request
                .scope
                .conversation_id
                .parse()
                .map_err(|_| malformed())?,
            operation_id: request
                .scope
                .operation_id
                .parse()
                .map_err(|_| malformed())?,
            resolution_id: row.id,
            decision: match resolution.choice {
                ApprovalChoice::Allow => PermissionDecision::Allow,
                ApprovalChoice::Deny => PermissionDecision::Deny,
            },
            resolved_at: UnixMillis::try_from(u64::try_from(resolved_at).map_err(|_| malformed())?)
                .map_err(|_| malformed())?,
        })
    }
}

impl Authorizer {
    pub fn decision_publication_ids(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PermissionError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(PermissionError::InvalidInspection(
                "publication page limit must be between one and sixty-four",
            ));
        }
        if let Some(after) = after {
            validate_identifier(after)?;
        }
        let connection = self.store.connection()?;
        let mut statement = connection
            .prepare("SELECT CASE WHEN typeof(id)='text' AND length(CAST(id AS BLOB)) BETWEEN 1 AND 256 THEN id END FROM permissions WHERE id>='decision:' AND id<'decision;' AND id>coalesce(?1,'') ORDER BY id LIMIT ?2")
            .map_err(StorageError::from)?;
        let rows = statement
            .query_map(rusqlite::params![after, limit as i64], |row| {
                row.get::<_, Option<String>>(0)
            })
            .map_err(StorageError::from)?;
        rows.map(|row| {
            row.map_err(StorageError::from)?
                .ok_or_else(|| PermissionError::MalformedRecord("publication identifier".into()))
        })
        .collect()
    }

    pub fn decision_publication(
        &self,
        resolution_id: &str,
    ) -> Result<PermissionDecisionPublication, PermissionError> {
        PermissionDecisionPublication::from_store(&self.store, resolution_id)
    }
}

pub(crate) fn decision_publication_id(
    conversation_id: ConversationId,
    operation_id: OperationId,
) -> Result<String, PermissionError> {
    Ok(super::resolution_id(&approval_id_for_operation(
        &conversation_id.to_string(),
        &operation_id.to_string(),
    )?))
}

pub(crate) fn stored_decision_publication(
    store: &Store,
    conversation_id: ConversationId,
    operation_id: OperationId,
) -> Result<PermissionDecisionPublication, PermissionError> {
    PermissionDecisionPublication::from_store(
        store,
        &decision_publication_id(conversation_id, operation_id)?,
    )
}

use super::{
    APPROVAL_LIFETIME_MS, ApprovalStatus, ApprovalView, Authorizer, PermissionError,
    approval_id_for_operation, decode_request, resolution_id, validate_identifier, validate_time,
};
use crate::storage::StorageError;
use serde::{Deserialize, Serialize};

pub const MAX_APPROVAL_METADATA_BYTES: usize = 256 * 1_024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCursor {
    pub requested_at: i64,
    pub approval_id: String,
}

impl ApprovalCursor {
    pub fn validate(&self) -> Result<(), PermissionError> {
        validate_time(self.requested_at)?;
        validate_identifier(&self.approval_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalSummary {
    pub id: String,
    pub conversation_id: String,
    pub task_id: String,
    pub operation_id: String,
    pub tool_name: String,
    pub requested_at: i64,
    pub expires_at: i64,
    pub arguments_bytes: u64,
    pub status: ApprovalStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPage {
    pub approvals: Vec<ApprovalSummary>,
    pub next_cursor: Option<ApprovalCursor>,
}

impl Authorizer {
    pub fn approval_for_id(
        &self,
        conversation_id: &str,
        approval_id: &str,
        now: i64,
    ) -> Result<Option<ApprovalView>, PermissionError> {
        validate_identifier(conversation_id)?;
        validate_identifier(approval_id)?;
        validate_time(now)?;
        let Some(row) = self.store.permission(approval_id)? else {
            if let Some(orphan) = self.store.permission(&resolution_id(approval_id))? {
                return Err(PermissionError::MalformedRecord(orphan.id));
            }
            return Ok(None);
        };
        let request = decode_request(&row)?;
        if request.scope.conversation_id != conversation_id {
            return Err(PermissionError::ApprovalConflict(approval_id.into()));
        }
        let status = self.status_for_request(&request, now)?;
        Ok(Some(ApprovalView { request, status }))
    }

    pub fn pending_approval_page(
        &self,
        conversation_id: &str,
        cursor: Option<&ApprovalCursor>,
        limit: usize,
        now: i64,
    ) -> Result<ApprovalPage, PermissionError> {
        validate_identifier(conversation_id)?;
        validate_time(now)?;
        if !(1..=100).contains(&limit) {
            return Err(PermissionError::InvalidInspection(
                "page limit must be between one and one hundred",
            ));
        }
        if let Some(cursor) = cursor {
            cursor.validate()?;
        }
        let connection = self.store.connection()?;
        let mut statement = connection.prepare(
            "SELECT
                CASE WHEN typeof(p.id)='text' AND length(CAST(p.id AS BLOB)) BETWEEN 1 AND 256 THEN p.id END,
                CASE WHEN json_type(p.payload,'$.version')='integer' AND json_extract(p.payload,'$.version')=1 THEN 1 END,
                CASE WHEN json_type(p.payload,'$.record.request.id')='text' AND length(CAST(json_extract(p.payload,'$.record.request.id') AS BLOB)) BETWEEN 1 AND 256 THEN json_extract(p.payload,'$.record.request.id') END,
                CASE WHEN json_type(p.payload,'$.record.request.scope.conversation_id')='text' AND length(CAST(json_extract(p.payload,'$.record.request.scope.conversation_id') AS BLOB)) BETWEEN 1 AND 256 THEN json_extract(p.payload,'$.record.request.scope.conversation_id') END,
                CASE WHEN json_type(p.payload,'$.record.request.scope.task_id')='text' AND length(CAST(json_extract(p.payload,'$.record.request.scope.task_id') AS BLOB)) BETWEEN 1 AND 256 THEN json_extract(p.payload,'$.record.request.scope.task_id') END,
                CASE WHEN json_type(p.payload,'$.record.request.scope.operation_id')='text' AND length(CAST(json_extract(p.payload,'$.record.request.scope.operation_id') AS BLOB)) BETWEEN 1 AND 256 THEN json_extract(p.payload,'$.record.request.scope.operation_id') END,
                CASE WHEN json_type(p.payload,'$.record.request.scope.tool_name')='text' AND length(CAST(json_extract(p.payload,'$.record.request.scope.tool_name') AS BLOB)) BETWEEN 1 AND 128 THEN json_extract(p.payload,'$.record.request.scope.tool_name') END,
                CASE WHEN json_type(p.payload,'$.record.request.requested_at')='integer' THEN json_extract(p.payload,'$.record.request.requested_at') END,
                CASE WHEN json_type(p.payload,'$.record.request.expires_at')='integer' THEN json_extract(p.payload,'$.record.request.expires_at') END,
                length(CAST(json_quote(json_extract(p.payload,'$.record.request.arguments')) AS BLOB)),
                p.created_at,p.expires_at
            FROM permissions p
            WHERE p.conversation_id=?1 AND p.revoked_at IS NULL AND p.created_at<=?2 AND p.expires_at>?2
                AND json_extract(p.payload,'$.record.kind')='approval'
                AND NOT EXISTS(SELECT 1 FROM permissions decision WHERE decision.id='decision:'||p.id)
                AND (?3 IS NULL OR (p.created_at,p.id)>(?3,?4))
            ORDER BY p.created_at,p.id LIMIT ?5",
        ).map_err(StorageError::from)?;
        let mut rows = statement
            .query(rusqlite::params![
                conversation_id,
                now,
                cursor.map(|cursor| cursor.requested_at),
                cursor.map(|cursor| cursor.approval_id.as_str()),
                (limit + 1) as i64,
            ])
            .map_err(StorageError::from)?;
        let mut approvals = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        while let Some(row) = rows.next().map_err(StorageError::from)? {
            if approvals.len() == limit {
                more = true;
                break;
            }
            let id: String = row.get(0).map_err(StorageError::from)?;
            let version: u32 = row.get(1).map_err(StorageError::from)?;
            let request_id: String = row.get(2).map_err(StorageError::from)?;
            let summary = ApprovalSummary {
                id: id.clone(),
                conversation_id: row.get(3).map_err(StorageError::from)?,
                task_id: row.get(4).map_err(StorageError::from)?,
                operation_id: row.get(5).map_err(StorageError::from)?,
                tool_name: row.get(6).map_err(StorageError::from)?,
                requested_at: row.get(7).map_err(StorageError::from)?,
                expires_at: row.get(8).map_err(StorageError::from)?,
                arguments_bytes: row.get(9).map_err(StorageError::from)?,
                status: ApprovalStatus::Pending,
            };
            let created_at: i64 = row.get(10).map_err(StorageError::from)?;
            let expires_at: i64 = row.get(11).map_err(StorageError::from)?;
            if version != super::RECORD_VERSION
                || id != request_id
                || summary.conversation_id != conversation_id
                || summary.requested_at != created_at
                || summary.expires_at != expires_at
                || summary
                    .expires_at
                    .checked_sub(summary.requested_at)
                    .is_none_or(|duration| !(1..=APPROVAL_LIFETIME_MS).contains(&duration))
                || summary.arguments_bytes > crate::tools::MAX_TOOL_ARGUMENT_BYTES as u64
                || approval_id_for_operation(conversation_id, &summary.operation_id)? != id
            {
                return Err(PermissionError::MalformedRecord(id));
            }
            for identifier in [&summary.task_id, &summary.operation_id, &summary.tool_name] {
                validate_identifier(identifier)?;
            }
            let encoded = serde_json::to_vec(&summary)?;
            if bytes + encoded.len() + 1 > MAX_APPROVAL_METADATA_BYTES - 1_024 {
                more = true;
                break;
            }
            bytes += encoded.len() + 1;
            approvals.push(summary);
        }
        let next_cursor = if more {
            Some(
                approvals
                    .last()
                    .map(|approval| ApprovalCursor {
                        requested_at: approval.requested_at,
                        approval_id: approval.id.clone(),
                    })
                    .ok_or(PermissionError::InvalidInspection(
                        "one approval summary exceeds its metadata byte limit",
                    ))?,
            )
        } else {
            None
        };
        let page = ApprovalPage {
            approvals,
            next_cursor,
        };
        if serde_json::to_vec(&page)?.len() > MAX_APPROVAL_METADATA_BYTES {
            return Err(PermissionError::InvalidInspection(
                "approval metadata page exceeds its byte limit",
            ));
        }
        Ok(page)
    }
}

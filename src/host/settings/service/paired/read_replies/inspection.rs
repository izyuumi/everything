use super::*;
use crate::devices::{
    BindingGeneration, OriginalKind, OriginalLink, OriginalView, OwnedCommittedSettingsInspection,
    RemoteServiceReceipt, ServiceLookup, SettingsOriginalInspectionLookup,
};
use crate::host::settings::actions::SettingsActionTarget;
use crate::host::settings::operations::{self, OperationRecord, PairedReplyHistoryAnchor};
use crate::protocol::CommandId;
use crate::storage::settings_agent::{SettingsTurnFence, read_inspection_turn_on};
use crate::storage::settings_originals::{
    PairedSettingsSessionReservation, RemoteOriginalClose, RemoteOriginalIdentity,
    RemoteOriginalKey, RemoteOriginalLink, RemoteOriginalRecord, RemoteOriginalTerminal,
    RemotePublicCommand, check_inspection_mutation_on, read_inspection_session_on,
    read_original_on,
};

pub(crate) struct QualifiedSettingsInspectionReply<'transaction, 'connection, 'owner> {
    read: QualifiedSettingsReadReply<'transaction, 'connection, 'owner>,
    request: &'owner OwnedCommittedSettingsInspection,
    disclosure: InspectionDisclosure,
}

pub(crate) struct SettingsInspectionReplyProjection {
    plaintext: Zeroizing<Vec<u8>>,
    floor: (UnixMillis, Instant),
    deadline: Instant,
}

impl SettingsInspectionReplyProjection {
    pub(crate) fn into_parts(self) -> (Zeroizing<Vec<u8>>, (UnixMillis, Instant), Instant) {
        (self.plaintext, self.floor, self.deadline)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct InspectionKeys {
    enabled: bool,
    dimensions: bool,
}

impl InspectionKeys {
    fn include(&mut self, key: SettingKey) -> Result<(), SettingsServiceError> {
        match key {
            SettingKey::EmbeddingsEnabled => self.enabled = true,
            SettingKey::EmbeddingsDimensions => self.dimensions = true,
            _ => return Err(SettingsServiceError::CorruptJournal),
        }
        Ok(())
    }

    fn check(&self, grant: &ReadReplyGrantFacts) -> Result<(), SettingsServiceError> {
        if self.enabled && !grant.readable_keys.contains(&SettingKey::EmbeddingsEnabled)
            || self.dimensions
                && !grant
                    .readable_keys
                    .contains(&SettingKey::EmbeddingsDimensions)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }
}

struct InspectionDisclosure {
    original: Option<RemoteOriginalRecord>,
    session: Option<PairedSettingsSessionReservation>,
    turn: Option<SettingsTurnFence>,
    operations: Vec<(CommandId, Option<OperationRecord>)>,
    keys: InspectionKeys,
}

pub(in crate::host::settings::service::paired) struct InspectionReplyComparison {
    read: SnapshotReplyComparison,
    original: Option<InspectionOriginalFloor>,
    members: Vec<CommandId>,
    anchors: Vec<(CommandId, Option<PairedReplyHistoryAnchor>)>,
    keys: InspectionKeys,
}

#[derive(PartialEq, Eq)]
struct InspectionOriginalFloor {
    generation: BindingGeneration,
    parent: Uuid,
    admitted_at: UnixMillis,
    interval: Option<(UnixMillis, UnixMillis)>,
    kind: InspectionIntentFloor,
    link: Option<RemoteOriginalLink>,
    close: Option<RemoteOriginalClose>,
    terminal: Option<RemoteOriginalTerminal>,
}

#[derive(PartialEq, Eq)]
enum InspectionIntentFloor {
    ChatOpen,
    Suppressed,
    Send { chat_id: Uuid, provider: String },
    Update { revision: u64, keys: InspectionKeys },
}

impl SettingsReceiptAuthority {
    pub(crate) fn qualify_inspection_reply_on<'transaction, 'connection, 'owner>(
        &'owner self,
        store: &Store,
        transaction: &'transaction Transaction<'connection>,
        mutation: &'owner OwnedMutexGuard<()>,
        request: &'owner OwnedCommittedSettingsInspection,
    ) -> Result<
        QualifiedSettingsInspectionReply<'transaction, 'connection, 'owner>,
        SettingsReadReplyError,
    > {
        let read =
            self.qualify_read_reply_on(store, transaction, mutation, request.read_owner())?;
        let disclosure =
            read_inspection_disclosure(transaction, request, read.owner, &read.grant, None)?;
        let observed =
            self.observe_read_reply_on(store, transaction, mutation, request.read_owner())?;
        check_read_reply_observation(&read, &observed)?;
        Ok(QualifiedSettingsInspectionReply {
            read,
            request,
            disclosure,
        })
    }

    pub(crate) fn recheck_inspection_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsInspectionReply<'_, '_, '_>,
    ) -> Result<(), SettingsReadReplyError> {
        self.check_read_reply_association(transaction, &qualified.read)?;
        let read = self.read_read_reply_on(
            store,
            transaction,
            qualified.read.mutation,
            qualified.read.request,
        )?;
        check_read_reply_reservation(transaction, qualified.read.request)?;
        let disclosure = read_inspection_disclosure(
            transaction,
            qualified.request,
            read.owner,
            &read.grant,
            None,
        )?;
        check_same_disclosure(&qualified.disclosure, &disclosure)?;
        let observed = self.finish_read_reply_observation(
            store,
            qualified.read.mutation,
            qualified.read.request,
            read,
        )?;
        check_read_reply_observation(&qualified.read, &observed)
    }

    pub(crate) fn project_inspection_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsInspectionReply<'_, '_, '_>,
        permit: &ReplyBuildPermit,
    ) -> Result<SettingsInspectionReplyProjection, SettingsReplyProjectionError> {
        permit.check_inspection(qualified.request)?;
        self.recheck_inspection_reply_on(store, transaction, qualified)?;
        let lookup = inspection_lookup(qualified)?;
        let source = qualified
            .request
            .read_owner()
            .verified_request()
            .authority_source();
        let in_reply_to = source
            .receipt_metadata()?
            .message_id
            .parse()
            .map_err(SettingsServiceError::from)?;
        let observed = self.observe_read_reply_on(
            store,
            transaction,
            qualified.read.mutation,
            qualified.read.request,
        )?;
        check_read_reply_observation(&qualified.read, &observed)?;
        let packet = SettingsReadReplyPacket::inspection(
            source.request(),
            in_reply_to,
            observed.wall,
            lookup,
        )?;
        Ok(SettingsInspectionReplyProjection {
            plaintext: packet.encode_checked()?,
            floor: (observed.wall, observed.monotonic),
            deadline: observed.reply_deadline,
        })
    }

    pub(in crate::host::settings::service::paired) fn recheck_consumed_inspection_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        consumed.check_on(transaction)?;
        let SettingsReplyQualification::Inspection(qualified) = consumed.sealed().qualification()
        else {
            return Err(SettingsReplyPublicationError::Conflict);
        };
        self.check_read_reply_association(transaction, &qualified.read)
            .map_err(SettingsReplyProjectionError::from)?;
        let created_at = consumed.sealed().header().created_at_ms;
        let read = self
            .read_read_reply_on(
                store,
                transaction,
                qualified.read.mutation,
                qualified.read.request,
            )
            .map_err(SettingsReplyProjectionError::from)?;
        let disclosure = read_inspection_disclosure(
            transaction,
            qualified.request,
            read.owner,
            &read.grant,
            None,
        )
        .map_err(SettingsReplyProjectionError::from)?;
        check_same_disclosure(&qualified.disclosure, &disclosure)
            .map_err(SettingsReplyProjectionError::from)?;
        let observed = self
            .finish_read_reply_observation(
                store,
                qualified.read.mutation,
                qualified.read.request,
                read,
            )
            .map_err(SettingsReplyProjectionError::from)?;
        check_read_reply_observation(&qualified.read, &observed)
            .map_err(SettingsReplyProjectionError::from)?;
        if observed.wall.get() < created_at {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        Ok(())
    }

    pub(in crate::host::settings::service::paired) fn capture_inspection_reply_comparison(
        &self,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsInspectionReply<'_, '_, '_>,
    ) -> Result<InspectionReplyComparison, SettingsReadReplyError> {
        self.check_read_reply_association(transaction, &qualified.read)?;
        let mut remaining = 8192;
        let mut anchors = Vec::with_capacity(qualified.disclosure.operations.len());
        for (id, operation) in &qualified.disclosure.operations {
            anchors.push((
                *id,
                operation
                    .as_ref()
                    .map(|operation| {
                        PairedReplyHistoryAnchor::capture_inspection_on(
                            transaction,
                            operation,
                            &mut remaining,
                        )
                    })
                    .transpose()?,
            ));
        }
        Ok(InspectionReplyComparison {
            read: self.capture_snapshot_reply_comparison(transaction, &qualified.read)?,
            original: qualified.disclosure.original.as_ref().map(original_floor),
            members: qualified
                .disclosure
                .turn
                .as_ref()
                .map_or_else(Vec::new, |turn| turn.mutation_ids.clone()),
            anchors,
            keys: qualified.disclosure.keys,
        })
    }

    pub(in crate::host::settings::service::paired) fn recheck_queued_inspection_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
        comparison: &InspectionReplyComparison,
    ) -> Result<(), super::super::replies::SettingsQueuedReplyCheckError> {
        let request = checked_inspection_owner(queued, comparison)?;
        let read = self
            .read_read_reply_on(store, transaction, mutation, request.read_owner())
            .map_err(SettingsReplyProjectionError::from)?;
        let current = read_inspection_disclosure(
            transaction,
            request,
            read.owner,
            &read.grant,
            Some(comparison),
        )
        .map_err(SettingsReplyProjectionError::from)?;
        check_queued_disclosure(comparison, &current, &read.grant)
            .map_err(SettingsReplyProjectionError::from)?;
        queued.request()?;
        let created_at = queued.committed()?.header().created_at_ms;
        let observed = self
            .finish_read_reply_observation(store, mutation, request.read_owner(), read)
            .map_err(SettingsReplyProjectionError::from)?;
        check_snapshot_comparison(&comparison.read, &observed)?;
        if observed.wall.get() < created_at {
            return Err(SettingsReplyPublicationError::Conflict.into());
        }
        Ok(())
    }

    pub(in crate::host::settings::service::paired) fn recheck_queued_inspection_socket_lifetime(
        &self,
        store: &Store,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
        comparison: &InspectionReplyComparison,
    ) -> Result<(), super::super::replies::SettingsQueuedReplyCheckError> {
        let request = checked_inspection_owner(queued, comparison)?;
        self.check_joined_read_context(store, mutation, request.read_owner())
            .map_err(SettingsReplyProjectionError::from)?;
        queued.request()?;
        let header = queued.committed()?.header();
        let observed = self
            .core
            .authority
            .observe_committed_read_query(
                &self.host,
                request.read_owner(),
                &self.core.application_stop,
            )
            .map_err(SettingsReplyProjectionError::from)?;
        let floor = &comparison.read;
        if !Arc::ptr_eq(&observed.query, &floor.query)
            || observed.floor != floor.floor
            || observed.deadline != floor.query_deadline
            || observed.parent_lifetime.expires_at != floor.parent_lifetime.expires_at
            || observed.parent_lifetime.deadline != floor.parent_lifetime.deadline
            || observed.wall < floor.wall
            || observed.monotonic < floor.monotonic
            || observed.wall < request.read_owner().reply_reservation().created_at
            || observed.wall.get() < header.created_at_ms
        {
            return Err(SettingsReplyPublicationError::Conflict.into());
        }
        if observed.wall >= request.read_owner().reply_reservation().expires_at
            || observed.wall >= floor.grant.expires_at
            || observed.monotonic >= floor.reply_deadline
        {
            return Err(SettingsReplyPublicationError::Expired.into());
        }
        Ok(())
    }
}

fn read_inspection_disclosure(
    connection: &Connection,
    request: &OwnedCommittedSettingsInspection,
    owner: SettingsGrantOwner,
    grant: &ReadReplyGrantFacts,
    prior: Option<&InspectionReplyComparison>,
) -> Result<InspectionDisclosure, SettingsServiceError> {
    let source = request.read_owner().verified_request().authority_source();
    let SettingsReadRequest::InspectOriginal(intent) = source.request() else {
        return Err(SettingsServiceError::OutsideScope);
    };
    let original = read_original_on(
        connection,
        RemoteOriginalKey {
            host_id: owner.host_id,
            actor_device_id: source.verified_peer_keys().device_id,
            request_id: intent.request_id,
        },
    )?;
    let mut disclosure = InspectionDisclosure {
        original,
        session: None,
        turn: None,
        operations: Vec::new(),
        keys: InspectionKeys::default(),
    };
    let Some(original) = &disclosure.original else {
        return Ok(disclosure);
    };
    let (peer, principal) = match &original.identity {
        RemoteOriginalIdentity::Accepted(identity) => {
            (&identity.peer_keys, identity.owner_principal_id)
        }
        RemoteOriginalIdentity::SuppressedChatOpen(identity) => {
            (&identity.peer_keys, identity.owner_principal_id)
        }
    };
    if peer != source.verified_peer_keys() || principal != owner.principal_id {
        return Err(SettingsServiceError::OutsideScope);
    }
    let mut remaining = 8192;
    match &original.identity {
        RemoteOriginalIdentity::SuppressedChatOpen(_) => {
            if intent.operation_id.is_some() {
                return Err(SettingsServiceError::OutsideScope);
            }
        }
        RemoteOriginalIdentity::Accepted(identity) => match &identity.command {
            RemotePublicCommand::ChatOpen => {
                if intent.operation_id.is_some() {
                    return Err(SettingsServiceError::OutsideScope);
                }
                if let Some(RemoteOriginalLink::Chat { session_id }) = &original.link {
                    disclosure.session = Some(read_inspection_session_on(connection, *session_id)?);
                }
            }
            RemotePublicCommand::Update {
                enabled,
                dimensions,
                ..
            } => {
                if intent.operation_id.is_some() {
                    return Err(SettingsServiceError::OutsideScope);
                }
                disclosure.keys.enabled = enabled.is_some();
                disclosure.keys.dimensions = dimensions.is_some();
                disclosure.keys.check(grant)?;
                if let Some(RemoteOriginalLink::Operation { operation_id }) = &original.link {
                    let operation = operations::read(connection, *operation_id)?
                        .ok_or(SettingsServiceError::CorruptJournal)?;
                    super::super::check_original_reply_operation(identity, &operation)?;
                    include_operation_keys(&mut disclosure.keys, &operation)?;
                    check_inspection_history(connection, &operation, &mut remaining, prior)?;
                    disclosure.operations.push((*operation_id, Some(operation)));
                }
            }
            RemotePublicCommand::Send { chat_id, .. } => {
                let session = read_inspection_session_on(connection, *chat_id)?;
                if let Some(RemoteOriginalLink::Turn {
                    session_id,
                    command_id,
                }) = &original.link
                {
                    let turn = read_inspection_turn_on(connection, *command_id)?;
                    if session.session_id != *session_id
                        || turn.session_id != *session_id
                        || turn.command_id != *command_id
                    {
                        return Err(SettingsServiceError::CorruptJournal);
                    }
                    if intent
                        .operation_id
                        .is_some_and(|selected| !turn.mutation_ids.contains(&selected))
                    {
                        return Err(SettingsServiceError::OutsideScope);
                    }
                    for id in &turn.mutation_ids {
                        let present = check_inspection_mutation_on(connection, *id, &session)?;
                        let operation = operations::read(connection, *id)?;
                        if present != operation.is_some() {
                            return Err(SettingsServiceError::CorruptJournal);
                        }
                        if let Some(operation) = &operation {
                            include_operation_keys(&mut disclosure.keys, operation)?;
                            check_inspection_history(connection, operation, &mut remaining, prior)?;
                        } else {
                            if intent.operation_id == Some(*id) {
                                disclosure.keys.enabled = true;
                                disclosure.keys.dimensions = true;
                            }
                        }
                        disclosure.operations.push((*id, operation));
                    }
                    disclosure.turn = Some(turn);
                } else if intent.operation_id.is_some() {
                    return Err(SettingsServiceError::OutsideScope);
                }
                disclosure.session = Some(session);
            }
        },
    }
    disclosure.keys.check(grant)?;
    Ok(disclosure)
}

fn include_operation_keys(
    keys: &mut InspectionKeys,
    operation: &OperationRecord,
) -> Result<(), SettingsServiceError> {
    let SettingsActionTarget::Patch(patch) = &operation.identity.target else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    for change in &patch.changes {
        keys.include(change.key)?;
    }
    if let Some(write) = operation.write() {
        for key in &write.intent.changed_keys {
            keys.include(*key)?;
        }
    }
    Ok(())
}

fn check_same_disclosure(
    left: &InspectionDisclosure,
    right: &InspectionDisclosure,
) -> Result<(), SettingsServiceError> {
    if left.original != right.original
        || left.session != right.session
        || left.turn != right.turn
        || left.keys != right.keys
        || left.operations.len() != right.operations.len()
        || left.operations.iter().zip(&right.operations).any(
            |((left_id, left), (right_id, right))| {
                left_id != right_id
                    || match (left, right) {
                        (None, None) => false,
                        (Some(left), Some(right)) => {
                            left.identity != right.identity
                                || left.inspection() != right.inspection()
                        }
                        _ => true,
                    }
            },
        )
    {
        return Err(SettingsServiceError::Conflict);
    }
    Ok(())
}

fn inspection_lookup(
    qualified: &QualifiedSettingsInspectionReply<'_, '_, '_>,
) -> Result<SettingsOriginalInspectionLookup, SettingsServiceError> {
    let disclosure = &qualified.disclosure;
    let Some(original) = &disclosure.original else {
        return Ok(SettingsOriginalInspectionLookup::NotRecorded);
    };
    let source = qualified
        .request
        .read_owner()
        .verified_request()
        .authority_source();
    let SettingsReadRequest::InspectOriginal(intent) = source.request() else {
        return Err(SettingsServiceError::OutsideScope);
    };
    let original_kind = match &original.identity {
        RemoteOriginalIdentity::SuppressedChatOpen(_) => OriginalKind::ChatOpen,
        RemoteOriginalIdentity::Accepted(identity) => match identity.command {
            RemotePublicCommand::ChatOpen => OriginalKind::ChatOpen,
            RemotePublicCommand::Update { .. } => OriginalKind::Update,
            RemotePublicCommand::Send { .. } => OriginalKind::Send,
        },
    };
    let link = match &original.link {
        None => OriginalLink::Unlinked,
        Some(RemoteOriginalLink::Chat { session_id }) => OriginalLink::Chat {
            chat_id: *session_id,
        },
        Some(RemoteOriginalLink::Turn {
            session_id,
            command_id,
        }) => OriginalLink::Turn {
            chat_id: *session_id,
            turn_id: *command_id,
        },
        Some(RemoteOriginalLink::Operation { operation_id }) => OriginalLink::Operation {
            operation_id: *operation_id,
        },
    };
    let selected = match &original.link {
        Some(RemoteOriginalLink::Operation { operation_id }) => Some(*operation_id),
        Some(RemoteOriginalLink::Turn { .. }) => intent.operation_id,
        _ => None,
    };
    let service = match selected {
        None => ServiceLookup::NotRequested,
        Some(selected) => match disclosure.operations.iter().find(|(id, _)| *id == selected) {
            Some((_, Some(operation))) => {
                let inspection = operation.inspection();
                ServiceLookup::Recorded {
                    receipt: RemoteServiceReceipt {
                        operation_id: inspection.binding.request_id,
                        expected_revision: inspection.binding.expected_revision,
                        updated_at: inspection.updated_at,
                        phase: super::super::replies::remote_phase(&inspection.phase)?,
                        write: inspection.write,
                        application: inspection.application,
                    },
                }
            }
            Some((_, None)) => ServiceLookup::NotRecorded,
            None => return Err(SettingsServiceError::CorruptJournal),
        },
    };
    Ok(SettingsOriginalInspectionLookup::Recorded {
        original_generation: generation(&original.identity),
        view: Box::new(OriginalView {
            original_kind,
            terminal: original.terminal,
            link,
            mutation_operation_ids: disclosure
                .turn
                .as_ref()
                .map_or_else(Vec::new, |turn| turn.mutation_ids.clone()),
            service,
        }),
    })
}

fn generation(identity: &RemoteOriginalIdentity) -> BindingGeneration {
    let (grant_id, grant_revision, host_epoch, connection_epoch) = match identity {
        RemoteOriginalIdentity::Accepted(identity) => (
            identity.grant_id,
            identity.grant_revision,
            identity.host_epoch,
            identity.connection_epoch,
        ),
        RemoteOriginalIdentity::SuppressedChatOpen(identity) => (
            identity.grant_id,
            identity.grant_revision,
            identity.host_epoch,
            identity.connection_epoch,
        ),
    };
    BindingGeneration {
        grant_id,
        grant_revision,
        host_epoch,
        connection_epoch,
    }
}

fn original_floor(original: &RemoteOriginalRecord) -> InspectionOriginalFloor {
    let (parent, admitted_at, interval, kind) = match &original.identity {
        RemoteOriginalIdentity::SuppressedChatOpen(identity) => (
            identity.remote_parent_id,
            identity.admitted_at,
            None,
            InspectionIntentFloor::Suppressed,
        ),
        RemoteOriginalIdentity::Accepted(identity) => (
            identity.remote_parent_id,
            identity.admitted_at,
            Some((identity.issued_at, identity.expires_at)),
            match &identity.command {
                RemotePublicCommand::ChatOpen => InspectionIntentFloor::ChatOpen,
                RemotePublicCommand::Send {
                    chat_id,
                    provider_binding_sha256,
                } => InspectionIntentFloor::Send {
                    chat_id: *chat_id,
                    provider: provider_binding_sha256.clone(),
                },
                RemotePublicCommand::Update {
                    expected_settings_revision,
                    enabled,
                    dimensions,
                } => InspectionIntentFloor::Update {
                    revision: *expected_settings_revision,
                    keys: InspectionKeys {
                        enabled: enabled.is_some(),
                        dimensions: dimensions.is_some(),
                    },
                },
            },
        ),
    };
    InspectionOriginalFloor {
        generation: generation(&original.identity),
        parent,
        admitted_at,
        interval,
        kind,
        link: original.link.clone(),
        close: original.close.clone(),
        terminal: original.terminal,
    }
}

fn check_queued_disclosure(
    comparison: &InspectionReplyComparison,
    current: &InspectionDisclosure,
    grant: &ReadReplyGrantFacts,
) -> Result<(), SettingsServiceError> {
    comparison.keys.check(grant)?;
    match (&comparison.original, &current.original) {
        (None, None) => return Ok(()),
        (Some(floor), Some(current)) => {
            let now = original_floor(current);
            if floor.generation != now.generation
                || floor.parent != now.parent
                || floor.admitted_at != now.admitted_at
                || floor.interval != now.interval
                || floor.kind != now.kind
                || floor
                    .link
                    .as_ref()
                    .is_some_and(|link| now.link.as_ref() != Some(link))
                || floor
                    .close
                    .as_ref()
                    .is_some_and(|close| now.close.as_ref() != Some(close))
                || floor
                    .terminal
                    .is_some_and(|terminal| now.terminal != Some(terminal))
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
        _ => return Err(SettingsServiceError::Conflict),
    }
    if comparison.members.iter().any(|id| {
        current
            .turn
            .as_ref()
            .is_none_or(|turn| !turn.mutation_ids.contains(id))
    }) {
        return Err(SettingsServiceError::CorruptJournal);
    }
    for (id, anchor) in &comparison.anchors {
        if anchor.is_some()
            && current
                .operations
                .iter()
                .find(|(current, _)| current == id)
                .is_none_or(|(_, operation)| operation.is_none())
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    Ok(())
}

fn checked_inspection_owner<'owner>(
    queued: &'owner OwnedQueuedSettingsReply,
    comparison: &InspectionReplyComparison,
) -> Result<
    &'owner OwnedCommittedSettingsInspection,
    super::super::replies::SettingsQueuedReplyCheckError,
> {
    let SettingsReplyBuildRequest::Inspection(request) = queued.request()? else {
        return Err(SettingsReplyPublicationError::Conflict.into());
    };
    let source = request.read_owner().verified_request().authority_source();
    if !Arc::ptr_eq(source.invocation_identity(), &comparison.read.invocation)
        || source.receipt_floor() != comparison.read.floor
        || !matches!(source.request(), SettingsReadRequest::InspectOriginal(_))
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    Ok(request)
}

impl fmt::Debug for QualifiedSettingsInspectionReply<'_, '_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("QualifiedSettingsInspectionReply([current requesting-peer disclosure])")
    }
}

fn check_inspection_history(
    connection: &Connection,
    operation: &OperationRecord,
    remaining: &mut usize,
    prior: Option<&InspectionReplyComparison>,
) -> Result<(), SettingsServiceError> {
    match prior
        .and_then(|prior| {
            prior
                .anchors
                .iter()
                .find(|(id, _)| *id == operation.identity.binding.request_id)
        })
        .and_then(|(_, anchor)| anchor.as_ref())
    {
        Some(anchor) => anchor.check_inspection_current_on(connection, operation, remaining),
        None => {
            PairedReplyHistoryAnchor::capture_inspection_on(connection, operation, remaining)?;
            Ok(())
        }
    }
}

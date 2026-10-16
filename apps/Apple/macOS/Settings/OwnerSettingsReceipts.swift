import Foundation

enum OwnerSettingsFileOutcome: Equatable, Sendable {
    case unsettled
    case notApplied
    case applied(revision: UInt64)
}

struct OwnerSettingsReceipt: Sendable {
    let operation: SettingsOperationInspection
    let snapshot: OwnerSettingsDocument?
    let fileOutcome: OwnerSettingsFileOutcome

    var restartRequired: Bool {
        operation.write?.resolution == .applied && operation.write?.restartRequired == true
    }

    var application: SettingsApplicationState? { operation.application }

    var applicationPending: Bool {
        guard case .finished(let result) = operation.phase,
              case .pendingApplication = result.state else { return false }
        return operation.application == nil
    }

    fileprivate init(operation: SettingsOperationInspection, snapshot: OwnerSettingsDocument?) throws {
        self.operation = operation
        self.snapshot = snapshot
        switch operation.write?.resolution {
        case .applied:
            guard let revision = operation.write?.revision else { throw OwnerSettingsMetadataError.unexpectedReceipt }
            fileOutcome = .applied(revision: revision)
        case .notApplied: fileOutcome = .notApplied
        case nil:
            switch operation.phase {
            case .applying: fileOutcome = .unsettled
            case .finished(let result):
                switch result.state {
                case .unknown: fileOutcome = .unsettled
                case .hostActionRequired, .conflict, .unavailable, .failed, .cancelled: fileOutcome = .notApplied
                default: throw OwnerSettingsMetadataError.unexpectedReceipt
                }
            default: throw OwnerSettingsMetadataError.unexpectedReceipt
            }
        }
    }
}

extension OwnerSettingsPreparedLink {
    func receipt(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt {
        let fields = try receiptFields(response, kind: "settings_receipt", keys: ["kind", "operation", "settings", "restart_required"])
        let operation = try matchingOperation(fields["operation"], previous: previous)
        guard let restart = fields["restart_required"]?.bool,
              let settings = fields["settings"],
              restart == (operation.write?.resolution == .applied && operation.write?.restartRequired == true) else {
            throw OwnerSettingsMetadataError.unexpectedReceipt
        }
        let snapshot: OwnerSettingsDocument?
        if settings == .null {
            snapshot = nil
        } else {
            guard operation.write?.resolution == .applied else { throw OwnerSettingsMetadataError.unexpectedReceipt }
            snapshot = try matchingAppliedSnapshot(settings)
        }
        return try OwnerSettingsReceipt(operation: operation, snapshot: snapshot)
    }

    func inspected(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt {
        let fields = try receiptFields(response, kind: "settings_operation", keys: ["kind", "operation"])
        let operation = try matchingOperation(fields["operation"], previous: previous)
        return try OwnerSettingsReceipt(operation: operation, snapshot: nil)
    }

    func reconciled(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt {
        let fields = try receiptFields(response, kind: "settings_reconciled", keys: ["kind", "operation"])
        let operation = try matchingOperation(fields["operation"], previous: previous)
        guard operation.write != nil else { throw OwnerSettingsMetadataError.unexpectedReceipt }
        return try OwnerSettingsReceipt(operation: operation, snapshot: nil)
    }

    private func receiptFields(_ response: JSONValue, kind: String, keys: Set<String>) throws -> [String: JSONValue] {
        do {
            _ = try response.encoded(maximum: OwnerSettingsDocument.maximumBytes + SettingsWire.snapshotLimit + 1024)
            guard case .object(let fields) = response, Set(fields.keys) == keys,
                  fields["kind"] == .string(kind) else { throw OwnerSettingsMetadataError.unexpectedReceipt }
            return fields
        } catch {
            throw OwnerSettingsMetadataError.unexpectedReceipt
        }
    }

    private func matchingOperation(_ value: JSONValue?, previous: OwnerSettingsReceipt?) throws -> SettingsOperationInspection {
        do {
            guard let value else { throw OwnerSettingsMetadataError.unexpectedReceipt }
            let operation = try SettingsWire.decode(SettingsOperationInspection.self, from: value.encoded(maximum: SettingsWire.snapshotLimit))
            guard operation.binding.requestID == ownerRequest.id.value,
                  operation.binding.clientSessionID == ownerRequest.id.value,
                  operation.binding.principalID == hostID,
                  operation.binding.scope == .host(hostID: hostID),
                  operation.binding.expectedRevision == expectedRevision,
                  operation.origin == .localOwner(requestID: ownerRequest.id.value) else {
                throw OwnerSettingsMetadataError.unexpectedReceipt
            }
            if let previous {
                guard operation.binding == previous.operation.binding,
                      operation.origin == previous.operation.origin else { throw OwnerSettingsMetadataError.unexpectedReceipt }
                if case .finished(let result) = previous.operation.phase, result.state != .unknown {
                    guard operation.phase == previous.operation.phase else { throw OwnerSettingsMetadataError.unexpectedReceipt }
                }
                if let originalWrite = previous.operation.write {
                    guard let write = operation.write,
                          write.expectedRevision == originalWrite.expectedRevision,
                          write.revision == originalWrite.revision,
                          write.changedKeys == originalWrite.changedKeys,
                          write.restartRequired == originalWrite.restartRequired,
                          originalWrite.resolution == nil || write.resolution == originalWrite.resolution else {
                        throw OwnerSettingsMetadataError.unexpectedReceipt
                    }
                }
                if let application = previous.operation.application {
                    guard operation.application == application else { throw OwnerSettingsMetadataError.unexpectedReceipt }
                }
            }
            if let write = operation.write {
                guard write.expectedRevision == expectedRevision, write.revision == expectedRevision + 1,
                      write.changedKeys.isEmpty, !write.restartRequired else { throw OwnerSettingsMetadataError.unexpectedReceipt }
            }
            switch operation.phase {
            case .applying: break
            case .finished(let result):
                switch result.state {
                case .applied, .pendingApplication:
                    guard operation.write?.resolution == .applied else { throw OwnerSettingsMetadataError.unexpectedReceipt }
                case .hostActionRequired, .conflict, .unavailable, .failed, .cancelled, .unknown: break
                default: throw OwnerSettingsMetadataError.unexpectedReceipt
                }
            default: throw OwnerSettingsMetadataError.unexpectedReceipt
            }
            return operation
        } catch {
            throw OwnerSettingsMetadataError.unexpectedReceipt
        }
    }
}

import Foundation

struct NativeSettingsOperation: Identifiable, Sendable {
    let id: UUID
    let target: SettingsActionTarget
    let originalSnapshot: SettingsSnapshot
    let originalRegistry: SettingRegistry
    var prepared: SettingsOperationResult?
    var inspection: SettingsOperationInspection?
    var issue: NativeSettingsSessionError?
    var isWorking = false
    var executionPrepared = false
    var wasSubmitted = false
    var cancelledBeforeSubmission = false

    var result: SettingsOperationResult? {
        if let inspection {
            if case .finished(let result) = inspection.phase { return result }
            return nil
        }
        return prepared
    }

    var confirmation: SettingsConfirmationReference? {
        guard !executionPrepared else { return nil }
        if let inspection {
            if case .awaitingConfirmation(let reference) = inspection.phase { return reference }
            return nil
        }
        if case .requiresConfirmation(let reference) = prepared?.state { return reference }
        return nil
    }

    var blocksNewChanges: Bool {
        if cancelledBeforeSubmission { return false }
        if isWorking || issue != nil { return true }
        if let inspection {
            switch inspection.phase {
            case .awaitingAction, .awaitingConfirmation, .inFlight, .applying: return true
            case .finished(let result): return result.state == .unknown
            }
        }
        guard let prepared else { return true }
        switch prepared.state {
        case .requiresConfirmation, .requiresUserAction, .unknown: return true
        default: return false
        }
    }

    var canReconcile: Bool {
        inspection?.write?.resolution == nil && inspection?.write != nil && !isWorking
    }

    mutating func adopt(_ value: SettingsOperationInspection) throws {
        try value.validate()
        guard value.binding.requestID == id, value.binding.scope == target.scope,
              value.binding.expectedRevision == target.expectedRevision else { throw NativeSettingsSessionError.invalidResponse }
        if let original = inspection {
            guard original.binding == value.binding, original.origin == value.origin else { throw NativeSettingsSessionError.invalidResponse }
            if case .finished(let previous) = original.phase {
                if original.write == nil, value.write != nil { throw NativeSettingsSessionError.invalidResponse }
                if value.phase != original.phase {
                    guard previous.state == .unknown, let write = original.write, write.resolution == nil,
                          case .finished = value.phase, value.write?.resolution != nil else {
                        throw NativeSettingsSessionError.invalidResponse
                    }
                }
            }
            if let write = original.write {
                guard let next = value.write, write.expectedRevision == next.expectedRevision,
                      write.revision == next.revision, write.changedKeys == next.changedKeys,
                      write.restartRequired == next.restartRequired,
                      write.resolution == nil || write.resolution == next.resolution else { throw NativeSettingsSessionError.invalidResponse }
            }
            if let application = original.application, application != value.application { throw NativeSettingsSessionError.invalidResponse }
        }
        inspection = value
        issue = nil
    }
}

enum NativeSettingsText {
    static func label(_ key: SettingKey) -> String {
        switch key {
        case .modelSelection: "Model"
        case .toolSelection: "Tool selection"
        case .embeddingsEnabled: "Local embeddings"
        case .embeddingsDimensions: "Embedding dimensions"
        case .embeddingsModel: "Embedding model"
        case .notificationsCompletion: "Completion notifications"
        case .notificationsAttention: "Attention notifications"
        case .notificationAuthorization: "Notification authorization"
        }
    }

    static func value(_ value: SettingValue?) -> String {
        guard let value else { return "Not reported" }
        switch value {
        case .model(let selection):
            guard let selection else { return "No model selected" }
            let endpoint: String
            switch selection.endpoint {
            case .official: endpoint = "Official endpoint"
            case .loopback(let url): endpoint = url
            }
            return selection.provider.rawValue + " · " + selection.model + " · " + endpoint
        case .toolSelection(.local): return "Local discovery"
        case .toolSelection(.systemOne(let endpoint, let model, _)): return "System One · " + model + " · " + endpoint
        case .boolean(let enabled): return enabled ? "Enabled" : "Disabled"
        case .dimensions(let dimensions): return dimensions.map { String($0) } ?? "Model default"
        case .embeddingModel(let model): return model ?? "Unavailable"
        case .notificationAuthorization(let state): return state.rawValue.replacingOccurrences(of: "_", with: " ")
        }
    }

    static func state(_ state: SettingState) -> String {
        switch state {
        case .effective: "Effective"
        case .pendingApplication: "Pending application"
        case .restartRequired: "Restart required"
        case .unavailable: "Unavailable"
        }
    }

    static func unavailable(_ reason: SettingsUnavailableReason) -> String {
        switch reason {
        case .hostOffline: "Host is offline."
        case .providerUnavailable: "The provider is unavailable. A saved model does not verify successful inference."
        case .embeddingMetadataUnavailable: "Embedding model metadata is unavailable."
        case .embeddingBoundaryUnavailable: "The local embedding boundary is unavailable; lexical retrieval remains available."
        case .nativeActionUnavailable: "The native action is unavailable."
        case .permissionDenied: "Permission was denied."
        case .expired: "The original authority expired."
        }
    }

    static func applyBehavior(_ behavior: SettingApplyBehavior) -> String {
        switch behavior {
        case .subsequentRun: "Subsequent runs"
        case .subsequentSelection: "Subsequent tool selection"
        case .stopNewWorkThenApply: "After new work stops"
        case .rebuildRequired: "Index rebuild required"
        case .readOnly: "Read-only observation"
        case .thisDevice: "This device"
        }
    }

    static func result(_ state: SettingsOperationState) -> String {
        switch state {
        case .applied(let revision): "Saved revision \(revision); application acknowledged"
        case .completed: "Completed; no settings revision was created"
        case .pendingApplication(let revision): "Saved revision \(revision); application pending"
        case .requiresConfirmation: "Awaiting your confirmation of the original changes"
        case .requiresUserAction: "Awaiting a native action"
        case .hostActionRequired: "A supported native action is required"
        case .restartRequired(let revision): "Saved revision \(revision); Host restart required"
        case .conflict(let revision): "Conflict at revision \(revision); reload and review your drafts"
        case .unavailable(let reason): unavailable(reason)
        case .failed(let reason): "Failed: " + reason.rawValue.replacingOccurrences(of: "_", with: " ")
        case .unknown: "Outcome unknown; inspect the original operation before another save"
        case .cancelled: "Original operation cancelled"
        }
    }

    static func operation(_ operation: NativeSettingsOperation) -> String {
        if operation.cancelledBeforeSubmission { return "Cancelled before submission; no Host receipt requested" }
        if let inspection = operation.inspection {
            switch inspection.phase {
            case .awaitingConfirmation: return "Awaiting your confirmation of the original changes"
            case .awaitingAction: return "Awaiting a native action"
            case .inFlight: return "Original native action is in flight"
            case .applying(let expected, let revision): return "Saving original revision \(expected) → \(revision); inspect before another save"
            case .finished(let result):
                if case .pendingApplication(let revision) = result.state, let application = inspection.application {
                    return "Saved revision \(revision); " + Self.application(application)
                }
                return Self.result(result.state)
            }
        }
        if let prepared = operation.prepared { return result(prepared.state) }
        return operation.isWorking ? "Submitting the original request" : "Original request retained; no receipt observed"
    }

    static func application(_ state: SettingsApplicationState) -> String {
        switch state {
        case .applied: "Application acknowledged"
        case .restartRequired: "Host restart required"
        case .unavailable(let reason): "Saved configuration: " + unavailable(reason)
        case .failed(let reason): "Saved configuration could not be applied: " + reason.rawValue.replacingOccurrences(of: "_", with: " ")
        }
    }
}

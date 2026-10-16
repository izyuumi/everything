import Foundation
import Observation

enum NativeCredentialChoice: Equatable, Sendable {
    case selectedModel(storage: SettingsCredentialStorage)
    case configuredModel(SettingsModelCredentialBinding)
    case systemOne
}

enum NativeCredentialIssue: Sendable {
    case invalidSecret
    case staleContext
    case unconfirmed
    case native(NativeSettingsSessionError)
    case metadata(OwnerSettingsMetadataError)

    var message: String {
        switch self {
        case .invalidSecret: "Enter a nonempty credential of at most 8192 UTF-8 bytes without control characters."
        case .staleContext: "The original Host settings are stale or changed. Refresh and review the original binding before another entry."
        case .unconfirmed: "The original outcome is unconfirmed. Inspect that operation before another action."
        case .native(let error): error.errorDescription ?? "The original native operation is unconfirmed."
        case .metadata(let error): error.errorDescription ?? "The original metadata operation is unconfirmed."
        }
    }
}

enum NativeCredentialLinkState: Sendable {
    case notRequested
    case notSubmitted
    case working
    case observed(OwnerSettingsReceipt)
    case unconfirmed
}

@MainActor
struct NativeCredentialOperation: Identifiable {
    let id: UUID
    let handle: NativeSettingsStore.CredentialHandle
    let request: SettingsActionRequest
    let operation: NativeSettingsOperation
    let initial: Bool
    let presentationRetired: Bool
    let linkRequestID: UUID?
    let linkState: NativeCredentialLinkState
    let issue: NativeCredentialIssue?

    var storageCompleted: Bool {
        guard let inspection = operation.inspection, inspection.write == nil, inspection.application == nil,
              case .finished(let result) = inspection.phase, case .completed = result.state else { return false }
        return true
    }

    var binding: SettingsCredentialBinding? {
        guard case .credentialEntry(let binding) = request.action else { return nil }
        return binding
    }
}

@MainActor
@Observable
final class NativeCredentialField {
    var value = ""

    func clear() { value = "" }

    func take() -> String {
        let result = value
        clear()
        return result
    }
}

import Foundation

enum NativeSettingsAssistantWire {
    enum Phase: String, Decodable, Sendable {
        case ready
        case running
        case completed
        case closing
        case closed
        case unavailable
        case stoppedUncertain = "stopped_uncertain"
        case stoppedUnusable = "stopped_unusable"
        case unknown
    }

    enum ProvisionalState: String, Decodable, Sendable {
        case streaming
        case interrupted
    }

    struct ProvisionalText: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let attemptID: UUID
        let state: ProvisionalState
        let text: String

        var description: String { "NativeSettingsAssistantProvisionalText([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["attempt_id", "state", "text"])
            attemptID = try fields.required(UUID.self, "attempt_id")
            state = try fields.required(ProvisionalState.self, "state")
            text = try fields.required(String.self, "text")
            try SettingsWire.identifier(attemptID)
            try NativeSettingsAssistantWire.assistantText(text)
        }
    }

    struct ContextObservation: Decodable, Sendable {
        let revision: UInt64
        let observedAt: UInt64
        let stale: Bool

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["revision", "observed_at", "stale"])
            revision = try fields.required(UInt64.self, "revision")
            observedAt = try fields.required(UInt64.self, "observed_at")
            stale = try fields.required(Bool.self, "stale")
            try SettingsWire.revision(revision)
            try SettingsWire.timestamp(observedAt)
            guard observedAt > 0 else { throw SettingsWireError.invalid }
        }
    }

    struct Opened: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let chatID: UUID
        let provider: SettingsProviderKind
        let model: String
        let recipient: String
        let providerBindingSHA256: String
        let tools: [String]
        let expiresAt: UInt64

        var description: String { "NativeSettingsAssistantOpened([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["chat_id", "provider", "model", "recipient", "provider_binding_sha256", "tools", "expires_at"])
            chatID = try fields.required(UUID.self, "chat_id")
            provider = try fields.required(SettingsProviderKind.self, "provider")
            model = try fields.required(String.self, "model")
            recipient = try fields.required(String.self, "recipient")
            providerBindingSHA256 = try fields.required(String.self, "provider_binding_sha256")
            tools = try fields.required([String].self, "tools")
            expiresAt = try fields.required(UInt64.self, "expires_at")
            try SettingsWire.identifier(chatID)
            try SettingsWire.digest(providerBindingSHA256)
            try SettingsWire.timestamp(expiresAt)
            try NativeSettingsAssistantWire.displayText(model, maximum: 256)
            try NativeSettingsAssistantWire.displayText(recipient, maximum: 4096)
            guard expiresAt > 0, tools == SettingsToolName.allCases.map(\.providerName) else {
                throw SettingsWireError.invalid
            }
        }
    }

    struct Status: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let chatID: UUID
        let phase: Phase
        let turnID: UUID?
        let assistantText: String
        let textRevision: UInt64
        let provisional: ProvisionalText?
        let context: ContextObservation?
        let operationIDs: [UUID]
        let modelAttempts: UInt32
        let chargedTokens: UInt64
        let reservedOutputTokens: UInt64
        let providerComplete: Bool
        let usageComplete: Bool

        var description: String { "NativeSettingsAssistantStatus([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["chat_id", "phase", "turn_id", "assistant_text", "text_revision", "provisional", "context", "operation_ids", "model_attempts", "charged_tokens", "reserved_output_tokens", "provider_complete", "usage_complete"])
            chatID = try fields.required(UUID.self, "chat_id")
            phase = try fields.required(Phase.self, "phase")
            turnID = try fields.optional(UUID.self, "turn_id")
            assistantText = try fields.required(String.self, "assistant_text")
            textRevision = try fields.required(UInt64.self, "text_revision")
            provisional = try fields.optional(ProvisionalText.self, "provisional")
            context = try fields.optional(ContextObservation.self, "context")
            operationIDs = try fields.required([UUID].self, "operation_ids")
            modelAttempts = try fields.required(UInt32.self, "model_attempts")
            chargedTokens = try fields.required(UInt64.self, "charged_tokens")
            reservedOutputTokens = try fields.required(UInt64.self, "reserved_output_tokens")
            providerComplete = try fields.required(Bool.self, "provider_complete")
            usageComplete = try fields.required(Bool.self, "usage_complete")
            try SettingsWire.identifier(chatID)
            try NativeSettingsAssistantWire.assistantText(assistantText)
            guard operationIDs.count <= 8, Set(operationIDs).count == operationIDs.count,
                  modelAttempts <= 4, reservedOutputTokens <= 8192,
                  turnID != nil || (provisional == nil && context == nil) else {
                throw SettingsWireError.invalid
            }
            if let turnID { try SettingsWire.identifier(turnID) }
            for operationID in operationIDs { try SettingsWire.identifier(operationID) }
        }
    }

    enum OpenState: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        case notAccepted
        case ready(Opened)
        case closing(chatID: UUID)
        case closed(chatID: UUID?)
        case unavailable
        case unknown(chatID: UUID?)

        var description: String { "NativeSettingsAssistantOpenState([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            let state = try fields.required(String.self, "state")
            switch state {
            case "not_accepted":
                try fields.only(["state"])
                self = .notAccepted
            case "ready":
                try fields.only(["state", "chat"])
                self = .ready(try fields.required(Opened.self, "chat"))
            case "closing":
                try fields.only(["state", "chat_id"])
                let chatID = try fields.required(UUID.self, "chat_id")
                try SettingsWire.identifier(chatID)
                self = .closing(chatID: chatID)
            case "closed", "unknown":
                try fields.only(["state", "chat_id"])
                let chatID = try fields.optional(UUID.self, "chat_id")
                if let chatID { try SettingsWire.identifier(chatID) }
                if state == "closed" {
                    self = .closed(chatID: chatID)
                } else {
                    self = .unknown(chatID: chatID)
                }
            case "unavailable":
                try fields.only(["state"])
                self = .unavailable
            default:
                throw SettingsWireError.invalid
            }
        }
    }

    enum OperationLookup: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        case notRecorded
        case inspected(SettingsOperationInspection)

        var description: String { "NativeSettingsAssistantOperationLookup([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            switch try fields.required(String.self, "state") {
            case "not_recorded":
                try fields.only(["state"])
                self = .notRecorded
            case "inspected":
                try fields.only(["state", "operation"])
                self = .inspected(try fields.required(SettingsOperationInspection.self, "operation"))
            default:
                throw SettingsWireError.invalid
            }
        }
    }

    enum Reply: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        case opened(Opened)
        case openState(openRequestID: UUID, state: OpenState)
        case accepted(chatID: UUID, turnID: UUID)
        case status(Status)
        case operation(chatID: UUID, turnID: UUID, operationID: UUID, lookup: OperationLookup)
        case actionDetails(chatID: UUID, turnID: UUID, operationID: UUID, operation: SettingsOperationInspection, target: SettingsActionTarget)
        case unavailable

        var description: String { "NativeSettingsAssistantReply([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            switch try fields.required(String.self, "kind") {
            case "opened":
                try fields.only(["kind", "chat"])
                self = .opened(try fields.required(Opened.self, "chat"))
            case "open_state":
                try fields.only(["kind", "open_request_id", "state"])
                let openRequestID = try fields.required(UUID.self, "open_request_id")
                try SettingsWire.identifier(openRequestID)
                self = .openState(openRequestID: openRequestID, state: try fields.required(OpenState.self, "state"))
            case "accepted":
                try fields.only(["kind", "chat_id", "turn_id"])
                let chatID = try fields.required(UUID.self, "chat_id")
                let turnID = try fields.required(UUID.self, "turn_id")
                try SettingsWire.identifier(chatID)
                try SettingsWire.identifier(turnID)
                self = .accepted(chatID: chatID, turnID: turnID)
            case "status":
                try fields.only(["kind", "status"])
                self = .status(try fields.required(Status.self, "status"))
            case "operation":
                try fields.only(["kind", "chat_id", "turn_id", "operation_id", "lookup"])
                let chatID = try fields.required(UUID.self, "chat_id")
                let turnID = try fields.required(UUID.self, "turn_id")
                let operationID = try fields.required(UUID.self, "operation_id")
                let lookup = try fields.required(OperationLookup.self, "lookup")
                try SettingsWire.identifier(chatID)
                try SettingsWire.identifier(turnID)
                try SettingsWire.identifier(operationID)
                if case .inspected(let operation) = lookup {
                    try NativeSettingsAssistantWire.operation(operation, chatID: chatID, operationID: operationID)
                }
                self = .operation(chatID: chatID, turnID: turnID, operationID: operationID, lookup: lookup)
            case "action_details":
                try fields.only(["kind", "chat_id", "turn_id", "operation_id", "operation", "target"])
                let chatID = try fields.required(UUID.self, "chat_id")
                let turnID = try fields.required(UUID.self, "turn_id")
                let operationID = try fields.required(UUID.self, "operation_id")
                let operation = try fields.required(SettingsOperationInspection.self, "operation")
                let target = try fields.required(SettingsActionTarget.self, "target")
                try SettingsWire.identifier(chatID)
                try SettingsWire.identifier(turnID)
                try SettingsWire.identifier(operationID)
                try NativeSettingsAssistantWire.operation(operation, chatID: chatID, operationID: operationID)
                try NativeSettingsAssistantWire.target(target, operation: operation)
                self = .actionDetails(chatID: chatID, turnID: turnID, operationID: operationID, operation: operation, target: target)
            case "unavailable":
                try fields.only(["kind"])
                self = .unavailable
            default:
                throw SettingsWireError.invalid
            }
        }

        func validate(for request: RequestExpectation) throws {
            try request.validate()
            switch (request, self) {
            case (.open, .opened), (.open, .unavailable), (.send, .unavailable):
                return
            case let (.open(expectedID), .openState(openRequestID, _)),
                 let (.openStatus(expectedID), .openState(openRequestID, _)):
                guard expectedID == openRequestID else { throw SettingsWireError.invalid }
            case let (.closeOpen(expectedID), .openState(openRequestID, state)):
                guard expectedID == openRequestID else { throw SettingsWireError.invalid }
                if case .notAccepted = state { throw SettingsWireError.invalid }
            case let (.send(expectedChat, expectedTurn), .accepted(chatID, turnID)):
                guard expectedChat == chatID && expectedTurn == turnID else {
                    throw SettingsWireError.invalid
                }
            case let (.poll(chatID), .status(status)), let (.close(chatID), .status(status)):
                guard chatID == status.chatID else { throw SettingsWireError.invalid }
            case let (.inspectOperation(nativeParentID, hostID, expectedChat, expectedTurn, expectedOperation),
                      .operation(chatID, turnID, operationID, lookup)):
                guard expectedChat == chatID, expectedTurn == turnID, expectedOperation == operationID else {
                    throw SettingsWireError.invalid
                }
                if case .inspected(let operation) = lookup {
                    try NativeSettingsAssistantWire.scopedOperation(operation, chatID: chatID, operationID: operationID,
                                                                    nativeParentID: nativeParentID, hostID: hostID)
                }
            case let (.actionDetails(nativeParentID, hostID, expectedChat, expectedTurn, expectedOperation, reference),
                      .actionDetails(chatID, turnID, operationID, operation, target)):
                guard expectedChat == chatID, expectedTurn == turnID, expectedOperation == operationID else {
                    throw SettingsWireError.invalid
                }
                try NativeSettingsAssistantWire.scopedOperation(operation, chatID: chatID, operationID: operationID,
                                                                nativeParentID: nativeParentID, hostID: hostID)
                try NativeSettingsAssistantWire.target(target, operation: operation)
                switch (operation.phase, reference) {
                case let (.awaitingConfirmation(actual), .confirmation(expected)) where actual == expected:
                    return
                case let (.awaitingAction(actual), .action(expected)) where actual == expected:
                    return
                default:
                    throw SettingsWireError.invalid
                }
            default:
                throw SettingsWireError.invalid
            }
        }
    }

    enum RequestExpectation: Sendable {
        case open(openRequestID: UUID)
        case openStatus(openRequestID: UUID)
        case closeOpen(openRequestID: UUID)
        case send(chatID: UUID, turnID: UUID)
        case poll(chatID: UUID)
        case close(chatID: UUID)
        case inspectOperation(nativeParentID: UUID, hostID: UUID, chatID: UUID, turnID: UUID, operationID: UUID)
        case actionDetails(nativeParentID: UUID, hostID: UUID, chatID: UUID, turnID: UUID, operationID: UUID, reference: NativeActionReference)

        fileprivate func validate() throws {
            switch self {
            case .open(let openRequestID), .openStatus(let openRequestID), .closeOpen(let openRequestID):
                try SettingsWire.identifier(openRequestID)
            case .send(let chatID, let turnID):
                try SettingsWire.identifier(chatID)
                try SettingsWire.identifier(turnID)
            case .poll(let chatID), .close(let chatID):
                try SettingsWire.identifier(chatID)
            case let .inspectOperation(nativeParentID, hostID, chatID, turnID, operationID):
                try NativeSettingsAssistantWire.scopedIdentity(nativeParentID: nativeParentID, hostID: hostID,
                                                               chatID: chatID, turnID: turnID, operationID: operationID)
            case let .actionDetails(nativeParentID, hostID, chatID, turnID, operationID, reference):
                try NativeSettingsAssistantWire.scopedIdentity(nativeParentID: nativeParentID, hostID: hostID,
                                                               chatID: chatID, turnID: turnID, operationID: operationID)
                try reference.validate()
            }
        }
    }

    private static func displayText(_ value: String, maximum: Int) throws {
        guard !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              value.utf8.count <= maximum, !value.contains("\0") else {
            throw SettingsWireError.invalid
        }
    }

    private static func assistantText(_ value: String) throws {
        guard value.utf8.count <= 16_384, !value.contains("\0") else {
            throw SettingsWireError.invalid
        }
    }

    private static func scopedIdentity(nativeParentID: UUID, hostID: UUID, chatID: UUID,
                                       turnID: UUID, operationID: UUID) throws {
        try SettingsWire.identifier(nativeParentID)
        try SettingsWire.identifier(hostID)
        try SettingsWire.identifier(chatID)
        try SettingsWire.identifier(turnID)
        try SettingsWire.identifier(operationID)
    }

    private static func operation(_ value: SettingsOperationInspection, chatID: UUID, operationID: UUID) throws {
        try value.validate()
        guard value.binding.requestID == operationID, value.binding.scope.kind == .host,
              case .settingsAgent(let originalChat, _) = value.origin, originalChat == chatID else {
            throw SettingsWireError.invalid
        }
    }

    private static func scopedOperation(_ value: SettingsOperationInspection, chatID: UUID, operationID: UUID,
                                        nativeParentID: UUID, hostID: UUID) throws {
        try operation(value, chatID: chatID, operationID: operationID)
        guard value.binding.clientSessionID == nativeParentID,
              case .settingsAgent(_, let originalParent) = value.origin, originalParent == nativeParentID,
              case .host(let originalHost) = value.binding.scope, originalHost == hostID else {
            throw SettingsWireError.invalid
        }
    }

    private static func target(_ value: SettingsActionTarget, operation: SettingsOperationInspection) throws {
        try value.validate()
        guard value.requestID == operation.binding.requestID, value.scope == operation.binding.scope,
              value.expectedRevision == operation.binding.expectedRevision else {
            throw SettingsWireError.invalid
        }
        switch (value, operation.phase) {
        case (.patch, .awaitingConfirmation), (.device, .awaitingAction):
            return
        case let (.native(request), .awaitingAction):
            guard case .openSection = request.action else { throw SettingsWireError.invalid }
        default:
            throw SettingsWireError.invalid
        }
    }
}

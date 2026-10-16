import Foundation

enum NativeSettingsWireError: Error, Sendable {
    case invalidRequest
    case invalidResponse
}

enum NativeSettingsWire {
    struct Proof: Codable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let sessionID: UUID
        private let secret: String

        var description: String { "NativeSettingsProof([private])" }
        var debugDescription: String { description }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["session_id", "secret"])
            sessionID = try fields.required(UUID.self, "session_id")
            secret = try fields.required(String.self, "secret")
            try SettingsWire.identifier(sessionID)
            guard secret.utf8.count == 44, let bytes = Data(base64Encoded: secret),
                  bytes.count == 32, bytes.base64EncodedString() == secret else {
                throw NativeSettingsWireError.invalidResponse
            }
        }

        func encode(to encoder: any Encoder) throws {
            var fields = encoder.container(keyedBy: SettingsWireKey.self)
            try fields.encode(sessionID, forKey: SettingsWireKey("session_id"))
            try fields.encode(secret, forKey: SettingsWireKey("secret"))
        }
    }

    struct Opened: Decodable, Sendable {
        let hostID: UUID
        let expiresAt: UInt64
        let proof: Proof

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["host_id", "expires_at", "proof"])
            hostID = try fields.required(UUID.self, "host_id")
            expiresAt = try fields.required(UInt64.self, "expires_at")
            proof = try fields.required(Proof.self, "proof")
            try SettingsWire.identifier(hostID)
            try SettingsWire.timestamp(expiresAt)
            guard expiresAt > 0 else { throw NativeSettingsWireError.invalidResponse }
        }
    }

    enum PairingLifecycle: String, Decodable, Sendable {
        case retained
        case cancellationPending = "cancellation_pending"
        case cancelled
        case expired
        case noLongerPending = "no_longer_pending"
        case unavailable
    }

    struct Invitation: Decodable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let operationID: UUID
        let pairingID: UUID
        let expiresAt: UInt64
        let fingerprint: String
        private let invitation: String

        var description: String { "NativeSettingsInvitation([private])" }
        var debugDescription: String { description }

        func displayText() -> String { invitation }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["operation_id", "pairing_id", "expires_at", "fingerprint", "invitation"])
            operationID = try fields.required(UUID.self, "operation_id")
            pairingID = try fields.required(UUID.self, "pairing_id")
            expiresAt = try fields.required(UInt64.self, "expires_at")
            fingerprint = try fields.required(String.self, "fingerprint")
            invitation = try fields.required(String.self, "invitation")
            try SettingsWire.identifier(operationID)
            try SettingsWire.identifier(pairingID)
            try SettingsWire.timestamp(expiresAt)
            try SettingsWire.text(fingerprint, maximum: 128)
            try SettingsWire.text(invitation, maximum: 8192)
        }
    }

    struct PairingCreation: Decodable, Sendable {
        let operation: SettingsOperationInspection
        let lifecycle: PairingLifecycle
        let invitation: Invitation?

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            try fields.only(["operation", "lifecycle", "invitation"])
            operation = try fields.required(SettingsOperationInspection.self, "operation")
            lifecycle = try fields.required(PairingLifecycle.self, "lifecycle")
            invitation = try fields.optional(Invitation.self, "invitation")
            if let invitation {
                guard invitation.operationID == operation.binding.requestID,
                      invitation.expiresAt <= operation.binding.expiresAt else {
                    throw NativeSettingsWireError.invalidResponse
                }
            }
        }
    }

    enum DeviceResult: Decodable, Sendable {
        case existing(SettingsOperationInspection)
        case pairing(PairingCreation)
        case revoked(SettingsOperationInspection)

        var operation: SettingsOperationInspection {
            switch self {
            case .existing(let value), .revoked(let value): value
            case .pairing(let value): value.operation
            }
        }

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            switch try fields.required(String.self, "kind") {
            case "existing":
                try fields.only(["kind", "operation"])
                self = .existing(try fields.required(SettingsOperationInspection.self, "operation"))
            case "pairing":
                try fields.only(["kind", "creation"])
                self = .pairing(try fields.required(PairingCreation.self, "creation"))
            case "revoked":
                try fields.only(["kind", "operation"])
                self = .revoked(try fields.required(SettingsOperationInspection.self, "operation"))
            default: throw NativeSettingsWireError.invalidResponse
            }
        }
    }

    enum Operation: Sendable {
        case open
        case describe
        case get(SettingsScope)
        case update(SettingsPatch)
        case confirmPatch(NativeActionInvocation)
        case changes(after: UInt64?, limit: Int)
        case reconcile(UUID)
        case requestAction(SettingsActionRequest)
        case devices(scope: SettingsScope, after: UUID?, limit: Int)
        case pairDevice(SettingsDeviceRequest)
        case revokeDevice(SettingsDeviceRequest)
        case confirmDevice(NativeActionInvocation)
        case pairing(UUID)
        case cancelPairing(UUID)
        case details(UUID, NativeActionReference)
        case admit(NativeActionInvocation)
        case submitCredential(NativeActionInvocation, String)
        case inspect(UUID)
        case finalize(NativeActionInvocation, NativeActionObservation)
        case cancel(UUID)
        case close

        var kind: String {
            switch self {
            case .open: "open"
            case .describe: "describe"
            case .get: "get"
            case .update: "update"
            case .confirmPatch: "confirm_patch"
            case .changes: "changes"
            case .reconcile: "reconcile"
            case .requestAction: "request_action"
            case .devices: "devices"
            case .pairDevice: "pair_device"
            case .revokeDevice: "revoke_device"
            case .confirmDevice: "confirm_device"
            case .pairing: "pairing"
            case .cancelPairing: "cancel_pairing"
            case .details: "details"
            case .admit: "admit"
            case .submitCredential: "submit_credential"
            case .inspect: "inspect"
            case .finalize: "finalize"
            case .cancel: "cancel"
            case .close: "close"
            }
        }

        func validate() throws {
            switch self {
            case .get(let scope): try scope.validate()
            case .update(let patch): try patch.validate()
            case .requestAction(let request): try request.validate()
            case .pairDevice(let request):
                try request.validate()
                guard request.action == .pair else { throw NativeSettingsWireError.invalidRequest }
            case .revokeDevice(let request):
                try request.validate()
                guard case .revoke = request.action else { throw NativeSettingsWireError.invalidRequest }
            case .confirmPatch(let invocation), .confirmDevice(let invocation), .admit(let invocation):
                try invocation.validate()
            case .submitCredential(let invocation, let secret):
                try invocation.validate()
                try SettingsWire.text(secret, maximum: 8192)
            case .finalize(let invocation, let observation):
                try invocation.validate()
                try observation.validate()
            case .changes(let after, let limit):
                if let after { try SettingsWire.timestamp(after) }
                guard (1...128).contains(limit) else { throw NativeSettingsWireError.invalidRequest }
            case .devices(let scope, let after, let limit):
                try scope.validate()
                if let after { try SettingsWire.identifier(after) }
                guard scope.kind == .host, (1...100).contains(limit) else { throw NativeSettingsWireError.invalidRequest }
            case .details(let id, let reference):
                try SettingsWire.identifier(id)
                try reference.validate()
            case .reconcile(let id), .pairing(let id), .cancelPairing(let id), .inspect(let id), .cancel(let id):
                try SettingsWire.identifier(id)
            case .open, .describe, .close: break
            }
        }
    }

    enum Response: Decodable, Sendable {
        case described(SettingRegistry)
        case snapshot(SettingsSnapshot)
        case changes(SettingsChangePage)
        case devices(SettingsDevicePage)
        case deviceConfirmed(DeviceResult)
        case pairing(PairingCreation)
        case opened(Opened)
        case prepared(SettingsOperationResult)
        case details(SettingsOperationInspection, SettingsActionTarget)
        case admitted(SettingsOperationInspection, SettingsActionTarget)
        case inspected(SettingsOperationInspection)
        case closed(UUID)

        init(from decoder: any Decoder) throws {
            let fields = try SettingsWireFields(decoder)
            switch try fields.required(String.self, "kind") {
            case "described":
                try fields.only(["kind", "registry"])
                self = .described(try fields.required(SettingRegistry.self, "registry"))
            case "snapshot":
                try fields.only(["kind", "snapshot"])
                self = .snapshot(try fields.required(SettingsSnapshot.self, "snapshot"))
            case "changes":
                try fields.only(["kind", "page"])
                self = .changes(try fields.required(SettingsChangePage.self, "page"))
            case "devices":
                try fields.only(["kind", "page"])
                self = .devices(try fields.required(SettingsDevicePage.self, "page"))
            case "device_confirmed":
                try fields.only(["kind", "result"])
                self = .deviceConfirmed(try fields.required(DeviceResult.self, "result"))
            case "pairing":
                try fields.only(["kind", "creation"])
                self = .pairing(try fields.required(PairingCreation.self, "creation"))
            case "opened":
                try fields.only(["kind", "session"])
                self = .opened(try fields.required(Opened.self, "session"))
            case "prepared":
                try fields.only(["kind", "result"])
                self = .prepared(try fields.required(SettingsOperationResult.self, "result"))
            case "details", "admitted":
                try fields.only(["kind", "operation", "target"])
                let operation = try fields.required(SettingsOperationInspection.self, "operation")
                let target = try fields.required(SettingsActionTarget.self, "target")
                if try fields.required(String.self, "kind") == "details" { self = .details(operation, target) }
                else { self = .admitted(operation, target) }
            case "inspected":
                try fields.only(["kind", "operation"])
                self = .inspected(try fields.required(SettingsOperationInspection.self, "operation"))
            case "closed":
                try fields.only(["kind", "session_id"])
                let id = try fields.required(UUID.self, "session_id")
                try SettingsWire.identifier(id)
                self = .closed(id)
            default: throw NativeSettingsWireError.invalidResponse
            }
        }

        func validate(for request: Operation, proof: Proof?) throws {
            switch (request, self) {
            case (.open, .opened): return
            case (.describe, .described(let registry)): try registry.validate()
            case (.get(let scope), .snapshot(let snapshot)) where snapshot.scope == scope: try snapshot.validate()
            case (.changes(let after, let limit), .changes(let page)): try page.validate(afterSequence: after, limit: limit)
            case (.devices(let scope, let after, let limit), .devices(let page)):
                guard case .host(let hostID) = scope, page.hostID == hostID, page.devices.count <= limit,
                      page.devices.allSatisfy({ device in after.map { $0.uuidString < device.deviceID.uuidString } ?? true }) else {
                    throw NativeSettingsWireError.invalidResponse
                }
                try page.validate()
            case (.update(let patch), .inspected(let operation)):
                guard operation.binding.requestID == patch.requestID, operation.binding.scope == patch.scope,
                      operation.binding.expectedRevision == patch.expectedRevision else { throw NativeSettingsWireError.invalidResponse }
                try Self.validate(operation, proof: proof)
            case (.requestAction(let request), .prepared(let result)):
                try Self.validate(result, requestID: request.requestID, scope: request.scope)
            case (.pairDevice(let request), .prepared(let result)), (.revokeDevice(let request), .prepared(let result)):
                try Self.validate(result, requestID: request.requestID, scope: request.scope)
            case (.details(let id, let reference), .details(let operation, let target)):
                try Self.validate(operation, target: target, operationID: id, proof: proof)
                switch (operation.phase, reference) {
                case (.awaitingConfirmation(let actual), .confirmation(let expected)) where actual == expected: break
                case (.awaitingAction(let actual), .action(let expected)) where actual == expected: break
                default: throw NativeSettingsWireError.invalidResponse
                }
            case (.admit(let invocation), .admitted(let operation, let target)):
                try Self.validate(operation, target: target, operationID: invocation.operationID, proof: proof)
                guard case .inFlight(let id) = operation.phase, id == invocation.nativeRequestID,
                      case .native(let request) = target else { throw NativeSettingsWireError.invalidResponse }
                switch request.action {
                case .notificationAuthorization, .openSection: break
                case .credentialEntry: throw NativeSettingsWireError.invalidResponse
                }
            case (.admit(let invocation), .inspected(let operation)), (.confirmPatch(let invocation), .inspected(let operation)),
                 (.confirmDevice(let invocation), .deviceConfirmed(.existing(let operation))),
                 (.confirmDevice(let invocation), .deviceConfirmed(.revoked(let operation))),
                 (.submitCredential(let invocation, _), .inspected(let operation)),
                 (.finalize(let invocation, _), .inspected(let operation)):
                try Self.validate(operation, invocation: invocation, proof: proof)
            case (.confirmDevice(let invocation), .deviceConfirmed(.pairing(let creation))):
                try Self.validate(creation.operation, invocation: invocation, proof: proof)
            case (.pairing(let id), .pairing(let creation)), (.cancelPairing(let id), .pairing(let creation)):
                guard creation.operation.binding.requestID == id else { throw NativeSettingsWireError.invalidResponse }
                try Self.validate(creation.operation, proof: proof)
            case (.inspect(let id), .inspected(let operation)), (.reconcile(let id), .inspected(let operation)),
                 (.cancel(let id), .inspected(let operation)):
                guard operation.binding.requestID == id else { throw NativeSettingsWireError.invalidResponse }
                try Self.validate(operation, proof: proof)
            case (.close, .closed(let id)):
                guard id == proof?.sessionID else { throw NativeSettingsWireError.invalidResponse }
            default: throw NativeSettingsWireError.invalidResponse
            }
        }

        private static func validate(_ result: SettingsOperationResult, requestID: UUID, scope: SettingsScope) throws {
            try result.validate()
            guard result.requestID == requestID, result.scope == scope else { throw NativeSettingsWireError.invalidResponse }
        }

        private static func validate(_ operation: SettingsOperationInspection, invocation: NativeActionInvocation, proof: Proof?) throws {
            guard operation.binding.requestID == invocation.operationID else { throw NativeSettingsWireError.invalidResponse }
            if case .inFlight(let nativeRequestID) = operation.phase {
                guard nativeRequestID == invocation.nativeRequestID else { throw NativeSettingsWireError.invalidResponse }
            }
            try validate(operation, proof: proof)
        }

        private static func validate(_ operation: SettingsOperationInspection, target: SettingsActionTarget, operationID: UUID, proof: Proof?) throws {
            try validate(operation, proof: proof)
            try target.validate()
            guard operation.binding.requestID == operationID, target.requestID == operationID,
                  target.scope == operation.binding.scope, target.expectedRevision == operation.binding.expectedRevision else {
                throw NativeSettingsWireError.invalidResponse
            }
        }

        private static func validate(_ operation: SettingsOperationInspection, proof: Proof?) throws {
            try operation.validate()
            guard let proof, operation.binding.clientSessionID == proof.sessionID else { throw NativeSettingsWireError.invalidResponse }
            switch operation.origin {
            case .nativeUI(let id): guard id == proof.sessionID else { throw NativeSettingsWireError.invalidResponse }
            case .settingsAgent(_, let id): guard id == proof.sessionID else { throw NativeSettingsWireError.invalidResponse }
            case .localOwner: throw NativeSettingsWireError.invalidResponse
            }
        }
    }

    static func encode(_ operation: Operation, requestID: UUID, proof: Proof?) throws -> Data {
        do {
            try SettingsWire.identifier(requestID)
            try operation.validate()
            if case .open = operation {} else if proof == nil { throw NativeSettingsWireError.invalidRequest }
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
            let data = try encoder.encode(Request(requestID: requestID, operation: operation, proof: proof))
            guard !data.isEmpty, data.count <= 32 * 1024 else { throw NativeSettingsWireError.invalidRequest }
            return data
        } catch { throw NativeSettingsWireError.invalidRequest }
    }

    static func decode(_ data: Data, for request: Operation, proof: Proof?) throws -> Response {
        do {
            guard !data.isEmpty, data.count <= 2 * 1024 * 1024 else { throw NativeSettingsWireError.invalidResponse }
            let response = try JSONDecoder().decode(Response.self, from: data)
            try response.validate(for: request, proof: proof)
            return response
        } catch { throw NativeSettingsWireError.invalidResponse }
    }

    private struct Request: Encodable {
        let requestID: UUID
        let operation: Operation
        let proof: Proof?

        func encode(to encoder: any Encoder) throws {
            var root = encoder.container(keyedBy: SettingsWireKey.self)
            try root.encode("native_settings", forKey: SettingsWireKey("kind"))
            var input = root.nestedContainer(keyedBy: SettingsWireKey.self, forKey: SettingsWireKey("input"))
            try input.encode(requestID, forKey: SettingsWireKey("request_id"))
            var fields = input.nestedContainer(keyedBy: SettingsWireKey.self, forKey: SettingsWireKey("operation"))
            try fields.encode(operation.kind, forKey: SettingsWireKey("kind"))
            if case .open = operation {} else { try fields.encode(proof, forKey: SettingsWireKey("proof")) }
            switch operation {
            case .get(let scope): try fields.encode(scope, forKey: SettingsWireKey("scope"))
            case .update(let patch): try fields.encode(patch, forKey: SettingsWireKey("patch"))
            case .requestAction(let request): try fields.encode(request, forKey: SettingsWireKey("request"))
            case .pairDevice(let request), .revokeDevice(let request): try fields.encode(request, forKey: SettingsWireKey("request"))
            case .confirmPatch(let invocation), .confirmDevice(let invocation), .admit(let invocation):
                try fields.encode(invocation, forKey: SettingsWireKey("invocation"))
            case .submitCredential(let invocation, let secret):
                try fields.encode(invocation, forKey: SettingsWireKey("invocation"))
                try fields.encode(secret, forKey: SettingsWireKey("secret"))
            case .finalize(let invocation, let observation):
                try fields.encode(invocation, forKey: SettingsWireKey("invocation"))
                try fields.encode(observation, forKey: SettingsWireKey("observation"))
            case .changes(let after, let limit):
                try fields.encode(after, forKey: SettingsWireKey("after_sequence"))
                try fields.encode(limit, forKey: SettingsWireKey("limit"))
            case .devices(let scope, let after, let limit):
                try fields.encode(scope, forKey: SettingsWireKey("scope"))
                try fields.encode(after, forKey: SettingsWireKey("after"))
                try fields.encode(limit, forKey: SettingsWireKey("limit"))
            case .details(let id, let reference):
                try fields.encode(id, forKey: SettingsWireKey("operation_id"))
                try fields.encode(reference, forKey: SettingsWireKey("reference"))
            case .reconcile(let id), .pairing(let id), .cancelPairing(let id), .inspect(let id), .cancel(let id):
                try fields.encode(id, forKey: SettingsWireKey("operation_id"))
            case .open, .describe, .close: break
            }
        }
    }
}

import SwiftUI

struct SettingsView: View {
    let model: AppModel
    let pair: () -> Void
    @State private var origin = ""
    @State private var disabling = false
    @State private var inspectionRequest = ""
    @State private var inspectionOperation = ""

    var body: some View {
        Form {
            Section("Relay") {
                TextField("wss://relay.example", text: $origin).textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                Button("Save relay") { Task { await model.saveRelay(origin) } }.disabled(model.busy || model.connectionEnabled || origin.isEmpty)
                Text("The relay receives encrypted envelopes and public device metadata. Use your explicitly trusted relay. Changing this endpoint discloses routing metadata to its operator.").font(.footnote)
                Text(model.state?.connection ?? "Disconnected")
                if model.connectionEnabled {
                    Button("Disconnect") { Task { await model.disconnect() } }
                } else {
                    Button("Connect while foreground") { Task { await model.connect() } }
                        .disabled(model.busy || model.state?.snapshot.host == nil || model.state?.snapshot.revoked != false)
                }
                Text("Synchronization reconnects while this app is active. iOS suspension and termination stop its connection; remote push delivery is unavailable.").font(.footnote)
            }
            Section("Device") {
                if let state = model.state {
                    Text(state.identity.deviceID.uuidString.lowercased()).font(.caption.monospaced()).textSelection(.enabled)
                    Text(state.identity.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                    PublicKeysView(keys: state.identity)
                    if let host = state.snapshot.host {
                        Text("Pinned host").font(.headline)
                        Text(host.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                    }
                }
                Button("Pair with host", action: pair).disabled(model.busy || model.state?.snapshot.revoked == true)
                Text("The host chooses access separately; its signed acceptance does not contain conversation or permission grants. Add only conversation IDs that its owner authorized.").font(.footnote)
            }
            Section("Connected Host access") {
                Button("Check Host access") { model.checkHostSettingsAccess() }
                    .disabled(!model.canCheckHostSettingsAccess)
                if model.hostSettingsAccessBusy || model.hostSettingsAccessStatus == .preparing
                    || model.hostSettingsAccessStatus == .awaitingReply {
                    Button("Cancel access check", role: .cancel) { model.cancelHostSettingsAccess() }
                }
                Text(accessStatus).font(.footnote).foregroundStyle(.secondary)
                if let issue = model.hostSettingsAccessIssue { Text(issue).foregroundStyle(.red) }
                if let access = model.hostSettingsAccess {
                    Text(model.hostSettingsAccessIsCurrent ? "Current connection observation" : "Stale observation")
                        .font(.caption).foregroundStyle(.secondary)
                    switch access.display.status {
                    case .grantUnavailable:
                        Text("The Host reported that Settings access is unavailable for this device.")
                    case .available(let summary):
                        LabeledContent("Readable settings", value: settingNames(summary.readableKeys))
                        LabeledContent("Writable settings", value: settingNames(summary.writableKeys))
                        switch summary.sessionConsent {
                        case .none:
                            Text("No configured Settings assistant recipient was reported.")
                        case .configuredProvider(let provider, let model, let recipient):
                            LabeledContent("Configured provider", value: provider == .anthropic ? "Anthropic" : "OpenAI")
                            LabeledContent("Configured model") { Text(verbatim: model) }
                            LabeledContent("Configured recipient") { Text(verbatim: recipient) }
                            if let revision = summary.configuredSettingsRevision {
                                LabeledContent("Configuration revision", value: String(revision))
                            }
                        }
                    }
                    LabeledContent("Observed") { Text(Date(timeIntervalSince1970: Double(access.observedAt) / 1000), style: .time) }
                }
                Button("Request Settings binding") { model.bindHostSettings() }
                    .disabled(!model.canBindHostSettings)
                if let binding = model.hostSettingsBinding {
                    Text(bindingStatus(binding.stage)).font(.footnote).foregroundStyle(.secondary)
                    if let observed = binding.observedAt, let expires = binding.expiresAt {
                        Text(model.hostSettingsBindingIsCurrent ? "Current connection binding observation" : "Stale binding observation")
                            .font(.caption).foregroundStyle(.secondary)
                        LabeledContent("Binding observed") { Text(Date(timeIntervalSince1970: Double(observed) / 1000), style: .time) }
                        LabeledContent("Binding expires") { Text(Date(timeIntervalSince1970: Double(expires) / 1000), style: .time) }
                    }
                    if binding.stage.blocksNewRequest {
                        Button("Cancel binding request", role: .cancel) { model.cancelHostSettingsBinding() }
                    }
                    Text("This original binding cannot be retried here. Cancelling or disconnecting does not confirm that the Host closed it.").font(.footnote)
                }
                Text("Binding requests do not start an assistant or change Host settings.").font(.footnote)
                Text("This check reports access metadata only. It does not change Host settings or contact an AI provider. A stale observation grants no access.").font(.footnote)
            }
            Section("Connected Host values") {
                Button("Read Host values") { model.readHostSettingsSnapshot() }
                    .disabled(!model.canReadHostSettingsSnapshot)
                if model.hostSettingsSnapshotStatus?.blocksNewRequest == true {
                    Button("Cancel value read", role: .cancel) { model.cancelHostSettingsSnapshot() }
                }
                Text(snapshotStatus).font(.footnote).foregroundStyle(.secondary)
                if let access = model.hostSettingsAccess, case .available(let summary) = access.display.status,
                   summary.readableKeys.isEmpty {
                    Text("No Host values were reported as readable for this device.").font(.footnote)
                }
                if let observation = model.hostSettingsSnapshot {
                    Text(model.hostSettingsSnapshotIsCurrent ? "Current connection observation" : "Stale read-only observation")
                        .font(.caption).foregroundStyle(.secondary)
                    switch observation.display {
                    case .denied(let reason):
                        Text(reason == .grantUnavailable
                             ? "The Host reported that Settings access is unavailable."
                             : "The Host reported that the requested values are outside this device’s scope.")
                    case .values(let snapshot):
                        LabeledContent("Observed revision", value: String(snapshot.revision))
                        LabeledContent("Observed") { Text(Date(timeIntervalSince1970: Double(snapshot.observedAt) / 1000), style: .time) }
                        ForEach(snapshot.entries, id: \.key) { entry in
                            VStack(alignment: .leading, spacing: 6) {
                                Text(settingNames([entry.key])).font(.headline)
                                LabeledContent("Default") { Text(verbatim: settingValue(entry.defaultValue)) }
                                LabeledContent("Desired") { Text(verbatim: entry.desired.map(settingValue) ?? "No desired value") }
                                LabeledContent("Effective") { Text(verbatim: entry.effective.map(settingValue) ?? "No effective value") }
                                LabeledContent("State", value: settingState(entry.state))
                                if let reason = entry.unavailableReason {
                                    Text(unavailableReason(reason)).font(.footnote).foregroundStyle(.secondary)
                                }
                            }
                        }
                    }
                }
                Text("Reads show only the granted Host values. They do not change settings or contact an AI provider. Offline observations are stale.").font(.footnote)
            }
            Section("Inspect an existing Host request") {
                TextField("Original request ID", text: Binding(get: { inspectionRequest }, set: { inspectionRequest = boundedIdentifierDraft($0) }))
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                TextField("Send operation ID (optional)", text: Binding(get: { inspectionOperation }, set: { inspectionOperation = boundedIdentifierDraft($0) }))
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                Button("Inspect request") {
                    guard let target = inspectionTarget else { return }
                    model.inspectHostSettingsOriginal(requestID: target.requestID, operationID: target.operationID)
                }
                .disabled(!model.canInspectHostSettingsOriginal || inspectionTarget == nil)
                if model.hostSettingsInspectionStatus?.blocksNewRequest == true {
                    Button("Cancel inspection", role: .cancel) { model.cancelHostSettingsInspection() }
                }
                Text(inspectionStatus).font(.footnote).foregroundStyle(.secondary)
                if let observation = model.hostSettingsInspection {
                    Text(model.hostSettingsInspectionIsCurrent ? "Current connection observation" : "Stale read-only observation")
                        .font(.caption).foregroundStyle(.secondary)
                    switch observation.display {
                    case .denied(let reason):
                        Text(reason == .grantUnavailable
                             ? "The Host reported that Settings access is unavailable."
                             : "The Host reported that this inspection is outside this device’s scope.")
                    case .observation(let requestID, let operationID, let observedAt, let lookup):
                        LabeledContent("Inspected request") { Text(verbatim: requestID.uuidString.lowercased()) }
                        if let operationID {
                            LabeledContent("Selected operation") { Text(verbatim: operationID.uuidString.lowercased()) }
                        }
                        LabeledContent("Observed") { Text(Date(timeIntervalSince1970: Double(observedAt) / 1000), style: .time) }
                        switch lookup {
                        case .notRecorded:
                            Text("The Host has no recorded Original for this request.")
                        case .recorded(let original):
                            originalInspection(original)
                        }
                    }
                }
                Text("Enter an existing request ID. Inspection does not retry the request or change Host settings. A missing record or completed request does not by itself confirm that a setting was saved.").font(.footnote)
            }
            Section("This iPhone") {
                NavigationLink("Appearance and notifications", value: ClientSettingsDestination.thisDevice)
                if let issue = model.localSettingsIssue { Text(issue).foregroundStyle(.red) }
                Text("Local preferences remain available while the Host is offline. Notifications use your saved choices and system permission while Everything is active and synchronization is enabled. Your first sync stays quiet.").font(.footnote)
                if let status = model.notificationDeliveryStatus { Text(status).font(.footnote).foregroundStyle(.secondary) }
            }
            Section("Local privacy") {
                Text("Identity secrets use unlocked-only, non-synchronizing ThisDeviceOnly Keychain storage. Local history uses complete file protection. These Curve25519 keys are not Secure Enclave keys.").font(.footnote)
                Button("Permanently disable this device", role: .destructive) { setDisabling(true) }.disabled(model.state?.snapshot.revoked != false)
                Text("Disabling is sticky and stops this client. The host owner must separately revoke this device and its relay enrollment. Previously received local history remains here.").font(.footnote)
            }
        }
        .navigationTitle("Settings")
        .onAppear { model.hostSettingsAccessSurfaceChanged(visible: true) }
        .onDisappear { model.hostSettingsAccessSurfaceChanged(visible: false) }
        .task { origin = model.state?.snapshot.origin ?? "" }
        .confirmationDialog("Permanently disable this device?", isPresented: Binding(get: { disabling }, set: { setDisabling($0) })) {
            Button("Disable device", role: .destructive) { Task { await model.disable() } }
            Button("Cancel", role: .cancel) { setDisabling(false) }
        }
    }

    private func boundedIdentifierDraft(_ value: String) -> String {
        var bounded = String(value.prefix(36))
        while bounded.utf8.count > 36 { bounded.removeLast() }
        return bounded
    }

    private func canonicalIdentifier(_ value: String) -> UUID? {
        guard value.utf8.count == 36, let id = UUID(uuidString: value), id != WireID.zero,
              id.uuidString.lowercased() == value else { return nil }
        return id
    }

    private var inspectionTarget: (requestID: UUID, operationID: UUID?)? {
        guard let requestID = canonicalIdentifier(inspectionRequest) else { return nil }
        if inspectionOperation.isEmpty { return (requestID, nil) }
        guard let operationID = canonicalIdentifier(inspectionOperation) else { return nil }
        return (requestID, operationID)
    }

    private var inspectionStatus: String {
        switch model.hostSettingsInspectionStatus {
        case .preparing: "Preparing the original inspection."
        case .awaitingReply: "Waiting for the Host inspection."
        case .settling: "Finishing the original inspection."
        case .received: "The original Host inspection was observed."
        case .cancelled: "The inspection was cancelled."
        case .expired: "The inspection expired without a current observation."
        case .interrupted: "The inspection was interrupted; the request outcome is unknown."
        case nil: "Request a current Settings binding, then explicitly inspect a known request."
        }
    }

    @ViewBuilder
    private func originalInspection(_ original: RemoteSettingsOriginalView) -> some View {
        LabeledContent("Request kind", value: originalKind(original.originalKind))
        LabeledContent("Original result", value: originalTerminal(original.terminal))
        switch original.link {
        case .unlinked: Text("The Original is not linked to another result.")
        case .chat(let chatID):
            LabeledContent("Settings chat") { Text(verbatim: chatID.uuidString.lowercased()) }
        case .turn(let chatID, let turnID):
            LabeledContent("Settings chat") { Text(verbatim: chatID.uuidString.lowercased()) }
            LabeledContent("Settings turn") { Text(verbatim: turnID.uuidString.lowercased()) }
        case .operation(let operationID):
            LabeledContent("Linked operation") { Text(verbatim: operationID.uuidString.lowercased()) }
        }
        ForEach(original.mutationOperationIDs, id: \.self) { id in
            LabeledContent("Declared operation") { Text(verbatim: id.uuidString.lowercased()) }
        }
        switch original.service {
        case .notRequested: Text("No operation receipt was requested.")
        case .notRecorded: Text("The selected operation has no recorded receipt.")
        case .recorded(let receipt):
            LabeledContent("Receipt operation") { Text(verbatim: receipt.operationID.uuidString.lowercased()) }
            if let expected = receipt.expectedRevision { LabeledContent("Expected revision", value: String(expected)) }
            LabeledContent("Receipt updated") { Text(Date(timeIntervalSince1970: Double(receipt.updatedAt) / 1000), style: .time) }
            LabeledContent("Operation result", value: inspectionPhase(receipt.phase))
            if let write = receipt.write {
                LabeledContent("File expected revision", value: String(write.expectedRevision))
                LabeledContent("File revision", value: String(write.revision))
                LabeledContent("Changed settings", value: settingNames(write.changedKeys))
                LabeledContent("Restart needed", value: write.restartRequired ? "Yes" : "No")
                LabeledContent("File resolution", value: write.resolution.map { $0 == .applied ? "Applied" : "Not applied" } ?? "Unknown")
            }
            LabeledContent("Application", value: receipt.application.map(inspectionApplication) ?? "Not reported")
        }
    }

    private func originalKind(_ kind: RemoteSettingsOriginalKind) -> String {
        switch kind {
        case .chatOpen: "Settings chat open"
        case .send: "Settings turn"
        case .update: "Settings update"
        }
    }

    private func originalTerminal(_ terminal: RemoteSettingsOriginalTerminal?) -> String {
        switch terminal {
        case .completed: "Completed"
        case .denied: "Denied"
        case .suppressed: "Suppressed"
        case .retiredLostRAM: "Retired; result unavailable"
        case .unknown: "Unknown"
        case nil: "No terminal result reported"
        }
    }

    private func inspectionPhase(_ phase: RemoteSettingsOperationPhase) -> String {
        switch phase {
        case .applying(let expected, let revision): return "Applying revision \(revision), expected \(expected)"
        case .finished(let state):
            switch state {
            case .applied(let revision): return "Saved at revision \(revision)"
            case .pendingApplication(let revision): return "Saved at revision \(revision); application pending"
            case .restartRequired(let revision): return "Saved at revision \(revision); restart required"
            case .conflict(let observed): return "Conflict at revision \(observed)"
            case .hostActionRequired: return "Action required on the Host"
            case .unavailable(let reason): return unavailableReason(reason)
            case .failed(let reason): return inspectionFailure(reason)
            case .unknown: return "Unknown"
            case .cancelled: return "Cancelled"
            }
        }
    }

    private func inspectionApplication(_ application: SettingsApplicationState) -> String {
        switch application {
        case .applied: "Applied"
        case .restartRequired: "Restart required"
        case .unavailable(let reason): unavailableReason(reason)
        case .failed(let reason): inspectionFailure(reason)
        }
    }

    private func inspectionFailure(_ reason: SettingsFailureReason) -> String {
        switch reason {
        case .invalidRequest: "The request was invalid."
        case .storage: "The Host reported a storage failure."
        case .applyFailed: "The Host could not apply the setting."
        case .nativeActionFailed: "The native action failed."
        }
    }

    private var accessStatus: String {
        switch model.hostSettingsAccessStatus {
        case .preparing: "Preparing the original access check."
        case .awaitingReply: "Waiting for the Host access response."
        case .settling: "Finishing the original access check before another can start."
        case .received: "The Host access response was observed."
        case .cancelled: "The access check was cancelled."
        case .expired: "The access check expired without a current observation."
        case .interrupted: "The access check was interrupted; current Host access is unknown."
        case nil: "Connect to the Host, then explicitly check its access for this device."
        }
    }

    private var snapshotStatus: String {
        switch model.hostSettingsSnapshotStatus {
        case .preparing: "Preparing the original value read."
        case .awaitingReply: "Waiting for the Host values."
        case .settling: "Finishing the original value read."
        case .received: "The original Host response was observed."
        case .cancelled: "The value read was cancelled."
        case .expired: "The value read expired without a current observation."
        case .interrupted: "The value read was interrupted; current Host values are unknown."
        case nil: "Check Host access and request a binding, then explicitly read its granted values."
        }
    }

    private func settingValue(_ value: RemoteSettingsValue) -> String {
        switch value {
        case .model(let selection):
            if let selection {
                return "\(selection.provider == .anthropic ? "Anthropic" : "OpenAI"): \(selection.model)"
            }
            return "Not selected"
        case .toolSelection(let selection):
            switch selection {
            case .local: return "Local"
            case .systemOne(let model): return "System One: \(model)"
            }
        case .boolean(let value): return value ? "Enabled" : "Disabled"
        case .dimensions(let value): return value.map { String($0) } ?? "Backend default"
        case .embeddingModel(let model): return model ?? "Unavailable"
        }
    }

    private func settingState(_ state: SettingState) -> String {
        switch state {
        case .effective: "Effective"
        case .pendingApplication: "Pending application"
        case .restartRequired: "Restart required"
        case .unavailable: "Unavailable"
        }
    }

    private func unavailableReason(_ reason: SettingsUnavailableReason) -> String {
        switch reason {
        case .hostOffline: "The Host is offline."
        case .providerUnavailable: "The provider is unavailable."
        case .embeddingMetadataUnavailable: "Embedding status is unavailable."
        case .embeddingBoundaryUnavailable: "Local embeddings are unavailable."
        case .nativeActionUnavailable: "This action requires the Host."
        case .permissionDenied: "Permission was denied."
        case .expired: "The observation expired."
        }
    }

    private func bindingStatus(_ stage: ReplicaSettingsBindingStage) -> String {
        switch stage {
        case .preparing: "Preparing the original binding request."
        case .awaitingReply: "Waiting for the Host binding response."
        case .settling: "Finishing the original binding request."
        case .observed: "The Host binding response was observed."
        case .retained: "The original binding is retained; its current Host outcome has not been verified."
        }
    }

    private func settingNames(_ keys: [SettingKey]) -> String {
        guard !keys.isEmpty else { return "None" }
        return keys.map { key -> String in
            switch key {
            case .modelSelection: "Model selection"
            case .toolSelection: "Tool selection"
            case .embeddingsEnabled: "Local embeddings"
            case .embeddingsDimensions: "Embedding dimensions"
            case .embeddingsModel: "Embedding model"
            case .notificationsCompletion: "Completion notifications"
            case .notificationsAttention: "Attention notifications"
            case .notificationAuthorization: "Notification authorization"
            }
        }.joined(separator: ", ")
    }

    private func setDisabling(_ presented: Bool) {
        if presented { model.localCoverChanged(.deviceDisable, presented: true) }
        disabling = presented
        if !presented { model.localCoverChanged(.deviceDisable, presented: false) }
    }
}

private struct PublicKeysView: View {
    let keys: DevicePublicKeys

    private var text: String {
        do {
            guard let value = String(data: try keys.wire.encoded(maximum: 4_096), encoding: .utf8) else { return "Public identity unavailable" }
            return value
        } catch { return "Public identity unavailable" }
    }

    var body: some View {
        DisclosureGroup("Public relay enrollment keys") {
            Text(text).font(.caption.monospaced()).textSelection(.enabled)
            Text("Only public keys are shown. The relay operator must enroll these exact keys before this device can authenticate.").font(.footnote)
        }
    }
}

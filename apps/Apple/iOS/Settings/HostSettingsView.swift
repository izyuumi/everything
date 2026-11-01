import SwiftUI

func hostSettingName(_ key: SettingKey) -> String {
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
}

private func hostSettingValue(_ value: SettingValue?) -> String {
    switch value {
    case nil: "Not set"
    case .model(let selection):
        selection.map { "\($0.provider == .anthropic ? "Anthropic" : "OpenAI") \($0.model)" } ?? "Not configured"
    case .toolSelection(.local): "Local"
    case .toolSelection(.systemOne(let endpoint, let model, let provider)): "System One · " + SystemOneChoice.title(provider: provider) + " · " + model + " · " + endpoint
    case .boolean(let value): value ? "On" : "Off"
    case .dimensions(let value): value.map(String.init) ?? "Backend default"
    case .embeddingModel(let model): model ?? "Not set"
    case .notificationAuthorization(let state): state.rawValue
    }
}

private func hostSettingState(_ entry: SettingEntry) -> String {
    switch entry.state {
    case .effective: "Effective"
    case .pendingApplication: "Saved; waiting to apply"
    case .restartRequired: "Saved; Host restart required"
    case .unavailable: "Unavailable" + (entry.unavailableReason.map { " (\($0.rawValue))" } ?? "")
    }
}

private func hostActionName(_ action: RemoteSettingsHostAction) -> String {
    switch action {
    case .credentialEntry: "Enter or replace a credential"
    case .pairingInvitation: "Create a pairing invitation"
    case .pairingApproval: "Approve a pairing request"
    case .deviceRevocation: "Revoke a device"
    case .grantChange: "Change this iPhone's Settings access"
    case .executionPolicy: "Change execution policy"
    case .recipientConsent: "Change the assistant recipient consent"
    case .lifecycle: "Start, stop or restart the Host"
    }
}

private func hostOperationText(_ operation: RemoteSettingsOperation) -> String {
    guard case .finished(let result) = operation.phase else {
        return "The Host recorded this change and is still applying it. Refresh to reconcile."
    }
    switch result.state {
    case .applied(let revision): return "Saved and applied on the Host (revision \(revision))."
    case .pendingApplication(let revision): return "Saved on the Host (revision \(revision)); it applies when the Host can."
    case .restartRequired(let revision): return "Saved on the Host (revision \(revision)); a Host restart is required."
    case .completed: return "Completed on the Host."
    case .conflict(let observed): return "Not saved: Host settings were at revision \(observed)."
    case .unavailable(let reason): return "Not saved: unavailable (\(reason.rawValue))."
    case .failed(let reason): return "Not saved: failed (\(reason.rawValue))."
    case .cancelled: return "Not saved: cancelled."
    case .unknown: return "The Host recorded an unknown outcome. Refresh to reconcile."
    case .hostActionRequired, .requiresConfirmation, .requiresUserAction: return "This change requires action on the Host."
    }
}

struct HostSettingsSections: View {
    let model: AppModel
    @State private var draftEnabled: Bool?
    @State private var draftDimensions = ""
    @State private var draftDefaultDimensions: Bool?

    var body: some View {
        Section("Connected Host settings") {
            if let values = model.hostValues {
                Text(model.hostValuesCurrent ? "Current values from this connection" : "Stale cached values (read-only)")
                    .font(.caption).foregroundStyle(model.hostValuesCurrent ? Color.secondary : Color.orange)
                LabeledContent("Host revision", value: String(values.snapshot.revision))
                LabeledContent("Read") { Text(Date(timeIntervalSince1970: Double(values.observedAt) / 1000), style: .time) }
                ForEach(values.snapshot.entries, id: \.key) { entry in
                    VStack(alignment: .leading, spacing: 2) {
                        LabeledContent(hostSettingName(entry.key), value: hostSettingValue(entry.desired ?? entry.effective))
                        Text(hostSettingState(entry) + " · " + editability(entry.key)).font(.caption).foregroundStyle(.secondary)
                    }
                }
            } else {
                Text("No Host values have been read on this iPhone.").foregroundStyle(.secondary)
            }
            if let summary = model.hostSettingsSummary, model.hostValuesCurrent, let values = model.hostValues {
                if summary.writableKeys.contains(.embeddingsEnabled) {
                    Toggle("Local embeddings", isOn: Binding(
                        get: { draftEnabled ?? currentEnabled(values) },
                        set: { draftEnabled = $0 }
                    )).disabled(!model.canSaveHostSettings)
                }
                if summary.writableKeys.contains(.embeddingsDimensions) {
                    Toggle("Backend default dimensions", isOn: Binding(
                        get: { draftDefaultDimensions ?? (currentDimensions(values) == nil) },
                        set: { draftDefaultDimensions = $0 }
                    )).disabled(!model.canSaveHostSettings)
                    if !(draftDefaultDimensions ?? (currentDimensions(values) == nil)) {
                        TextField("Dimensions (1–65536)", text: $draftDimensions).keyboardType(.numberPad)
                            .disabled(!model.canSaveHostSettings)
                    }
                }
                if !summary.writableKeys.isEmpty {
                    Button("Save to Host") { save(values) }.disabled(!model.canSaveHostSettings || changes(values) == nil)
                }
            }
            if let mutation = model.hostMutation { mutationStatus(mutation) }
            Text("Only keys the Host owner granted to this iPhone are shown. Saving requires a current connection and the revision shown; a relay acknowledgment is never reported as saved. Protected and credential settings stay on the Host.")
                .font(.footnote)
        }
        Section("Requires the Host") {
            ForEach(RemoteSettingsHostAction.allCases, id: \.self) { action in
                Button(hostActionName(action)) { model.confirmHostAction(action) }
                    .disabled(!model.canRefreshHostSettings)
            }
            if let action = model.hostActionResult {
                Text("The Host confirmed: “\(hostActionName(action))” must be done on the Host itself. Nothing was changed and no secret was sent.")
                    .font(.footnote)
            }
            Text("Credential entry, pairing, revocation, access grants, execution policy, consent and lifecycle are local-owner actions. This iPhone cannot perform them over the relay.")
                .font(.footnote)
        }
        Section("Ask Settings") {
            NavigationLink("Open Settings chat", value: ClientSettingsDestination.hostAssistant)
                .disabled(!model.canAskHostSettings && model.assistant.chat == nil)
            Text(assistantNote).font(.footnote)
        }
    }

    private var assistantNote: String {
        guard let summary = model.hostSettingsSummary else {
            return "Refresh Host settings first. The assistant runs on the Host, not on this iPhone."
        }
        switch summary.sessionConsent {
        case .none:
            return "The Host owner has not consented to a Settings assistant for this iPhone. No provider is contacted."
        case .configuredProvider(let provider, let model, let recipient):
            return "Messages go to the Host's configured \(provider == .anthropic ? "Anthropic" : "OpenAI") model \(model) at \(recipient). The assistant can only read and change keys granted to this iPhone."
        }
    }

    private func editability(_ key: SettingKey) -> String {
        if model.hostSettingsSummary?.writableKeys.contains(key) == true { return "Editable here" }
        switch key.agentMutability {
        case .requiresNativeConfirmation: return "Change on the Host (protected)"
        case .readOnly: return "Read-only"
        default: return "Not granted for editing"
        }
    }

    @ViewBuilder
    private func mutationStatus(_ mutation: HostSettingsMutation) -> some View {
        switch mutation.state {
        case .sending:
            Text("Sending change… Not saved until the Host replies.").font(.footnote)
        case .uncertain:
            Text("Change outcome unknown. Its original identity is kept so a refresh replays it exactly instead of applying it twice.").font(.footnote).foregroundStyle(.orange)
            HStack {
                Button("Reconcile now") { model.reconcileHostSettingsMutation() }.disabled(!model.canRefreshHostSettings)
                Button("Stop tracking", role: .destructive) { model.forgetHostSettingsMutation() }.disabled(model.hostSettingsBusy)
            }
        case .recorded(let operation):
            Text(hostOperationText(operation)).font(.footnote)
            if mutation.isOpen {
                Button("Reconcile now") { model.reconcileHostSettingsMutation() }.disabled(!model.canRefreshHostSettings)
            }
        case .rejected(let reason):
            Text("Not saved: the Host refused the change (\(reason.rawValue)).").font(.footnote).foregroundStyle(.red)
        }
    }

    private func currentEnabled(_ values: HostSettingsValues) -> Bool {
        guard let entry = values.snapshot.entries.first(where: { $0.key == .embeddingsEnabled }),
              case .boolean(let value) = entry.desired ?? entry.effective else { return false }
        return value
    }

    private func currentDimensions(_ values: HostSettingsValues) -> UInt32? {
        guard let entry = values.snapshot.entries.first(where: { $0.key == .embeddingsDimensions }),
              case .dimensions(let value) = entry.desired ?? entry.effective else { return nil }
        return value
    }

    private func changes(_ values: HostSettingsValues) -> [SettingChange]? {
        guard let summary = model.hostSettingsSummary else { return nil }
        var result: [SettingChange] = []
        if summary.writableKeys.contains(.embeddingsEnabled), let draftEnabled, draftEnabled != currentEnabled(values) {
            result.append(SettingChange(key: .embeddingsEnabled, value: .boolean(value: draftEnabled)))
        }
        if summary.writableKeys.contains(.embeddingsDimensions) {
            let useDefault = draftDefaultDimensions ?? (currentDimensions(values) == nil)
            let requested: UInt32?
            if useDefault {
                requested = nil
            } else if let parsed = UInt32(draftDimensions.trimmingCharacters(in: .whitespaces)), (1...65_536).contains(parsed) {
                requested = parsed
            } else if draftDimensions.isEmpty, let current = currentDimensions(values) {
                requested = current
            } else {
                return nil
            }
            if requested != currentDimensions(values) {
                result.append(SettingChange(key: .embeddingsDimensions, value: .dimensions(value: requested)))
            }
        }
        return result.isEmpty ? nil : result
    }

    private func save(_ values: HostSettingsValues) {
        guard let changes = changes(values) else { return }
        model.saveHostSettings(changes)
        draftEnabled = nil
        draftDimensions = ""
        draftDefaultDimensions = nil
    }
}

struct HostSettingsAssistantView: View {
    let model: AppModel
    @State private var message = ""

    var body: some View {
        Form {
            Section("Recipient") {
                if let chat = model.assistant.chat {
                    LabeledContent("Provider", value: chat.provider == .anthropic ? "Anthropic" : "OpenAI")
                    LabeledContent("Model") { Text(verbatim: chat.model) }
                    LabeledContent("Recipient") { Text(verbatim: chat.recipient) }
                    Text("This chat runs on the Host with only the Settings tools and only the keys granted to this iPhone. It has no access to conversations or the wiki.")
                        .font(.footnote)
                } else if model.canAskHostSettings {
                    Button("Start Settings chat") { model.openHostSettingsAssistant() }.disabled(model.hostSettingsBusy)
                    Text("Starting a chat sends no message. The Host checks the owner's consent for its configured model before anything is sent.")
                        .font(.footnote)
                } else {
                    Text("The Settings assistant is unavailable: refresh Host settings and confirm the Host owner consented to a configured model for this iPhone. No other provider is used.")
                        .font(.footnote)
                }
            }
            if model.assistant.chat != nil {
                Section("Conversation") {
                    ForEach(model.assistant.turns) { turn in
                        Text(verbatim: turn.text).frame(maxWidth: .infinity, alignment: .trailing)
                            .foregroundStyle(turn.accepted ? Color.primary : Color.secondary)
                    }
                    if !model.assistant.text.isEmpty {
                        Text(verbatim: model.assistant.assistantText).textSelection(.enabled)
                    }
                    if model.assistant.isRunning, model.assistant.ended == nil { ProgressView("The Host is working…") }
                    if let ended = model.assistant.ended { Text(ended).font(.footnote).foregroundStyle(.secondary) }
                }
                if !cards.isEmpty {
                    Section("Changes made by the assistant") {
                        ForEach(cards, id: \.sequence) { hint in
                            VStack(alignment: .leading) {
                                Text(hint.changedKeys.map(hostSettingName).joined(separator: ", ")).font(.headline)
                                Text("Host state: \(hint.state.rawValue)" + (hint.revision.map { " · revision \($0)" } ?? "")).font(.caption)
                            }
                        }
                        Text("These cards are the Host's authenticated change records for granted keys since your message, not chat text. They may include changes made on the Host at the same time.").font(.footnote)
                    }
                } else if model.assistant.status?.operationIDs.isEmpty == false {
                    Section("Changes made by the assistant") {
                        Text("The Host reported \(model.assistant.status?.operationIDs.count ?? 0) Settings change(s). Refresh Host settings to read their current state.")
                            .font(.footnote)
                    }
                }
                Section {
                    TextField("Ask about granted settings", text: $message, axis: .vertical)
                        .disabled(model.assistant.ended != nil)
                    HStack {
                        Button("Send") { if model.sendHostSettingsAssistant(message) { message = "" } }
                            .disabled(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                                      || model.assistant.isRunning || model.assistant.ended != nil || !model.canAskHostSettings)
                        Spacer()
                        Button(model.assistant.ended == nil ? "Close chat" : "New chat", role: model.assistant.ended == nil ? .destructive : nil) {
                            if model.assistant.ended == nil { model.closeHostSettingsAssistant() }
                            else { model.closeHostSettingsAssistant(); model.openHostSettingsAssistant() }
                        }
                        .disabled(model.hostSettingsBusy && model.assistant.ended == nil)
                    }
                }
            }
            if let issue = model.hostSettingsAccessIssue { Section { Text(issue).foregroundStyle(.red) } }
        }
        .navigationTitle("Ask Settings")
        .onAppear { model.hostSettingsAssistantSurfaceChanged(visible: true) }
        .onDisappear { model.hostSettingsAssistantSurfaceChanged(visible: false) }
    }

    private var cards: [SettingsChangeHint] {
        guard let operations = model.assistant.status?.operationIDs, !operations.isEmpty,
              let floor = model.assistant.changesFloor else { return [] }
        return model.hostChanges.filter { hint in
            (hint.requestID.map(operations.contains) ?? false) || (hint.requestID == nil && hint.sequence > floor)
        }
    }
}

import SwiftUI

@MainActor
struct ThisDeviceSettingsView: View {
    let store: ThisDeviceSettingsStore
    @State private var confirmingReload = false
    @State private var confirmingAppearanceReload = false

    var body: some View {
        Form {
            Section("Appearance on this iPhone") {
                if let draft = store.appearanceDraft {
                    Picker("Appearance", selection: Binding(get: { store.appearanceDraft?.appearance ?? draft.appearance }, set: { value in store.setAppearance(value) })) {
                        ForEach(DeviceAppearance.allCases, id: \.self) { value in
                            Text(appearanceLabel(value)).tag(value)
                        }
                    }
                    .disabled(!store.canEditAppearance)
                } else {
                    Text(store.isOpening ? "Reading saved appearance…" : "Saved appearance is unavailable.")
                }
                if let saved = store.appearanceSnapshot {
                    LabeledContent("Saved appearance", value: appearanceLabel(saved.appearance))
                }
                HStack {
                    Button("Save appearance") { store.saveAppearance() }
                        .disabled(!store.canSaveAppearance)
                    Button("Reload appearance") {
                        if store.hasAppearanceDraftChanges { confirmingAppearanceReload = true }
                        else { store.reloadAppearance() }
                    }
                    .disabled(!store.canReloadAppearance)
                    if store.pendingAppearancePatch != nil {
                        Button("Inspect original appearance save") { store.inspectAppearanceSave() }
                            .disabled(!store.canInspectAppearance)
                    }
                }
                if let message = store.appearanceStatus { Text(message) }
                if let error = store.appearanceError { Text(error.errorDescription ?? "Local appearance is unavailable.").foregroundStyle(.red) }
                if let patch = store.pendingAppearancePatch {
                    Text("Appearance save reference: \(patch.requestID.uuidString.lowercased())")
                        .font(.caption)
                        .textSelection(.enabled)
                }
                if store.isAppearanceSnapshotStale && store.appearanceSnapshot != nil {
                    Text("Showing the last observed saved appearance.").foregroundStyle(.secondary)
                }
                Text("Appearance and notification preferences share a saved revision. Save them separately; reload a draft if another save changes its revision.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if store.capacityReached {
                    Text("This app session has reached its retained request limit. Existing requests remain available for inspection.").foregroundStyle(.secondary)
                }
            }
            .disabled(store.isClosed)

            Section("Preferences on this iPhone") {
                if store.isOpening { ProgressView("Opening local preferences…") }
                if let draft = store.draft {
                    Toggle("Completion notifications", isOn: Binding(get: { store.draft?.completion ?? draft.completion }, set: { value in store.setCompletion(value) }))
                        .disabled(!store.canEdit)
                    Toggle("Attention notifications", isOn: Binding(get: { store.draft?.attention ?? draft.attention }, set: { value in store.setAttention(value) }))
                        .disabled(!store.canEdit)
                } else {
                    Text("Saved notification preferences are unavailable.")
                }
                Text("These preferences are saved on this iPhone independently of the Host connection. OS permission is shown separately below. Completion and attention delivery is currently unavailable; these choices can still be saved locally.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    Button("Save") { store.save() }
                        .disabled(!store.canSave)
                    Button("Reload") {
                        if store.hasDraftChanges { confirmingReload = true }
                        else { store.reload() }
                    }
                    .disabled(!store.canReload)
                    if store.pendingPatch != nil {
                        Button("Inspect original save") { store.inspectPendingSave() }
                            .disabled(!store.canInspect)
                    }
                }
                if let message = store.preferenceStatus { Text(message) }
                if let error = store.preferenceError { Text(error.errorDescription ?? "Local preferences are unavailable.").foregroundStyle(.red) }
                if let patch = store.pendingPatch {
                    Text("Save reference: \(patch.requestID.uuidString.lowercased())")
                        .font(.caption)
                        .textSelection(.enabled)
                }
                if store.isSnapshotStale && store.snapshot != nil { Text("Showing the last observed local values.").foregroundStyle(.secondary) }
                if store.capacityReached { Text("This app session has reached its retained request limit. Existing requests remain available for inspection.").foregroundStyle(.secondary) }
            }
            .disabled(store.isClosed)

            Section("iOS notification permission") {
                if let observation = store.notificationObservation {
                    LabeledContent("Authorization", value: authorizationLabel(observation.authorization))
                    LabeledContent("Alerts", value: featureLabel(observation.alerts))
                    LabeledContent("Sounds", value: featureLabel(observation.sounds))
                    LabeledContent("Notification Center", value: featureLabel(observation.notificationCenter))
                    Text("Last observed \(observation.observedAt.formatted(date: .abbreviated, time: .shortened))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text(store.isObservingAuthorization ? "Reading iOS notification settings…" : "Notification permission has not been observed.")
                }
                HStack {
                    Button(notificationButtonTitle) { store.requestNotificationAuthorization() }
                        .disabled(!store.canAuthorize)
                    Button("Refresh status") { store.refresh() }
                        .disabled(!store.isForeground || store.isObservingAuthorization || store.isRefreshing)
                }
                if let message = store.notificationStatus { Text(message) }
                if let issue = store.notificationIssue { Text(issue.message).foregroundStyle(.red) }
                if let instructions = store.notificationGuidance.instructions { Text(instructions) }
                if store.notificationObservation?.authorization == .denied {
                    Text("Open Settings, choose Notifications, then Everything to review permissions. A denied request cannot show another permission prompt.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text("A saved preference or observed permission does not confirm that a notification was delivered.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .disabled(store.isClosed)
        }
        .navigationTitle("This iPhone")
        .confirmationDialog("Discard unsaved notification preferences?", isPresented: $confirmingReload, titleVisibility: .visible) {
            Button("Reload saved preferences", role: .destructive) { store.reload() }
        } message: {
            Text("The latest saved notification preferences will replace this draft. Appearance edits are kept. An unresolved save must be inspected first.")
        }
        .confirmationDialog("Discard unsaved appearance edits?", isPresented: $confirmingAppearanceReload, titleVisibility: .visible) {
            Button("Reload saved appearance", role: .destructive) { store.reloadAppearance() }
        } message: {
            Text("The latest saved appearance will replace this draft. Notification edits are kept. An unresolved save must be inspected first.")
        }
    }

    private func appearanceLabel(_ value: DeviceAppearance) -> String {
        switch value {
        case .system: "Follow system"
        case .light: "Light"
        case .dark: "Dark"
        }
    }

    private var notificationButtonTitle: String {
        switch store.notificationObservation?.authorization {
        case .denied: "Review in Settings"
        case .notDetermined, nil: "Request notification permission"
        default: "Review notification permission"
        }
    }

    private func authorizationLabel(_ value: DeviceNotificationAuthorization) -> String {
        switch value {
        case .notDetermined: "Not determined"
        case .denied: "Denied"
        case .authorized: "Authorized"
        case .provisional: "Provisional"
        case .ephemeral: "Ephemeral"
        case .unavailable: "Unavailable"
        }
    }

    private func featureLabel(_ value: DeviceNotificationFeature) -> String {
        switch value {
        case .enabled: "Enabled"
        case .disabled: "Disabled"
        case .unsupported: "Unsupported"
        }
    }
}

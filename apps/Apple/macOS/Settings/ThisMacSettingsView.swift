import SwiftUI

@MainActor
struct ThisMacSettingsView: View {
    let store: ThisMacSettingsStore
    @State private var confirmingReload = false

    var body: some View {
        Form {
            Section("Preferences on this Mac") {
                if store.isOpening { ProgressView("Opening local preferences…") }
                Toggle("Completion notifications", isOn: Binding(get: { store.draft?.completion ?? false }, set: store.setCompletion))
                    .disabled(!store.canEdit)
                Toggle("Attention notifications", isOn: Binding(get: { store.draft?.attention ?? false }, set: store.setAttention))
                    .disabled(!store.canEdit)
                Text("These preferences are saved on this Mac independently of the Host connection. OS permission is shown separately below.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    Button("Save") { store.save() }
                        .disabled(!store.canSave)
                        .keyboardShortcut("s", modifiers: .command)
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

            Section("macOS notification permission") {
                if let observation = store.notificationObservation {
                    LabeledContent("Authorization", value: authorizationLabel(observation.authorization))
                    LabeledContent("Alerts", value: featureLabel(observation.alerts))
                    LabeledContent("Sounds", value: featureLabel(observation.sounds))
                    LabeledContent("Notification Center", value: featureLabel(observation.notificationCenter))
                    Text("Last observed \(observation.observedAt.formatted(date: .abbreviated, time: .shortened))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text(store.isObservingAuthorization ? "Reading macOS notification settings…" : "Notification permission has not been observed.")
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
                    Text("Open System Settings, choose Notifications, then Everything to review permissions. A denied request cannot show another permission prompt.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text("A saved preference or observed permission does not confirm that a notification was delivered.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .disabled(store.isClosed)
        }
        .formStyle(.grouped)
        .confirmationDialog("Discard unsaved local preference edits?", isPresented: $confirmingReload, titleVisibility: .visible) {
            Button("Reload saved preferences", role: .destructive) { store.reload() }
        } message: {
            Text("The latest saved values will replace this draft. An unresolved save must be inspected first.")
        }
    }

    private var notificationButtonTitle: String {
        switch store.notificationObservation?.authorization {
        case .denied: "Review in System Settings"
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

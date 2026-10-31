import SwiftUI

struct SettingsView: View {
    let model: AppModel
    let pair: () -> Void
    @State private var origin = ""
    @State private var disabling = false

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
                Button("Pair with Mac", action: pair).disabled(model.busy || model.state?.snapshot.revoked == true)
                Text("Pairing sets the relay from the code your Mac shows. The Mac owner approves this iPhone and chooses its access there.").font(.footnote)
            }
            Section("Connected Host access") {
                Button("Refresh Host settings") { model.refreshHostSettings() }
                    .disabled(!model.canRefreshHostSettings)
                if model.hostSettingsBusy || model.hostSettingsAccessStatus == .preparing
                    || model.hostSettingsAccessStatus == .awaitingReply {
                    Button("Cancel Host request", role: .cancel) { model.cancelHostSettingsAccess() }
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
                Text("Refreshing reads access metadata and granted values over the encrypted relay. It does not contact an AI provider. A stale observation grants no access.").font(.footnote)
            }
            HostSettingsSections(model: model)
            Section("This iPhone") {
                NavigationLink("Appearance and notifications", value: ClientSettingsDestination.thisDevice)
                if let issue = model.localSettingsIssue { Text(issue).foregroundStyle(.red) }
                Text("Local preferences remain available while the Host is offline. Notifications use your saved choices and system permission while Vorra is active and synchronization is enabled. Your first sync stays quiet.").font(.footnote)
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

    private func settingNames(_ keys: [SettingKey]) -> String {
        keys.isEmpty ? "None" : keys.map(hostSettingName).joined(separator: ", ")
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
            Text("Only public keys are shown. Pairing enrolls them on a relay that lets your host pair devices; otherwise the relay operator must enroll these exact keys.").font(.footnote)
        }
    }
}

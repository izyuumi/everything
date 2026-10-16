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
                Button("Pair with host", action: pair).disabled(model.busy || model.state?.snapshot.revoked == true)
                Text("The host chooses access separately; its signed acceptance does not contain conversation or permission grants. Add only conversation IDs that its owner authorized.").font(.footnote)
            }
            Section("This iPhone") {
                NavigationLink("Appearance and notifications", value: ClientSettingsDestination.thisDevice)
                if let issue = model.localSettingsIssue { Text(issue).foregroundStyle(.red) }
                Text("Local preferences remain available while the Host is offline. New committed completions and permission requests can trigger generic notifications while synchronization is enabled in the active app, using saved choices and current OS permission. Historical catch-up is silent; skipped or failed attempts are not replayed.").font(.footnote)
                if let status = model.notificationDeliveryStatus { Text(status).font(.footnote).foregroundStyle(.secondary) }
            }
            Section("Local privacy") {
                Text("Identity secrets use unlocked-only, non-synchronizing ThisDeviceOnly Keychain storage. Local history uses complete file protection. These Curve25519 keys are not Secure Enclave keys.").font(.footnote)
                Button("Permanently disable this device", role: .destructive) { setDisabling(true) }.disabled(model.state?.snapshot.revoked != false)
                Text("Disabling is sticky and stops this client. The host owner must separately revoke this device and its relay enrollment. Previously received local history remains here.").font(.footnote)
            }
        }
        .navigationTitle("Settings")
        .task { origin = model.state?.snapshot.origin ?? "" }
        .confirmationDialog("Permanently disable this device?", isPresented: Binding(get: { disabling }, set: { setDisabling($0) })) {
            Button("Disable device", role: .destructive) { Task { await model.disable() } }
            Button("Cancel", role: .cancel) { setDisabling(false) }
        }
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

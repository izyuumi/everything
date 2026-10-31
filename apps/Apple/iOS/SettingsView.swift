import SwiftUI

struct SettingsView: View {
    let model: AppModel
    let pair: () -> Void

    var body: some View {
        Form {
            Section {
                page("General", systemImage: "gearshape", .general)
                page("Devices", systemImage: "laptopcomputer.and.iphone", .devices)
                page("Host", systemImage: "server.rack", .host)
                page("Privacy", systemImage: "hand.raised", .privacy)
            }
            Section {
                page("About", systemImage: "info.circle", .about)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Settings")
        .navigationDestination(for: ClientSettingsDestination.self) { destination in
            switch destination {
            case .general:
                if let owner = model.localSettings {
                    ThisDeviceSettingsView(store: owner, issue: model.localSettingsIssue,
                                           deliveryStatus: model.notificationDeliveryStatus,
                                           onPresentationChange: { covered in
                        model.localCoverChanged(.localConfirmation, presented: covered)
                    })
                    .onAppear { model.localDestinationChanged(presented: true) }
                    .onDisappear { model.localDestinationChanged(presented: false) }
                } else {
                    ContentUnavailableView("Local preferences unavailable", systemImage: "lock", description: Text(model.localSettingsIssue ?? "Unlock this iPhone and reopen Vorra to access its existing protected identity."))
                }
            case .devices: DevicesSettingsView(model: model, pair: pair)
            case .host: HostSettingsPage(model: model)
            case .privacy: PrivacySettingsView(model: model)
            case .about: AboutSettingsView()
            case .hostAssistant: HostSettingsAssistantView(model: model)
            }
        }
    }

    private func page(_ title: LocalizedStringKey, systemImage: String, _ destination: ClientSettingsDestination) -> some View {
        NavigationLink(value: destination) { Label(title, systemImage: systemImage) }
    }
}

private struct DevicesSettingsView: View {
    let model: AppModel
    let pair: () -> Void

    var body: some View {
        Form {
            if let state = model.state {
                Section("This iPhone") {
                    LabeledContent("Device ID") {
                        Text(state.identity.deviceID.uuidString.lowercased()).font(.caption.monospaced()).textSelection(.enabled)
                    }
                    LabeledContent("Fingerprint") {
                        Text(state.identity.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                    }
                    PublicKeysView(keys: state.identity)
                }
                if let host = state.snapshot.host {
                    Section("Paired Host") {
                        LabeledContent("Pinned host") {
                            Text(host.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                        }
                    }
                }
            }
            Section {
                Button("Pair with Mac", systemImage: "qrcode", action: pair).disabled(model.busy || model.state?.snapshot.revoked == true)
            } footer: {
                Text("Pairing sets the relay from the code your Mac shows. The Mac owner approves this iPhone and chooses its access there.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Devices")
        .navigationBarTitleDisplayMode(.inline)
    }
}

private struct HostSettingsPage: View {
    let model: AppModel
    @State private var origin = ""

    var body: some View {
        Form {
            Section {
                TextField("wss://relay.example", text: $origin).textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                Button("Save relay") { Task { await model.saveRelay(origin) } }.disabled(model.busy || model.connectionEnabled || origin.isEmpty)
                LabeledContent("Status", value: model.state?.connection ?? "Disconnected")
                if model.connectionEnabled {
                    Button("Disconnect") { Task { await model.disconnect() } }
                } else {
                    Button("Connect while foreground") { Task { await model.connect() } }
                        .disabled(model.busy || model.state?.snapshot.host == nil || model.state?.snapshot.revoked != false)
                }
            } header: {
                Text("Relay")
            } footer: {
                Text("The relay receives encrypted envelopes and public device metadata. Use your explicitly trusted relay. Changing this endpoint discloses routing metadata to its operator. Synchronization reconnects while this app is active. iOS suspension and termination stop its connection; remote push delivery is unavailable.")
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
        }
        .formStyle(.grouped)
        .navigationTitle("Host")
        .navigationBarTitleDisplayMode(.inline)
        .onAppear { model.hostSettingsAccessSurfaceChanged(visible: true) }
        .onDisappear { model.hostSettingsAccessSurfaceChanged(visible: false) }
        .task { origin = model.state?.snapshot.origin ?? "" }
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
}

private struct PrivacySettingsView: View {
    let model: AppModel
    @State private var disabling = false

    var body: some View {
        Form {
            Section {
                Button("Permanently disable this device", role: .destructive) { setDisabling(true) }.disabled(model.state?.snapshot.revoked != false)
            } header: {
                Text("Local privacy")
            } footer: {
                Text("Identity secrets use unlocked-only, non-synchronizing ThisDeviceOnly Keychain storage. Local history uses complete file protection. These Curve25519 keys are not Secure Enclave keys. Disabling is sticky and stops this client. The host owner must separately revoke this device and its relay enrollment. Previously received local history remains here.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Privacy")
        .navigationBarTitleDisplayMode(.inline)
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

private struct AboutSettingsView: View {
    private let info = Bundle.main.infoDictionary

    var body: some View {
        Form {
            Section {
                LabeledContent("Version", value: info?["CFBundleShortVersionString"] as? String ?? "Unknown")
                LabeledContent("Build", value: info?["CFBundleVersion"] as? String ?? "Unknown")
            } footer: {
                Text("Vorra for iOS is a paired client of your Vorra host. The host owns execution and durable state.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle("About")
        .navigationBarTitleDisplayMode(.inline)
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

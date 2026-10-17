import AppKit
import Observation
import SwiftUI

@MainActor
struct NativeDevicesControls: View {
    let store: NativeSettingsStore
    let connectionEdited: Bool
    @State private var parent = NativeDeviceWindowReference()

    var body: some View {
        Form {
            Section("Original Host session") {
                LabeledContent("Session", value: store.status)
                HStack {
                    Button("Open Host settings") { store.open() }.disabled(!store.canOpen || connectionEdited)
                    Button("Refresh Host settings") { store.refresh() }.disabled(!store.canRefresh || connectionEdited)
                    Button("Close session") { store.close() }.disabled(!store.sessionPresent)
                }
                if let host = store.information?.hostID {
                    Text("Host: \(host.uuidString.lowercased())").font(.caption.monospaced()).textSelection(.enabled)
                }
                if connectionEdited { Text("Apply connection edits in General first.").foregroundStyle(.secondary) }
                if let issue = store.issue { NativeSettingsNotice(issue.errorDescription ?? "The original Host session is unavailable.") }
                Text("Devices belong to this Host. This list is separate from This Mac's offline notification preferences. Open and pairing are explicit; neither requires a model.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Observed devices") {
                HStack {
                    Button("Refresh first page") { store.refreshDevices() }.disabled(!store.canRefreshDevices || connectionEdited)
                    Button("Next page") { store.refreshDevices(first: false) }
                        .disabled(!store.canRefreshDevices || store.devicesAreStale || store.devicePage?.nextAfter == nil || connectionEdited)
                }
                if store.devicesLoading { ProgressView("Reading one bounded device page…") }
                if let issue = store.deviceIssue { NativeSettingsNotice(issue.errorDescription ?? "The original device request is unconfirmed.") }
                if let page = store.devicePage {
                    Text("Page observed \(Date(timeIntervalSince1970: Double(page.observedAt) / 1000).formatted())")
                        .font(.caption).foregroundStyle(.secondary)
                    if store.devicesAreStale { NativeSettingsNotice("Cached page: stale or offline. Refresh the original session before using a row.", kind: .warning) }
                    if page.devices.isEmpty { Text("This observed page contains no devices.") }
                    ForEach(page.devices, id: \.deviceID) { row in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(row.label).font(.headline)
                            Text(row.deviceID.uuidString.lowercased()).font(.caption.monospaced()).textSelection(.enabled)
                            Text(row.publicKeys.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                            if row.thisDevice { Text("Host endpoint identity").font(.caption) }
                            Text(row.revoked ? "Observed revoked" : "Observed active")
                            Button("Request revocation for \(row.label)…", role: .destructive) {
                                store.requestDeviceRevocation(row, parent: parent.window)
                            }
                            .accessibilityLabel("Request revocation for \(row.label), device \(row.deviceID.uuidString.lowercased())")
                            .disabled(!store.canRevokeDevice(row) || connectionEdited || parent.window == nil)
                        }.padding(.vertical, 6)
                    }
                } else { Text("Read the first device page through an explicitly opened Host session.").foregroundStyle(.secondary) }
                Text("Each page replaces the last one and is independently observed. Existing conversation grants are not included; a row is not evidence of current remote execution or pairing completion.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Pair a device") {
                Button("Start a private pairing ceremony…") { store.beginDevicePairing(parent: parent.window) }
                    .disabled(!store.canBeginDevicePairing || connectionEdited || parent.window == nil)
                Text("Save or discard drafts first. The native sheet keeps the original invitation alive while you compare the requesting device, choose exact access and explicitly approve. Host approval and completion on the requesting device are separate.")
                    .font(.caption).foregroundStyle(.secondary)
                Text("One private sheet, eight ceremonies per original session, 64 combined identities and four owned workers are bounded with the existing credential and settings work. Capacity refusal never evicts an unresolved operation.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if !store.deviceOperations.isEmpty {
                Section("Original device operations") {
                    ForEach(store.deviceOperations) { operation in operationRow(operation) }
                }
            }
        }
        .formStyle(.grouped)
        .padding(12)
        .background {
            NativeDeviceWindowReader { window in
                if parent.window !== window, parent.window != nil { store.devicesSurfaceHidden() }
                parent.window = window
            }.frame(width: 0, height: 0)
        }
        .onDisappear { store.devicesSurfaceHidden() }
    }

    private func operationRow(_ value: NativeSettingsStore.DeviceOperation) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(value.request.action == .pair ? "Original invitation creation" : "Original exact-device revocation").font(.headline)
            Text(NativeSettingsText.operation(value.operation))
            Text("Host: \(value.request.scope.identifier.uuidString.lowercased())").font(.caption.monospaced())
            Text("Operation: \(value.id.uuidString.lowercased())").font(.caption.monospaced())
            if value.retired { Text("Dismissal requested and private fields cleared. Sheet cleanup and admitted effects remain owned until joined.").font(.caption).foregroundStyle(.secondary) }
            if let lifecycle = value.pairingLifecycle {
                Text("Last Host invitation state: " + lifecycle.rawValue.replacingOccurrences(of: "_", with: " ")).font(.caption)
            }
            if let issue = value.issue ?? value.operation.issue { NativeSettingsNotice(issue.errorDescription ?? "Inspect the original operation.") }
            if let cleanup = value.cleanupIssue {
                NativeSettingsNotice("Original native cleanup remains unconfirmed: " + (cleanup.errorDescription ?? "Original cleanup is uncertain."), kind: .warning)
            }
            Button("Inspect original device operation") { store.inspectDeviceOperation(value.handle) }
                .disabled(!store.canInspectDeviceOperation(value.handle) || connectionEdited)
            if let approvalID = value.approvalID {
                Text("Separate final approval: \(approvalID.uuidString.lowercased())").font(.caption.monospaced())
                if value.approvalWorking { ProgressView("Original approval or private delivery is owned…") }
                if let status = value.approval {
                    if case .finished(let result) = status.operation.phase { Text(NativeSettingsText.result(result.state)) }
                    else { NativeSettingsNotice("Original final approval is unresolved.", kind: .warning) }
                    switch status.hostPin {
                    case .notEstablished: Text("No committed Host pin observed.")
                    case .committed(let label, let pinned, _, let revoked):
                        Text("Historical Host pin: \(label) · \(Date(timeIntervalSince1970: Double(pinned) / 1000).formatted())")
                        if revoked != nil { NativeSettingsNotice("The Host pin is observed revoked.", kind: .warning) }
                        Text("This receipt does not prove the requesting device completed pairing.").font(.caption).foregroundStyle(.secondary)
                    }
                }
                Button("Inspect original final approval") { store.inspectPairingApproval(value.handle) }
                    .disabled(!store.canInspectPairingApproval(value.handle) || connectionEdited)
            }
        }.padding(.vertical, 8)
    }
}

@MainActor
@Observable
private final class NativeDeviceWindowReference {
    weak var window: NSWindow?
}

@MainActor
private struct NativeDeviceWindowReader: NSViewRepresentable {
    let onChange: @MainActor (NSWindow?) -> Void

    func makeNSView(context: Context) -> WindowView {
        let view = WindowView()
        view.onChange = onChange
        return view
    }

    func updateNSView(_ view: WindowView, context: Context) { view.onChange = onChange }

    static func dismantleNSView(_ view: WindowView, coordinator: ()) {
        view.onChange?(nil)
        view.onChange = nil
    }

    @MainActor
    final class WindowView: NSView {
        var onChange: (@MainActor (NSWindow?) -> Void)?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            onChange?(window)
        }
    }
}

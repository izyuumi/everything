import SwiftUI

struct PairingView: View {
    let model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var invitation = ""
    @State private var acceptance = ""
    @State private var confirmed = false
    @State private var scanning: CameraSheet?

    private enum CameraSheet: String, Identifiable {
        case invitation
        var id: String { rawValue }
    }

    var body: some View {
        Form {
            Section("1. Receive the host invitation") {
                Text("Use a fresh invitation displayed by your trusted host. Invitations contain a secret; keep them out of messages, relay traffic and logs.").font(.footnote)
                Button("Scan invitation", systemImage: "qrcode.viewfinder") { scanning = .invitation }
                SecureField("Or paste invitation JSON", text: $invitation).textInputAutocapitalization(.never).autocorrectionDisabled()
                Button("Inspect invitation") {
                    let value = invitation
                    invitation = ""
                    confirmed = false
                    Task { await model.inspectInvitation(value) }
                }.disabled(model.busy || invitation.isEmpty || invitation.utf8.count > 4_096)
            }
            if let pairing = model.pairing {
                Section("2. Confirm the host fingerprint") {
                    Text(pairing.fingerprint).font(.caption.monospaced()).textSelection(.enabled)
                    Text("Compare every group with the fingerprint on your host before continuing. A QR code alone does not confirm the host you intended to pair.").font(.footnote)
                    Toggle("Fingerprint matches my host", isOn: $confirmed)
                    Button("Create pairing request") { Task { await model.requestPairing(confirmedFingerprint: pairing.fingerprint) } }
                        .disabled(!confirmed || model.busy || !model.pairingRequest.isEmpty)
                }
            }
            if !model.pairingRequest.isEmpty {
                Section("3. Ask the host owner to accept") {
                    Text(model.pairingRequest).font(.caption.monospaced()).textSelection(.enabled)
                    Text("Transfer this public authenticated request to the host. Its owner must confirm this phone's fingerprint and choose its access. Relay public-key enrollment is a separate operator step.").font(.footnote)
                    if let keys = model.state?.identity { Text("Phone fingerprint\n" + keys.fingerprint).font(.caption.monospaced()) }
                    TextField("Paste signed acceptance JSON", text: $acceptance, axis: .vertical).textInputAutocapitalization(.never).autocorrectionDisabled().lineLimit(3...6)
                    Button("Verify acceptance and pair") {
                        let value = acceptance
                        acceptance = ""
                        Task { if await model.finishPairing(value) { dismiss() } }
                    }.disabled(model.busy || acceptance.isEmpty || acceptance.utf8.count > 4_096)
                }
            }
        }
        .privacySensitive()
        .navigationTitle("Pair with host")
        .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
        .sheet(item: $scanning) { _ in
            QRScanner { value in
                scanning = nil
                invitation = ""
                confirmed = false
                Task { await model.inspectInvitation(value) }
            }
            .ignoresSafeArea()
            .modifier(InactivePrivacyCover())
        }
        .onDisappear { invitation = ""; acceptance = ""; Task { await model.cancelPairing() } }
    }
}

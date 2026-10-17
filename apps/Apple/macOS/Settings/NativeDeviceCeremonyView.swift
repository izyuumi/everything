import SwiftUI

@MainActor
struct NativeDeviceCeremonyView: View {
    @Bindable var ceremony: NativeDeviceCeremony

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(ceremony.request.action == .pair ? "Pair a device" : "Revoke this exact device").font(.title2)
            Text("Original Host: \(ceremony.request.scope.identifier.uuidString.lowercased())")
                .font(.caption.monospaced()).textSelection(.enabled)
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    content
                    if let issue = ceremony.issue { NativeSettingsNotice(issue) }
                    if ceremony.retirementUncertain {
                        NativeSettingsNotice("The native sheet has not confirmed that it is hidden. Original cleanup remains owned.", kind: .warning)
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            Divider()
            Text("Private transfer values remain in this ceremony's app and OS memory. Closing clears these fields; it cannot erase prior copies or screenshots.")
                .font(.caption).foregroundStyle(.secondary)
            HStack {
                Button(ceremony.stage == .acceptance || ceremony.stage == .finished ? "Done" : "Cancel") { ceremony.dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Text("No automatic device approval").font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(24)
        .frame(minWidth: 620, minHeight: 680)
        .background {
            TimelineView(.periodic(from: Date(), by: 1)) { context in
                Color.clear.onChange(of: context.date) { _, _ in ceremony.expireIfNeeded() }
            }
        }
        .privacySensitive()
    }

    @ViewBuilder
    private var content: some View {
        switch ceremony.stage {
        case .preparing: ProgressView("Preparing the original native device request…")
        case .confirmation: confirmation
        case .confirming: ProgressView("Waiting for the original native device effect…")
        case .unconfirmed:
            NativeSettingsNotice("The original result or private delivery is unconfirmed. Inspect the original identity; no effect is retried.", kind: .warning)
            Button("Inspect original device operation") { ceremony.inspectOriginal() }
            Button("Inspect original final approval") { ceremony.inspectApproval() }
        case .invitation, .review: invitation
        case .approving:
            ProgressView("Waiting for the original Host approval…")
            Text("If the response is lost, inspect the same approval. A committed Host pin is not undone by dismissing this window.")
            Button("Inspect original approval") { ceremony.inspectApproval() }
        case .acceptance:
            Text("Host approval and acceptance verified").font(.headline)
            Text("Transfer this private signed acceptance to the requesting device's existing acceptance field. That device must finish its own pairing; this Mac has not completed it.")
            Text(ceremony.acceptanceText).font(.caption.monospaced()).textSelection(.enabled)
                .accessibilityLabel("Verified private pairing acceptance")
        case .finished:
            Text("The original device operation finished. Review its actual receipt in Devices.")
        }
    }

    @ViewBuilder
    private var confirmation: some View {
        switch ceremony.request.action {
        case .pair:
            Text("Create a short-lived private invitation").font(.headline)
            Text("This creates an invitation only. The requesting device's signed identity and your exact access choices require a separate explicit approval.")
            Button("Create private invitation") { ceremony.confirm() }.keyboardShortcut(.defaultAction)
        case .revoke(let id, let keys):
            Text("Confirm the exact public identity").font(.headline)
            publicIdentity(keys)
            if id == ceremony.request.scope.identifier {
                NativeSettingsNotice("This is the Host's own device identity. Revocation can remove endpoint authority and disconnect clients; it does not stop the independent Host process.", kind: .warning)
            }
            Toggle("I checked this device's identity and intend to revoke it", isOn: $ceremony.compared)
            Button("Revoke this exact device", role: .destructive) { ceremony.confirm() }
                .disabled(!ceremony.compared)
            Text("The Host revalidates the original full keys, authority and expiry. An already-revoked result does not prove which caller caused it.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var invitation: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let fingerprint = ceremony.hostFingerprint {
                Text("Compare this Host fingerprint on the requesting device").font(.headline)
                Text(fingerprint).font(.caption.monospaced()).textSelection(.enabled)
            }
            if let image = ceremony.invitationImage {
                Image(nsImage: image).interpolation(.none).resizable().scaledToFit()
                    .frame(width: 240, height: 240).padding(12).background(.white)
                    .accessibilityLabel("Private pairing invitation QR code")
            } else {
                Text("A native QR image is unavailable. Use the genuine private invitation text below.").foregroundStyle(.secondary)
            }
            DisclosureGroup("Private invitation text") {
                Text(ceremony.invitationText).font(.caption.monospaced()).textSelection(.enabled)
                    .accessibilityLabel("Private pairing invitation")
            }
            Text("Paste the requesting device's signed request. Scanning the invitation does not approve that device.")
            TextEditor(text: Binding(get: { ceremony.requestText }, set: { ceremony.setRequestText($0) }))
                .font(.caption.monospaced()).frame(minHeight: 90)
                .accessibilityLabel("Private signed pairing request, at most 4096 bytes")
            Button("Review this signed request") { ceremony.reviewRequest() }.disabled(ceremony.requestText.isEmpty)
            if let review = ceremony.review {
                Divider()
                Text("Verify the requesting device before approval").font(.headline)
                publicIdentity(review.responder)
                Toggle("I compared this fingerprint with the intended device", isOn: $ceremony.compared)
                TextField("Device label", text: Binding(get: { ceremony.label }, set: { if $0.utf8.count <= 256 { ceremony.label = $0 } }))
                Toggle("Allow all conversations", isOn: $ceremony.allConversations)
                if ceremony.allConversations {
                    NativeSettingsNotice("All includes future conversations. This is an explicit broad grant.", kind: .warning)
                } else {
                    Text("Selected conversation UUIDs; leave empty for no conversations.").font(.caption)
                    TextEditor(text: Binding(get: { ceremony.conversationIDs }, set: { if $0.utf8.count <= 4096 { ceremony.conversationIDs = $0 } }))
                        .font(.caption.monospaced()).frame(minHeight: 70)
                        .accessibilityLabel("Up to 64 distinct conversation UUIDs, separated by spaces or commas")
                }
                Toggle("Allow command submission", isOn: $ceremony.submitCommands)
                    .onChange(of: ceremony.submitCommands) { _, enabled in if !enabled { ceremony.resolvePermissions = false } }
                Toggle("Allow permission resolution", isOn: $ceremony.resolvePermissions).disabled(!ceremony.submitCommands)
                Text("Approval uses this exact reviewed device, label and access selection. Parsing the request is not cryptographic approval; the Host verifies the original signed request.")
                    .font(.caption).foregroundStyle(.secondary)
                Button("Approve this exact device and access") { ceremony.approve() }
                    .disabled(!ceremony.compared || ceremony.label.isEmpty)
            }
        }
    }

    private func publicIdentity(_ keys: DevicePublicKeys) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Device: \(keys.deviceID.uuidString.lowercased())")
            Text("Fingerprint: \(keys.fingerprint)")
            DisclosureGroup("Full public keys") {
                Text("Signing: \(keys.signingKey.base64EncodedString())")
                Text("Encryption: \(keys.encryptionKey.base64EncodedString())")
            }
        }.font(.caption.monospaced()).textSelection(.enabled)
    }
}

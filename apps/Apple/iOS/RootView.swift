import SwiftUI

private enum ClientSheet: String, Identifiable {
    case pairing, conversation
    var id: String { rawValue }
}

struct RootView: View {
    @Bindable var model: AppModel
    @State private var sheet: ClientSheet?

    var body: some View {
        TabView(selection: Binding(get: { model.selectedTab }, set: { model.localTabChanged($0) })) {
            NavigationStack {
                Group {
                    if let state = model.state {
                        if state.snapshot.revoked {
                            ContentUnavailableView("Device disabled", systemImage: "lock.slash", description: Text("The host or this device disabled this connection. Previously received local history remains on this device."))
                        } else if state.snapshot.conversations.isEmpty {
                            ContentUnavailableView {
                                Label("Choose a conversation", systemImage: "bubble.left.and.bubble.right")
                            } description: {
                                Text("Pair with your host, then add a conversation ID that the host explicitly authorized.")
                            } actions: {
                                Button("Pair device") { showSheet(.pairing) }
                                Button("Add conversation") { showSheet(.conversation) }
                            }
                        } else {
                            ChatView(model: model)
                        }
                    } else {
                        ContentUnavailableView("Local storage unavailable", systemImage: "lock", description: Text("Unlock the device and reopen Vorra to access its protected identity and history."))
                    }
                }
                .navigationTitle("Vorra")
                .toolbar {
                    ToolbarItem(placement: .topBarTrailing) {
                        Menu {
                            Button("Add conversation", systemImage: "plus.bubble") { showSheet(.conversation) }
                            Button("Pair device", systemImage: "qrcode") { showSheet(.pairing) }
                        } label: { Image(systemName: "plus") }
                        .accessibilityLabel("Conversation and pairing actions")
                    }
                }
            }
            .tabItem { Label("Chat", systemImage: "bubble.left.and.bubble.right") }
            .tag(ClientTab.chat)
            NavigationStack { ApprovalsView(model: model) }
                .tabItem { Label("Approvals", systemImage: "checkmark.shield") }
                .tag(ClientTab.approvals)
            NavigationStack(path: Binding(get: { model.settingsPath }, set: { model.localSettingsPathChanged($0) })) {
                SettingsView(model: model, pair: { showSheet(.pairing) })
                    .navigationDestination(for: ClientSettingsDestination.self) { destination in
                        switch destination {
                        case .thisDevice:
                            if let owner = model.localSettings {
                                ThisDeviceSettingsView(store: owner, onPresentationChange: { covered in
                                    model.localCoverChanged(.localConfirmation, presented: covered)
                                })
                                .onAppear { model.localDestinationChanged(presented: true) }
                                .onDisappear { model.localDestinationChanged(presented: false) }
                            } else {
                                ContentUnavailableView("Local preferences unavailable", systemImage: "lock", description: Text(model.localSettingsIssue ?? "Unlock this iPhone and reopen Vorra to access its existing protected identity."))
                            }
                        case .hostAssistant:
                            HostSettingsAssistantView(model: model)
                        }
                    }
            }
            .tabItem { Label("Settings", systemImage: "gearshape") }
            .tag(ClientTab.settings)
        }
        .sheet(item: Binding(get: { sheet }, set: { value in
            if value != nil { model.localCoverChanged(.rootSheet, presented: true) }
            sheet = value
        }), onDismiss: {
            if sheet == nil { model.localCoverChanged(.rootSheet, presented: false) }
        }) { item in
            switch item {
            case .pairing: NavigationStack { PairingView(model: model) }.modifier(InactivePrivacyCover())
            case .conversation: NavigationStack { AddConversationView(model: model) }.modifier(InactivePrivacyCover())
            }
        }
        .onDisappear { model.localDestinationChanged(presented: false) }
        .alert("Operation unavailable", isPresented: Binding(get: { model.error != nil }, set: { if !$0 { model.dismissError() } })) {
            Button("OK", role: .cancel) { model.dismissError() }
        } message: { Text(model.error ?? "") }
    }

    private func showSheet(_ value: ClientSheet) {
        guard sheet == nil else { return }
        model.localCoverChanged(.rootSheet, presented: true)
        sheet = value
    }
}

private struct AddConversationView: View {
    let model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var identifier = ""
    @State private var title = ""

    var body: some View {
        Form {
            Section("Authorized conversation") {
                TextField("Conversation UUID", text: $identifier).textInputAutocapitalization(.never).autocorrectionDisabled()
                TextField("Local title", text: $title)
                Text("Obtain the ID from your host. Adding an ID does not grant access or create a host conversation.").font(.footnote)
            }
            Button("Add") {
                Task { if await model.addConversation(identifier, title: title) { dismiss() } }
            }.disabled(model.busy || identifier.isEmpty)
        }
        .navigationTitle("Add conversation")
        .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
    }
}

private struct ApprovalsView: View {
    let model: AppModel
    @State private var selected: ReplicaApproval?
    @State private var decision = "allow"

    private func eligible(_ approval: ReplicaApproval, at date: Date) -> Bool {
        model.state?.snapshot.revoked == false && model.state?.snapshot.approvals.contains(approval) == true && approval.canAllow(at: date)
    }

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            List {
                if model.state?.snapshot.approvals.isEmpty != false {
                    Text("No unresolved permission requests are stored locally. Synchronize an authorized conversation to check for changes.")
                }
                ForEach(model.state?.snapshot.approvals ?? []) { approval in
                    Section(approval.tool) {
                        Text("Status reflects the last received host events. The host checks the current operation before execution.").font(.footnote)
                        identity(approval)
                        if approval.vaultUse { vaultUse(approval) }
                        if let inspection = approval.inspection {
                            PermissionInspectionView(inspection: inspection, eligible: eligible(approval, at: context.date))
                        } else {
                            if let arguments = approval.arguments {
                                DisclosureGroup("Earlier tool-call arguments") { Text(arguments).font(.caption.monospaced()).textSelection(.enabled) }
                            }
                            Text("Review this operation on the host. This event does not include its complete authorization scope and expiry.").font(.footnote)
                        }
                        HStack {
                            Button(approval.vaultUse ? "Allow once" : "Allow this operation") { decision = "allow"; selected = approval }.disabled(!eligible(approval, at: context.date) || model.busy)
                            Button("Deny", role: .destructive) { Task { await model.resolve(approval, decision: "deny") } }.disabled(model.busy || model.state?.snapshot.revoked != false)
                        }.buttonStyle(.borderless)
                        if approval.leasable {
                            Button("Allow for this task") { decision = "allow_for_task"; selected = approval }.buttonStyle(.borderless).disabled(!eligible(approval, at: context.date) || model.busy)
                        }
                    }
                }
            }
            .confirmationDialog(decision == "allow_for_task" ? "Allow this vault use for the rest of this task?" : "Allow the displayed exact operation?", isPresented: Binding(get: { selected != nil }, set: { if !$0 { selected = nil } }), titleVisibility: .visible) {
                if let selected {
                    Button(decision == "allow_for_task" ? "Allow for this task" : selected.vaultUse ? "Allow once" : "Allow this operation") { Task { await model.resolve(selected, decision: decision); self.selected = nil } }
                        .disabled(!eligible(selected, at: context.date) || model.busy || (decision == "allow_for_task" && !selected.leasable))
                }
                Button("Cancel", role: .cancel) { selected = nil }
            } message: {
                Text(decision == "allow_for_task"
                     ? "For one hour, this task may use the same item for the same destination without asking again. Other tasks, items and destinations still ask. The host revalidates this device's permission grant; a queued decision does not mean execution completed."
                     : "The host revalidates the exact operation, its expiry and this device's permission grant. A queued decision does not mean execution completed.")
            }
        }
        .navigationTitle("Approvals")
    }

    private func vaultUse(_ approval: ReplicaApproval) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Vault use").font(.headline)
            LabeledContent("Item", value: approval.vaultTargets("label").first ?? "Unknown")
            LabeledContent("Provider", value: approval.vaultTargets("provider").first ?? "Unknown")
            LabeledContent("Kind", value: approval.vaultTargets("kind").first ?? "Unknown")
            let sites = approval.vaultTargets("site")
            if !sites.isEmpty { LabeledContent("Sites", value: sites.joined(separator: ", ")) }
            LabeledContent("Field", value: approval.vaultTargets("field").first ?? "Unknown")
            LabeledContent("Destination", value: approval.vaultTargets("destination").first ?? "Unknown")
            LabeledContent("Reason", value: approval.vaultReason ?? "None")
            Text("Only metadata is shown. The value never reaches this device, and secrets cannot be entered here.").font(.footnote).foregroundStyle(.secondary)
        }
    }

    private func identity(_ approval: ReplicaApproval) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Operation: " + approval.id.uuidString.lowercased())
            Text("Conversation: " + approval.conversationID.uuidString.lowercased())
            Text("Task: " + approval.taskID.uuidString.lowercased())
            Text("Tool call: " + approval.callID.uuidString.lowercased())
        }.font(.caption.monospaced()).textSelection(.enabled)
    }
}

private struct PermissionInspectionView: View {
    let inspection: WirePermissionInspection
    let eligible: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            identity
            scope
            arguments
        }
    }

    private var identity: some View {
        Group {
            Text("Approval ID").font(.caption).foregroundStyle(.secondary)
            Text(inspection.approvalID).font(.caption.monospaced()).textSelection(.enabled)
            Text("Workspace").font(.headline)
            Text(inspection.workspaceRoot).font(.body.monospaced()).textSelection(.enabled)
            Text("Execution mode: " + inspection.mode)
        }
    }

    private var scope: some View {
        Group {
            if inspection.capabilities.contains("shell") {
                Text("Shell commands run with the host permissions shown here; listed paths do not provide shell confinement.").font(.footnote)
            }
            Text("Required capabilities").font(.headline)
            if inspection.capabilities.isEmpty { Text("None declared") }
            ForEach(Array(inspection.capabilities.enumerated()), id: \.offset) { _, capability in Text(capability) }
            Text("Targets").font(.headline)
            if inspection.targets.isEmpty { Text("None declared") }
            ForEach(Array(inspection.targets.enumerated()), id: \.offset) { _, target in Text(target).font(.body.monospaced()).textSelection(.enabled) }
        }
    }

    private var arguments: some View {
        Group {
            Text("Complete saved arguments").font(.headline)
            Text(inspection.argumentsJSON).font(.caption.monospaced()).textSelection(.enabled)
            Text("Arguments SHA-256").font(.caption).foregroundStyle(.secondary)
            Text(inspection.argumentsDigest).font(.caption.monospaced()).textSelection(.enabled)
            Text("Requested: " + Date(timeIntervalSince1970: Double(inspection.requestedAt) / 1_000).formatted(date: .abbreviated, time: .standard)).font(.footnote)
            Text("Expires: " + Date(timeIntervalSince1970: Double(inspection.expiresAt) / 1_000).formatted(date: .abbreviated, time: .standard)).font(.footnote)
            if !eligible { Text("This request is expired or no longer eligible locally. Review it on the host.").font(.footnote) }
        }
    }
}

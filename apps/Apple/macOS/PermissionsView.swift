import SwiftUI

@MainActor
struct PermissionsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Picker("Conversation", selection: $model.selectedConversation) {
                Text("Choose a conversation").tag(Optional<OwnerID>.none)
                ForEach(model.conversations) { conversation in
                    Text(conversation.title).tag(Optional(conversation.id))
                }
            }
            .disabled(!model.connected)
            HStack {
                Text("Pending requests for this conversation").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Refresh") { Task { await model.loadApprovals() } }.disabled(model.approvalLoading)
            }
            List(model.approvals) { approval in
                Button { Task { await model.inspect(approval) } } label: {
                    VStack(alignment: .leading) {
                        Text(approval.tool).font(.headline)
                        Text("\(approval.argumentsBytes) bytes of arguments • Inspect before deciding")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }.buttonStyle(.plain)
            }
            if model.approvalCursor != .null {
                Button("Load next 20 requests") { Task { await model.loadApprovals(more: true) } }
                    .disabled(model.approvalLoading || model.approvals.count >= 1000)
            }
            if model.approvals.isEmpty && !model.approvalLoading {
                Text("No pending requests loaded for the selected conversation.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.top, 8)
        .task(id: model.selectedConversation) { await model.loadApprovals() }
    }
}

@MainActor
struct ApprovalDetailsView: View {
    @Bindable var model: HostModel
    let approval: ApprovalDetails
    @Environment(\.dismiss) private var dismiss
    @State private var confirmingAllow = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Permission for \(approval.summary.tool)").font(.title2)
            Text(approval.id).font(.caption.monospaced()).textSelection(.enabled)
            Text("Scope, targets and exact arguments").font(.headline)
            ScrollView {
                Text(approval.value.formatted)
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack {
                if approval.pending {
                    Button("Deny", role: .destructive) { Task { await model.decide(approval, allow: false); dismiss() } }.disabled(!model.canMutate)
                    Button("Allow this operation…") { confirmingAllow = true }.disabled(!model.canMutate)
                } else {
                    Text("This request is no longer pending.").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(20).frame(width: 700, height: 600)
        .confirmationDialog("Allow this exact tool operation?", isPresented: $confirmingAllow, titleVisibility: .visible) {
            Button("Allow this operation") { Task { await model.decide(approval, allow: true); dismiss() } }
        } message: {
            Text("The host binds this decision to the displayed conversation, task, operation and argument digest. It grants no general future permission.")
        }
    }
}

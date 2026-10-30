import SwiftUI

@MainActor
struct PermissionsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Pending approvals across all conversations").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Refresh") { Task { await model.loadApprovals() } }.disabled(model.approvalLoading || !model.connected)
            }
            List(model.approvals) { approval in
                Button { Task { await model.inspect(approval) } } label: {
                    VStack(alignment: .leading) {
                        Text(approval.tool).font(.headline)
                        Text(title(approval.conversationID)).font(.caption).lineLimit(1)
                        Text("Requested \(JSONSummary.date(approval.requestedAt)) • \(approval.argumentsBytes) bytes of arguments")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }.buttonStyle(.plain)
            }
            if model.approvalsLimited {
                Text("Showing the first 500 pending approvals.").font(.caption).foregroundStyle(.secondary)
            }
            if model.approvals.isEmpty && !model.approvalLoading {
                Text("No pending approvals.").font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.top, 8)
        .task(id: model.conversations.map(\.id)) { await model.loadApprovals() }
    }

    private func title(_ id: OwnerID) -> String {
        guard let title = model.conversations.first(where: { $0.id == id })?.title, !title.isEmpty else { return "Conversation " + id.string }
        return title
    }
}

@MainActor
struct ApprovalDetailsView: View {
    @Bindable var model: HostModel
    let approval: ApprovalDetails
    @Environment(\.dismiss) private var dismiss
    @State private var confirmingAllow = false

    private var scope: JSONValue { approval.request["scope"] }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Permission for \(approval.summary.tool)").font(.title2)
            Text(approval.id).font(.caption.monospaced()).textSelection(.enabled)
            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    LabeledContent("Tool", value: approval.summary.tool)
                    LabeledContent("Workspace") { Text(scope["workspace_root"].string ?? "Unknown").textSelection(.enabled) }
                    LabeledContent("Mode", value: JSONSummary.label(scope["mode"].string ?? "unknown"))
                    LabeledContent("Status", value: JSONSummary.label(approval.value["status"]["status"].string ?? "unknown"))
                    LabeledContent("Requested", value: JSONSummary.date(approval.request["requested_at"].unsigned))
                    LabeledContent("Expires", value: JSONSummary.date(approval.request["expires_at"].unsigned))
                    if let targets = scope["targets"].array?.compactMap(\.string), !targets.isEmpty {
                        LabeledContent("Targets") { Text(targets.joined(separator: "\n")).textSelection(.enabled) }
                    }
                    Text("Arguments").font(.headline).padding(.top, 6)
                    Text(JSONSummary.lines(approval.request["arguments"]))
                        .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    DisclosureGroup("Raw JSON") {
                        Text(approval.value.formatted)
                            .font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }
                }.font(.callout).frame(maxWidth: .infinity, alignment: .leading)
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

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
    @State private var confirming: String?

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
                    if approval.vaultUse { vaultUse }
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
                    Button("Deny", role: .destructive) { Task { await model.decide(approval, decision: "deny"); dismiss() } }.disabled(!model.canMutate)
                    Button(approval.vaultUse ? "Allow once…" : "Allow this operation…") { confirming = "allow" }.disabled(!model.canMutate)
                    if approval.leasable {
                        Button("Allow for this task…") { confirming = "allow_for_task" }.disabled(!model.canMutate)
                    }
                } else {
                    Text("This request is no longer pending.").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(20).frame(width: 700, height: 600)
        .confirmationDialog(confirming == "allow_for_task" ? "Allow this vault use for the rest of this task?" : "Allow this exact tool operation?",
                            isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }), titleVisibility: .visible) {
            Button(confirming == "allow_for_task" ? "Allow for this task" : approval.vaultUse ? "Allow once" : "Allow this operation") {
                guard let decision = confirming else { return }
                Task { await model.decide(approval, decision: decision); dismiss() }
            }
        } message: {
            Text(confirming == "allow_for_task"
                 ? "For one hour, this task may use the same item for the same destination without asking again. Other tasks, items and destinations still ask. You can revoke it."
                 : "The host binds this decision to the displayed conversation, task, operation and argument digest. It grants no general future permission.")
        }
    }

    @ViewBuilder
    private var vaultUse: some View {
        Text("Vault use").font(.headline).padding(.top, 6)
        LabeledContent("Item", value: approval.vaultTargets("label").first ?? "Unknown")
        LabeledContent("Provider", value: approval.vaultTargets("provider").first ?? "Unknown")
        LabeledContent("Kind", value: JSONSummary.label(approval.vaultTargets("kind").first ?? "unknown"))
        LabeledContent("Field", value: approval.vaultTargets("field").first ?? "Unknown")
        LabeledContent("Destination", value: approval.vaultTargets("destination").first ?? "Unknown")
        let sites = approval.vaultTargets("site")
        if !sites.isEmpty { LabeledContent("Sites", value: sites.joined(separator: ", ")) }
        LabeledContent("Reason") { Text(approval.request["arguments"]["reason"].string ?? "None").textSelection(.enabled) }
        Text("Only metadata is shown. The value is never part of this request.").font(.caption).foregroundStyle(.secondary)
    }
}

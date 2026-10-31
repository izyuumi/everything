import AppKit
import SwiftUI

@MainActor
struct MenuContent: View {
    @Bindable var model: HostModel
    @State private var clearingReminder = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Image(systemName: model.connected ? "circle.fill" : "circle")
                    .foregroundStyle(model.connected ? Color.green : Color.secondary)
                    .accessibilityHidden(true)
                VStack(alignment: .leading) {
                    Text("Vorra").font(.headline)
                    Text(model.status).font(.caption).foregroundStyle(.secondary)
                    if let service = model.serviceStatus {
                        Text("Service: " + service).font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Button { Task { await model.refresh() } } label: { Image(systemName: "arrow.clockwise") }
                    .disabled(model.refreshing || model.closing)
                    .help("Refresh host status")
                    .accessibilityLabel("Refresh host status")
                SettingsLink { Image(systemName: "gearshape") }
                    .help("Connection and host settings")
                    .accessibilityLabel("Open settings")
            }
            if model.quarantined {
                Label("Tool cleanup needs owner inspection", systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.orange)
            }
            if let message = model.message {
                Text(message).font(.caption).foregroundStyle(.secondary)
                    .textSelection(.enabled).lineLimit(5)
            }
            if model.pendingAction != nil {
                VStack(alignment: .leading) {
                    Text("An owner action needs reconciliation.").font(.caption.bold())
                    if let request = model.pendingAction {
                        Text(request.id.string).font(.caption2.monospaced()).textSelection(.enabled)
                    }
                    HStack {
                        Button("Repeat exact action") { Task { await model.repeatPending() } }
                            .disabled(model.acting || !model.connected || model.draining)
                        if model.pendingAction?.body["kind"].string == "command" {
                            Button("Inspect result") { Task { await model.inspectPendingCommand() } }
                                .disabled(model.acting || !model.connected)
                        }
                        Button("Clear reminder…") { clearingReminder = true }
                            .disabled(model.acting)
                    }
                }
            }
            TabView {
                ConversationsView(model: model).tabItem { Label("Conversations", systemImage: "bubble.left.and.bubble.right") }
                TasksView(model: model).tabItem { Label("Tasks", systemImage: "list.bullet") }
                SchedulesView(model: model).tabItem { Label("Schedules", systemImage: "calendar") }
                PermissionsView(model: model).tabItem { Label(model.approvals.isEmpty ? "Approvals" : "Approvals (\(model.approvals.count))", systemImage: "hand.raised") }
            }
            Divider()
            HStack {
                if let date = model.lastRefresh {
                    Text("Updated \(date, style: .time)").font(.caption2).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Quit menu bar app") { NSApplication.shared.terminate(nil) }
                    .keyboardShortcut("q")
            }
            Text("The background host keeps running when this app closes.")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .padding(16)
        .frame(width: 540, height: 620)
        .confirmationDialog("Clear this app's replay reminder?", isPresented: $clearingReminder, titleVisibility: .visible) {
            Button("I inspected current state; clear reminder") { model.pendingAction = nil }
                .disabled(model.acting)
        } message: {
            Text("The host retains admitted work and durable command history. This app will stop offering to repeat this action. Inspect the task, permission request or settings before continuing.")
        }
        .sheet(item: $model.selectedTask) { task in TaskDetailsView(model: model, task: task) }
        .sheet(item: $model.selectedApproval) { approval in ApprovalDetailsView(model: model, approval: approval) }
        .sheet(item: $model.openConversation) { conversation in ConversationTranscriptView(model: model, conversation: conversation) }
    }
}

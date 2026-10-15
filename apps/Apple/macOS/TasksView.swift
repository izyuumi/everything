import SwiftUI

@MainActor
struct TasksView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading) {
            Text("Up to 10 most recent tasks").font(.caption).foregroundStyle(.secondary)
            if model.tasks.isEmpty {
                ContentUnavailableView("No tasks loaded", systemImage: "list.bullet.rectangle", description: Text("Connect to the headless host to inspect its tasks."))
            } else {
                List(model.tasks) { task in
                    Button { Task { await model.inspect(task) } } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(task.instruction).lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
                            Text(task.status.replacingOccurrences(of: "_", with: " ").capitalized)
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Inspect task: \(task.instruction.prefix(120))")
                }
            }
        }
        .padding(.top, 8)
    }
}

@MainActor
struct TaskDetailsView: View {
    @Bindable var model: HostModel
    let task: HostTask
    @Environment(\.dismiss) private var dismiss
    @State private var confirmingCancel = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Task").font(.title2)
            Text(task.id.string).font(.caption.monospaced()).textSelection(.enabled)
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text(task.instruction).textSelection(.enabled)
                    Text(model.taskDetails.formatted).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }.frame(maxWidth: .infinity, alignment: .leading)
            }
            if model.taskDetails["kind"].string == "legacy_foreground" {
                Text("This task belongs to the foreground CLI. Stop the host explicitly and settle that task through the CLI before switching ownership.")
                    .font(.caption).foregroundStyle(.secondary)
            } else if !["completed", "failed", "cancelled"].contains(model.taskDetails["task"]["status"].string ?? task.status) {
                HStack {
                    Button("Wake at saved boundary") { Task { await model.wake(task) } }
                    Button("Cancel task…", role: .destructive) { confirmingCancel = true }
                }.disabled(!model.canMutate)
                Text("Wake grants no uncertain provider retry or tool-cleanup permission.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
        }
        .padding(20).frame(width: 620, height: 520)
        .task(id: model.lastRefresh) { await model.inspect(task) }
        .confirmationDialog("Cancel this task?", isPresented: $confirmingCancel, titleVisibility: .visible) {
            Button("Cancel task", role: .destructive) { Task { await model.cancel(task) } }
        } message: {
            Text("The host preserves uncertain native state and tool cleanup requirements.")
        }
    }
}

import SwiftUI

private struct ChatRow: Identifiable {
    let id: UUID
    var role: String
    var text: String
    var provisional: Bool
}

struct ChatView: View {
    @Bindable var model: AppModel
    @State private var draft = ""
    @State private var steering: ReplicaTask?
    @State private var cancelling: ReplicaTask?

    private var rows: [ChatRow] {
        var order: [UUID] = []
        var values: [UUID: ChatRow] = [:]
        for event in model.events where event.conversationID == model.selectedConversation {
            do {
                if event.kind == "message_appended" {
                    let message = try event.payload.field("message")
                    let id = try message.field("id").identifier
                    let content = try message.field("content").elements.compactMap { value -> String? in
                        guard try value.field("kind").text == "text" else { return nil }
                        return try value.field("text").text
                    }.joined(separator: "\n")
                    if values[id] == nil { order.append(id) }
                    values[id] = ChatRow(id: id, role: try message.field("role").text, text: content, provisional: false)
                } else if event.kind == "assistant_text_delta" {
                    let id = try event.payload.field("message_id").identifier
                    if values[id] == nil { order.append(id); values[id] = ChatRow(id: id, role: "assistant", text: "", provisional: true) }
                    if values[id]?.provisional == true { values[id]?.text += try event.payload.field("text").text }
                } else if event.kind == "task_status_changed" {
                    let status = try event.payload.field("status").text
                    if ["failed", "cancelled"].contains(status) {
                        var text = "Task " + status
                        if let failure = try event.payload.fields["failure"], failure != .null {
                            text += ": " + (try failure.field("message").text)
                        }
                        order.append(event.id)
                        values[event.id] = ChatRow(id: event.id, role: "Task status", text: text, provisional: false)
                    }
                } else if event.kind == "subagent_finished" {
                    let unknown = try event.payload.field("outcome_unknown").flag
                    let output = try event.payload.field("output").text
                    order.append(event.id)
                    values[event.id] = ChatRow(id: event.id, role: unknown ? "Subagent outcome unknown" : "Subagent finished", text: output, provisional: false)
                } else if event.kind == "scheduled_run_finished" {
                    let notification = try WireScheduledRunNotification(event.payload.field("notification"))
                    var text = notification.summary
                    if notification.outcome == "unknown" { text += "\nThe host outcome requires reconciliation." }
                    text += "\nJob " + notification.jobID.uuidString.lowercased()
                    text += "\nOccurrence " + notification.occurrenceID.uuidString.lowercased() + " · generation " + String(notification.generation)
                    text += "\nScheduled: " + Date(timeIntervalSince1970: Double(notification.scheduledFor) / 1_000).formatted(date: .abbreviated, time: .standard)
                    text += "\nFinished: " + Date(timeIntervalSince1970: Double(notification.finishedAt) / 1_000).formatted(date: .abbreviated, time: .standard)
                    text += "\nOutcome digest " + notification.outcomeDigest
                    if let task = notification.taskID, let conversation = notification.executionConversationID {
                        text += "\nTask " + task.uuidString.lowercased() + "\nExecution conversation " + conversation.uuidString.lowercased()
                    }
                    order.append(event.id)
                    values[event.id] = ChatRow(id: event.id, role: "Scheduled run · " + notification.outcome, text: text, provisional: false)
                } else if event.kind == "steering_received" {
                    order.append(event.id)
                    values[event.id] = ChatRow(id: event.id, role: "Steering received", text: try event.payload.field("instruction").text, provisional: false)
                }
            } catch {
                return [ChatRow(id: event.id, role: "History unavailable", text: "A stored event could not be rendered. Synchronize and inspect the host.", provisional: false)]
            }
        }
        return order.compactMap { values[$0] }
    }

    private func commandStatus(_ value: String) -> String {
        switch value {
        case "queued": "Queued locally"
        case "relayed": "Stored by relay; awaiting host result"
        case "delivery_expired_outcome_unknown": "Delivery expired; host outcome unknown"
        case "rejected": "Rejected by host"
        default: "Command status requires host inspection"
        }
    }

    private func commandText(_ command: ReplicaPendingCommand) -> String {
        do {
            let payload = try command.original.field("payload")
            switch try payload.field("kind").text {
            case "send_message":
                return try payload.field("message").field("content").elements.compactMap { item -> String? in
                    guard try item.field("kind").text == "text" else { return nil }
                    return try item.field("text").text
                }.joined(separator: "\n")
            case "steer_task": return "Steering: " + (try payload.field("instruction").text)
            case "cancel_task": return "Task cancellation"
            case "resolve_permission": return "Permission decision: " + (try payload.field("decision").text)
            default: return "Stored command"
            }
        } catch { return "The stored command could not be rendered. Inspect the host." }
    }

    private func resultText(_ value: WireValue) -> String {
        do {
            guard let text = String(data: try value.encoded(), encoding: .utf8) else { return "The host result could not be rendered." }
            return text
        } catch { return "The host result could not be rendered. Inspect the host." }
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Conversation", selection: $model.selectedConversation) {
                ForEach(model.state?.snapshot.conversations ?? []) { conversation in
                    Text(conversation.title.isEmpty ? conversation.id.uuidString.lowercased() : conversation.title).tag(Optional(conversation.id))
                }
            }.padding(.horizontal)
            List {
                Section {
                    ForEach(rows) { row in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(row.role.capitalized + (row.provisional ? " · streaming" : "")).font(.caption).foregroundStyle(.secondary)
                            Text(row.text).textSelection(.enabled)
                        }.accessibilityElement(children: .combine)
                    }
                }
                ForEach((model.state?.snapshot.commands ?? []).filter { command in
                    command.conversationID == model.selectedConversation && command.state != "completed" && !model.events.contains(where: { $0.causationID == command.id && $0.kind == "message_appended" })
                }) { command in
                    VStack(alignment: .leading) {
                        Text(commandStatus(command.state)).font(.caption)
                        Text(commandText(command)).textSelection(.enabled)
                        Text(command.id.uuidString.lowercased()).font(.caption2.monospaced())
                        if command.state == "rejected", let result = command.result { Text(resultText(result)).font(.caption) }
                    }.foregroundStyle(.secondary)
                }
                ForEach((model.state?.snapshot.tasks ?? []).filter { $0.conversationID == model.selectedConversation }) { task in
                    Section("Task · " + task.status.replacingOccurrences(of: "_", with: " ")) {
                        Text(task.instruction).lineLimit(4)
                        HStack {
                            Button("Steer") { steering = task }
                            Button("Cancel task", role: .destructive) { cancelling = task }
                        }.disabled(model.busy)
                    }
                }
            }
            .refreshable { await model.synchronize() }
            HStack(alignment: .bottom) {
                TextField("Message your host", text: $draft, axis: .vertical).lineLimit(1...6).textFieldStyle(.roundedBorder)
                Button("Send", systemImage: "arrow.up.circle.fill") {
                    let text = draft
                    Task { if await model.send(text) { draft = "" } }
                }.labelStyle(.iconOnly).font(.title2)
                    .disabled(model.busy || draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || draft.utf8.count > 32_768)
            }.padding()
            Text(model.state?.connection ?? "Disconnected").font(.caption).foregroundStyle(.secondary)
        }
        .onChange(of: model.selectedConversation) { _, _ in Task { await model.refresh() } }
        .sheet(item: $steering) { task in NavigationStack { SteeringView(model: model, task: task) }.modifier(InactivePrivacyCover(enabled: model.privacyCoverEnabled)) }
        .confirmationDialog("Cancel this task?", isPresented: Binding(get: { cancelling != nil }, set: { if !$0 { cancelling = nil } })) {
            if let cancelling { Button("Cancel task", role: .destructive) { Task { await model.cancel(cancelling); self.cancelling = nil } } }
            Button("Keep running", role: .cancel) { cancelling = nil }
        } message: { Text("The host preserves uncertain operations and may require owner reconciliation.") }
    }
}

private struct SteeringView: View {
    let model: AppModel
    let task: ReplicaTask
    @Environment(\.dismiss) private var dismiss
    @State private var instruction = ""

    var body: some View {
        Form {
            TextField("Steering instruction", text: $instruction, axis: .vertical).lineLimit(4...12)
            Text("Steering applies to the host's current active command. It preserves its authority and budgets.").font(.footnote)
            Button("Send steering") { Task { if await model.steer(task, instruction: instruction) { dismiss() } } }
                .disabled(model.busy || instruction.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || instruction.utf8.count > 32_768)
        }
        .navigationTitle("Steer task")
        .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } } }
    }
}

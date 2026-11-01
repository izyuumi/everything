import SwiftUI

@MainActor
struct ConversationsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading) {
            HStack {
                Text("Up to 100 conversations").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("New Conversation", systemImage: "square.and.pencil") { Task { await model.newConversation() } }
                    .disabled(!model.canMutate)
            }
            if model.conversations.isEmpty {
                ContentUnavailableView("No conversations loaded", systemImage: "bubble.left.and.bubble.right", description: Text("Connect to the headless host to read its conversations, or start a new one."))
            } else {
                List(model.conversations) { conversation in
                    Button { model.open(conversation) } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(conversation.title.isEmpty ? "Untitled conversation" : conversation.title)
                                .lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
                            Text("\(conversation.messageCount) messages • \(conversation.provider) \(conversation.model)")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Read conversation: \(conversation.title.prefix(120))")
                }
            }
        }
        .padding(.top, 8)
    }
}

@MainActor
struct ConversationTranscriptView: View {
    @Bindable var model: HostModel
    let conversation: HostConversation
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(conversation.title.isEmpty ? "Untitled conversation" : conversation.title).font(.title2).lineLimit(2)
            Text(conversation.id.string).font(.caption.monospaced()).textSelection(.enabled)
            if model.transcriptTrimmed {
                Text("Showing the most recent 2000 entries.").font(.caption).foregroundStyle(.secondary)
            }
            if let error = model.transcriptError {
                Text(error).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(model.transcript) { entry in
                            TranscriptEntryView(entry: entry).id(entry.id)
                        }
                    }.frame(maxWidth: .infinity, alignment: .leading)
                }
                .overlay {
                    if model.transcript.isEmpty {
                        if model.transcriptLoading { ProgressView() }
                        else if model.transcriptError == nil { ContentUnavailableView("No messages", systemImage: "bubble.left") }
                    }
                }
                .onChange(of: model.transcript.last?.id) { _, last in
                    if let last { proxy.scrollTo(last, anchor: .bottom) }
                }
            }
            HStack {
                Button("Refresh") { Task { await model.loadTranscript(conversation) } }.disabled(model.transcriptLoading)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(20).frame(width: 660, height: 600)
        .task(id: model.lastRefresh) { await model.loadTranscript(conversation) }
    }
}

struct TranscriptEntryView: View {
    let entry: TranscriptEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Label(heading, systemImage: symbol).font(.caption.bold()).foregroundStyle(tint)
                Spacer()
                Text(entry.date, style: .time).font(.caption2).foregroundStyle(.secondary)
            }
            Text(entry.text)
                .font(entry.kind == .text ? .body : .system(.caption, design: .monospaced))
                .lineLimit(entry.kind == .text ? nil : 12)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(8)
        .background(tint.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
    }

    private var heading: String {
        switch entry.kind {
        case .text: entry.role == "user" ? "You" : "Assistant"
        case .toolCall: "Tool call: " + (entry.tool ?? "unknown")
        case .toolResult: "Tool result"
        case .toolError: "Tool error"
        }
    }

    private var symbol: String {
        switch entry.kind {
        case .text: entry.role == "user" ? "person" : "sparkles"
        case .toolCall: "wrench.and.screwdriver"
        case .toolResult: "checkmark.circle"
        case .toolError: "exclamationmark.triangle"
        }
    }

    private var tint: Color {
        switch entry.kind {
        case .text: entry.role == "user" ? .blue : .purple
        case .toolCall, .toolResult: .secondary
        case .toolError: .orange
        }
    }
}

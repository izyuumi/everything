import SwiftUI

@MainActor
struct ConversationsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading) {
            HStack {
                Text("Up to 100 conversations").font(.caption).foregroundStyle(.secondary)
                Spacer()
                if let agents = model.agents {
                    Picker("Agent", selection: $model.newChatAgent) {
                        Text("Default (" + DisplayText.escaped(agents.name(nil) ?? "default") + ")").tag(OwnerID?.none)
                        ForEach(agents.agents) { agent in Text(DisplayText.escaped(agent.name)).tag(Optional(agent.id)) }
                    }
                    .frame(maxWidth: 200)
                    .help("Agent for the next new conversation")
                }
                Button("New Conversation", systemImage: "square.and.pencil") { Task { await model.newConversation() } }
                    .disabled(!model.canMutate && model.pendingAction == nil)
            }
            if model.conversations.isEmpty {
                ContentUnavailableView("No conversations loaded", systemImage: "bubble.left.and.bubble.right", description: Text("Connect to the headless host to read its conversations, or start a new one."))
            } else {
                List(model.conversations) { conversation in
                    Button { model.open(conversation) } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(conversation.title.isEmpty ? "Untitled conversation" : conversation.title)
                                .lineLimit(2).frame(maxWidth: .infinity, alignment: .leading)
                            Text("\(conversation.messageCount) messages • \(conversation.provider) \(conversation.model)" + agentLabel(conversation))
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

    private func agentLabel(_ conversation: HostConversation) -> String {
        guard let name = model.agents?.name(conversation.agentID) else { return conversation.agentID.map { " • agent " + $0.string } ?? "" }
        return " • " + DisplayText.escaped(name)
    }
}

@MainActor
struct AgentsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading) {
            HStack {
                Text("Up to 64 agents").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("New Agent", systemImage: "plus") { model.creatingAgent = true }
                    .disabled(!model.canMutate || model.agents == nil)
            }
            if let error = model.agentsError {
                Text(error).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
            if let agents = model.agents {
                List(agents.agents) { agent in
                    Button { model.editingAgent = agent } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(DisplayText.escaped(agent.name)).bold()
                                if agent.id == agents.defaultID {
                                    Text("Default").font(.caption2).padding(.horizontal, 6).background(.quaternary, in: Capsule())
                                }
                            }
                            Text(DisplayText.escaped(agent.description)).font(.caption).lineLimit(2)
                            Text([agent.model.map { DisplayText.escaped($0) } ?? "Host model", DisplayText.escaped(agent.tools), DisplayText.escaped(agent.skills), agent.memory == "shared" ? "Shared memory" : "Memory off"].joined(separator: " • "))
                                .font(.caption2).foregroundStyle(.secondary).lineLimit(2)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("Edit agent " + DisplayText.escaped(agent.name))
                }
            } else {
                ContentUnavailableView("No agents loaded", systemImage: "person.2", description: Text("Connect to the headless host to read its agents."))
            }
        }
        .padding(.top, 8)
    }
}

@MainActor
struct AgentEditorView: View {
    @Bindable var model: HostModel
    let agent: HostAgent?
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var description = ""
    @State private var persona = ""
    @State private var toolMode = "off"
    @State private var memory = "shared"
    @State private var skills = "all"
    @State private var removing = false

    private var isDefault: Bool { agent != nil && agent?.id == model.agents?.defaultID }
    private var validName: Bool {
        name.utf8.count <= 32 && name.split(separator: "-", omittingEmptySubsequences: false).allSatisfy { part in
            !part.isEmpty && part.unicodeScalars.allSatisfy { ("a"..."z").contains($0) || ("0"..."9").contains($0) }
        }
    }
    private var validDescription: Bool {
        !description.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && description.utf8.count <= 1024
            && !description.unicodeScalars.contains { $0.properties.generalCategory == .control }
    }
    private var validPersona: Bool {
        (persona.isEmpty || !persona.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) && persona.utf8.count <= 32768
            && !persona.unicodeScalars.contains { $0.properties.generalCategory == .control && $0 != "\n" && $0 != "\t" }
    }
    private var nameTaken: Bool {
        model.agents?.agents.contains { $0.id != agent?.id && $0.name.lowercased() == name.lowercased() } ?? false
    }
    private var toolsNote: String? {
        let hadAllowlist = agent?.definition["tools"]["on"]["allow"]["only"].array != nil
        let wasOff = agent.map { $0.toolMode == "off" } ?? true
        if toolMode == "off" && hadAllowlist { return "Turning tools off drops this agent's tool allowlist. Turning them on again later allows all tools." }
        if toolMode != "off" && wasOff { return "Tools on allows all tools up to this ceiling. Restrict them with vorra agents edit --allow-tool." }
        return nil
    }
    private var keepsSkillList: Bool { agent?.definition["skills"]["only"].array != nil }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(agent.map { "Agent " + DisplayText.escaped($0.name) } ?? "New agent").font(.title2)
            if let agent { Text(agent.id.string + " • revision \(agent.revision)").font(.caption.monospaced()).textSelection(.enabled) }
            Form {
                TextField("Name", text: $name, prompt: Text("lowercase-name"))
                if !name.isEmpty && !validName {
                    Text("Use 1 to 32 lowercase letters, digits and single hyphens.").font(.caption).foregroundStyle(.red)
                } else if nameTaken {
                    Text("Another agent already uses this name.").font(.caption).foregroundStyle(.red)
                }
                TextField("Description", text: $description)
                LabeledContent("Persona") {
                    TextEditor(text: $persona).font(.body).frame(height: 90)
                }
                Text(persona.isEmpty ? "An empty persona uses the Host instructions." : "\(persona.utf8.count) of 32768 bytes")
                    .font(.caption).foregroundStyle(validPersona ? Color.secondary : Color.red)
                Picker("Tool ceiling", selection: $toolMode) {
                    ForEach(HostAgent.modes, id: \.self) { mode in Text(mode == "off" ? "Off" : JSONSummary.label(mode)).tag(mode) }
                }
                if let toolsNote {
                    Text(toolsNote).font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
                }
                Picker("Memory", selection: $memory) {
                    Text("Shared").tag("shared")
                    Text("Off").tag("off")
                }
                Picker("Skills", selection: $skills) {
                    Text("None").tag("none")
                    Text("All").tag("all")
                    if keepsSkillList { Text("Keep current list").tag("only") }
                }
                if let agent {
                    LabeledContent("Model", value: agent.model.map { DisplayText.escaped($0) } ?? "Host model")
                    LabeledContent("Tools", value: DisplayText.escaped(agent.tools))
                    LabeledContent("Skills", value: DisplayText.escaped(agent.skills))
                }
            }
            Text("Edit the model, tool allowlist, traits, workspace and messaging with vorra agents edit.")
                .font(.caption).foregroundStyle(.secondary)
            HStack {
                if let agent {
                    Button("New Chat") { Task { dismiss(); await model.newConversation(agent: agent) } }
                        .disabled(!model.canMutate && model.pendingAction == nil)
                    Button("Make Default") { Task { await model.setDefaultAgent(agent) } }
                        .disabled(!model.canMutate || isDefault)
                    Button("Remove…", role: .destructive) { removing = true }
                        .disabled(!model.canMutate || isDefault)
                }
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Save") { Task { await model.saveAgent(definition, expectedRevision: agent?.revision) } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!model.canMutate || !validName || nameTaken || !validDescription || !validPersona)
            }
        }
        .padding(20).frame(width: 520)
        .onAppear(perform: load)
        .confirmationDialog("Remove this agent?", isPresented: $removing, titleVisibility: .visible) {
            Button("Remove", role: .destructive) { if let agent { Task { await model.removeAgent(agent) } } }
        } message: {
            Text("Conversations bound to it cannot start new turns.")
        }
    }

    private func load() {
        guard let agent else { return }
        name = agent.name
        description = agent.description
        persona = agent.persona ?? ""
        toolMode = agent.toolMode
        memory = agent.memory
        skills = agent.definition["skills"].string ?? "only"
    }

    private var definition: JSONValue {
        var value: [String: JSONValue] = ["id": .string(OwnerID().string), "messaging": .string("closed")]
        if case .object(let object) = agent?.definition { value = object }
        value["name"] = .string(name)
        value["description"] = .string(description)
        value["persona"] = persona.isEmpty ? .null : .string(persona)
        let allow = agent?.definition["tools"]["on"]["allow"] ?? .null
        value["tools"] = toolMode == "off" ? .string("off") : .object(["on": .object(["max_mode": .string(toolMode), "allow": allow == .null ? .string("all") : allow])])
        value["memory"] = .string(memory)
        value["skills"] = skills == "only" ? (agent?.definition["skills"] ?? .string("none")) : .string(skills)
        return .object(value)
    }
}

@MainActor
struct SkillsView: View {
    @Bindable var model: HostModel

    var body: some View {
        VStack(alignment: .leading) {
            HStack {
                Text("Skills on the Host for its workspace").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Button("Refresh", systemImage: "arrow.clockwise") { Task { await model.loadSkills() } }
                    .disabled(model.skillsLoading || !model.connected)
            }
            if let error = model.skillsError {
                Text(error).font(.caption).foregroundStyle(.red).textSelection(.enabled)
            }
            if let skills = model.skills {
                ForEach(skills.warnings.prefix(3), id: \.self) { warning in
                    Text(DisplayText.escaped(warning)).font(.caption).foregroundStyle(.orange).lineLimit(2)
                }
                if skills.omitted > 0 { Text("\(skills.omitted) more skills were omitted.").font(.caption).foregroundStyle(.orange) }
                if skills.skills.isEmpty {
                    ContentUnavailableView("No skills found", systemImage: "book.closed", description: Text("Install skills with vorra skills add."))
                } else {
                    List(skills.skills) { skill in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Text(DisplayText.escaped(skill.name)).bold().lineLimit(2)
                                Text([skill.root, skill.status, skill.shadowed ? "shadowed" : nil, skill.offered ? nil : "not offered"].compactMap { $0 }.joined(separator: " • "))
                                    .font(.caption2).foregroundStyle(.secondary)
                            }
                            if let description = skill.description { Text(DisplayText.escaped(description)).font(.caption).lineLimit(3) }
                            if let source = skill.source { Text(DisplayText.escaped(source)).font(.caption2).foregroundStyle(.secondary).lineLimit(1) }
                            if let tools = skill.allowedTools {
                                Text("allowed-tools (never applied): " + DisplayText.escaped(tools)).font(.caption2).foregroundStyle(.secondary).lineLimit(2)
                            }
                            ForEach(skill.issues, id: \.self) { issue in
                                Text(DisplayText.escaped(issue)).font(.caption2).foregroundStyle(.orange).lineLimit(2)
                            }
                        }
                        .accessibilityElement(children: .combine)
                    }
                }
            } else if model.skillsLoading {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView("No skills loaded", systemImage: "book.closed", description: Text("Connect to the headless host to list its skills."))
            }
            Text("Install, update and remove skills with vorra skills. Assign them in the agent editor or with vorra agents edit.")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .padding(.top, 8)
        .task(id: model.connectionKey) { if model.skills == nil { await model.loadSkills() } }
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

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
    struct Trait: Identifiable {
        let id = UUID()
        var key: String
        var value: String
    }

    @Bindable var model: HostModel
    let agent: HostAgent?
    @Environment(\.dismiss) private var dismiss
    @State private var newID = OwnerID()
    @State private var name = ""
    @State private var description = ""
    @State private var persona = ""
    @State private var provider = ""
    @State private var modelName = ""
    @State private var endpoint = ""
    @State private var toolMode = "off"
    @State private var allTools = true
    @State private var toolNames: [String] = []
    @State private var allowedTools: Set<String> = []
    @State private var newTool = ""
    @State private var traits: [Trait] = []
    @State private var workspace = ""
    @State private var messaging: Set<OwnerID> = []
    @State private var memory = "shared"
    @State private var skills = "all"
    @State private var removing = false

    private var ownID: OwnerID { agent?.id ?? newID }
    private var isDefault: Bool { agent != nil && agent?.id == model.agents?.defaultID }
    private var validName: Bool { HostAgent.handle(name) }
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
    private var modelIssue: String? {
        guard !provider.isEmpty else { return nil }
        if !HostAgent.identifier(modelName) { return "Model names are 1 to 256 bytes without spaces or control characters." }
        if !endpoint.isEmpty && (!(endpoint.hasPrefix("http://") || endpoint.hasPrefix("https://")) || endpoint.utf8.count > 2048) {
            return "Leave the endpoint empty for the official API, or give an http(s) loopback URL such as http://127.0.0.1:8080/."
        }
        return nil
    }
    private var toolIssue: String? {
        guard toolMode != "off", !allTools else { return nil }
        return allowedTools.count > 256 ? "At most 256 tools can be allowed." : nil
    }
    private var newToolValid: Bool {
        let name = newTool.hasPrefix("mcp:") ? String(newTool.dropFirst(4)) : newTool
        return HostAgent.identifier(name) && !toolNames.contains(newTool)
    }
    private var renderedBytes: Int {
        persona.utf8.count + (traits.isEmpty ? 0 : "\n\nAgent traits:".utf8.count + traits.reduce(0) { $0 + "\n- ".utf8.count + $1.key.utf8.count + ": ".utf8.count + $1.value.utf8.count })
    }
    private var traitIssue: String? {
        if traits.count > 16 { return "At most sixteen traits are allowed." }
        if traits.contains(where: { !HostAgent.handle($0.key) }) { return "Trait keys are 1 to 32 lowercase letters, digits and single hyphens." }
        if Set(traits.map(\.key)).count != traits.count { return "Each trait key can appear once." }
        if traits.contains(where: { $0.value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || $0.value.utf8.count > 256 || $0.value.unicodeScalars.contains { $0.properties.generalCategory == .control } }) {
            return "Trait values are 1 to 256 bytes without control characters."
        }
        if renderedBytes > 32768 { return "The persona and traits together exceed 32768 bytes." }
        return nil
    }
    private var workspaceIssue: String? {
        guard !workspace.isEmpty else { return nil }
        return workspace.hasPrefix("/") && workspace.utf8.count <= 4096 && !workspace.utf8.contains(0) ? nil : "The workspace must be an absolute path."
    }
    private var messageTargets: [OwnerID] {
        let others = (model.agents?.agents ?? []).map(\.id).filter { $0 != ownID }
        return others + messaging.filter { !others.contains($0) }.sorted { $0.string < $1.string }
    }
    private var toolsNote: String? {
        let hadAllowlist = agent?.definition["tools"]["on"]["allow"]["only"].array != nil
        if toolMode == "off" && hadAllowlist { return "Turning tools off drops this agent's tool allowlist. Turning them on again later allows all tools." }
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
                Section("Model") {
                    Picker("Provider", selection: $provider) {
                        Text("Host default").tag("")
                        ForEach(HostAgent.providers, id: \.self) { provider in Text(provider == "openai" ? "OpenAI" : "Anthropic").tag(provider) }
                    }
                    if !provider.isEmpty {
                        HStack {
                            TextField("Model", text: $modelName)
                            let known = model.knownModels(provider)
                            Menu("Known") {
                                ForEach(known, id: \.self) { item in Button(DisplayText.escaped(item)) { modelName = item } }
                            }
                            .fixedSize().disabled(known.isEmpty)
                        }
                        TextField("Endpoint", text: $endpoint, prompt: Text("Official API"))
                        if let modelIssue { Text(modelIssue).font(.caption).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true) }
                    } else {
                        Text("Conversations use the Host's default model.").font(.caption).foregroundStyle(.secondary)
                    }
                }
                Section("Tools") {
                    Picker("Tool ceiling", selection: $toolMode) {
                        ForEach(HostAgent.modes, id: \.self) { mode in Text(mode == "off" ? "Off" : JSONSummary.label(mode)).tag(mode) }
                    }
                    if let toolsNote {
                        Text(toolsNote).font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
                    }
                    if toolMode != "off" {
                        Picker("Allowed tools", selection: $allTools) {
                            Text("All tools").tag(true)
                            Text("Only these").tag(false)
                        }
                        if !allTools {
                            ForEach(toolNames, id: \.self) { tool in
                                Toggle(DisplayText.escaped(tool), isOn: Binding(
                                    get: { allowedTools.contains(tool) },
                                    set: { if $0 { allowedTools.insert(tool) } else { allowedTools.remove(tool) } }))
                            }
                            HStack {
                                TextField("Add tool", text: $newTool, prompt: Text("tool_name or mcp:server-id"))
                                Button("Add") {
                                    toolNames.append(newTool)
                                    allowedTools.insert(newTool)
                                    newTool = ""
                                }
                                .disabled(!newToolValid)
                            }
                            Text(allowedTools.isEmpty ? "No tools are allowed." : "The Host does not list its tools here; add names exactly as the Host registers them.")
                                .font(.caption).foregroundStyle(.secondary)
                            if let toolIssue { Text(toolIssue).font(.caption).foregroundStyle(.red) }
                        }
                    }
                }
                Section("Traits") {
                    ForEach($traits) { $item in
                        HStack {
                            TextField("Key", text: $item.key, prompt: Text("tone")).frame(width: 120)
                            TextField("Value", text: $item.value, prompt: Text("terse"))
                            Button("Remove", systemImage: "minus.circle") { traits.removeAll { $0.id == item.id } }.labelStyle(.iconOnly)
                        }
                    }
                    Button("Add Trait", systemImage: "plus") { traits.append(Trait(key: "", value: "")) }
                        .disabled(traits.count >= 16)
                    if let traitIssue { Text(traitIssue).font(.caption).foregroundStyle(.red) }
                }
                Section("Workspace") {
                    TextField("Workspace", text: $workspace, prompt: Text("None (Host workspace)"))
                    if let workspaceIssue { Text(workspaceIssue).font(.caption).foregroundStyle(.red) }
                }
                Section("Messaging") {
                    if messageTargets.isEmpty {
                        Text("No other agents.").font(.caption).foregroundStyle(.secondary)
                    }
                    ForEach(messageTargets) { target in
                        Toggle(model.agents?.agents.first { $0.id == target }.map { DisplayText.escaped($0.name) } ?? "Unknown agent " + target.string, isOn: Binding(
                            get: { messaging.contains(target) },
                            set: { if $0 { messaging.insert(target) } else { messaging.remove(target) } }))
                    }
                    Text(messaging.isEmpty ? "Messaging is closed." : "This agent may message the selected agents.").font(.caption).foregroundStyle(.secondary)
                }
                Section("Memory and skills") {
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
                        LabeledContent("Saved skills", value: DisplayText.escaped(agent.skills))
                    }
                }
            }
            .formStyle(.grouped)
            .frame(minHeight: 240, maxHeight: 460)
            Text("Assign individual skills from the Skills tab. Set max output tokens and temperature with vorra agents edit.")
                .font(.caption).foregroundStyle(.secondary)
            if let message = model.message {
                Text(message).font(.caption).foregroundStyle(.secondary).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            }
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
                    .disabled(!model.canMutate || !validName || nameTaken || !validDescription || !validPersona
                        || modelIssue != nil || toolIssue != nil || traitIssue != nil || workspaceIssue != nil)
            }
        }
        .padding(20).frame(width: 560)
        .onAppear(perform: load)
        .confirmationDialog("Remove this agent?", isPresented: $removing, titleVisibility: .visible) {
            Button("Remove", role: .destructive) { if let agent { Task { await model.removeAgent(agent) } } }
        } message: {
            Text("Conversations bound to it cannot start new turns.")
        }
    }

    private func load() {
        guard let agent else { return }
        let value = agent.definition
        name = agent.name
        description = agent.description
        persona = agent.persona ?? ""
        provider = value["model"]["provider"].string ?? ""
        modelName = value["model"]["model"].string ?? ""
        endpoint = value["model"]["endpoint"]["url"].string ?? ""
        toolMode = agent.toolMode
        let only = value["tools"]["on"]["allow"]["only"].array
        allTools = only == nil
        toolNames = (only ?? []).compactMap { $0["tool"].string ?? $0["mcp_server"].string.map { "mcp:" + $0 } }
        allowedTools = Set(toolNames)
        if case .object(let stored) = value["traits"] {
            traits = stored.keys.sorted().map { Trait(key: $0, value: stored[$0]?.string ?? "") }
        }
        workspace = value["workspace"].string ?? ""
        messaging = Set((value["messaging"]["may"].array ?? []).compactMap { $0.string.flatMap { try? OwnerID($0) } })
        memory = agent.memory
        skills = value["skills"].string ?? "only"
    }

    private var definition: JSONValue {
        var value: [String: JSONValue] = ["id": .string(newID.string)]
        if case .object(let object) = agent?.definition { value = object }
        value["name"] = .string(name)
        value["description"] = .string(description)
        value["persona"] = persona.isEmpty ? .null : .string(persona)
        if provider.isEmpty {
            value["model"] = .null
        } else {
            var stored: [String: JSONValue] = [:]
            if case .object(let object) = agent?.definition["model"] { stored = object }
            stored["provider"] = .string(provider)
            stored["model"] = .string(modelName)
            stored["endpoint"] = endpoint.isEmpty ? .object(["kind": .string("official")]) : .object(["kind": .string("loopback"), "url": .string(endpoint)])
            value["model"] = .object(stored)
        }
        value["traits"] = traits.isEmpty ? nil : .object(Dictionary(traits.map { ($0.key, JSONValue.string($0.value)) }, uniquingKeysWith: { first, _ in first }))
        let kept = toolNames.filter(allowedTools.contains)
        let references: [JSONValue] = HostModel.sortedSet(kept.filter { !$0.hasPrefix("mcp:") }).map { .object(["tool": .string($0)]) }
            + HostModel.sortedSet(kept.filter { $0.hasPrefix("mcp:") }.map { String($0.dropFirst(4)) }).map { .object(["mcp_server": .string($0)]) }
        let allow: JSONValue = allTools ? .string("all") : .object(["only": .array(references)])
        value["tools"] = toolMode == "off" ? .string("off") : .object(["on": .object(["max_mode": .string(toolMode), "allow": allow])])
        value["workspace"] = workspace.isEmpty ? .null : .string(workspace)
        let targets = messaging.filter { $0 != ownID }.map(\.string).sorted()
        value["messaging"] = targets.isEmpty ? .string("closed") : .object(["may": .array(targets.map(JSONValue.string))])
        value["memory"] = .string(memory)
        value["skills"] = skills == "only" ? (agent?.definition["skills"] ?? .string("none")) : .string(skills)
        return .object(value)
    }
}

@MainActor
struct SkillsView: View {
    struct Removal: Identifiable {
        let id = UUID()
        let skill: HostSkill
        let agent: HostAgent
    }

    @Bindable var model: HostModel
    @State private var removal: Removal?

    private func keptSkills(_ removal: Removal) -> String {
        let kept = (model.skills?.skills ?? []).compactMap(\.assignableName).filter { !removal.skill.matches($0) }
        return kept.isEmpty ? "none" : HostModel.sortedSet(kept).map { DisplayText.escaped($0) }.joined(separator: ", ")
    }

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
                            if skill.assignableName != nil, let agents = model.agents {
                                Menu("Assign to…") {
                                    ForEach(agents.agents) { agent in
                                        Toggle(DisplayText.escaped(agent.name), isOn: Binding(get: { agent.skillAssigned(skill) }, set: { assign in
                                            if !assign && agent.definition["skills"] == .string("all") { removal = Removal(skill: skill, agent: agent) }
                                            else { Task { await model.assignSkill(skill, to: agent, assign: assign) } }
                                        }))
                                    }
                                }
                                .fixedSize()
                                .disabled(!model.canMutate)
                                .accessibilityLabel("Assign skill " + DisplayText.escaped(skill.name) + " to an agent")
                            }
                        }
                        .accessibilityElement(children: .contain)
                    }
                }
            } else if model.skillsLoading {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ContentUnavailableView("No skills loaded", systemImage: "book.closed", description: Text("Connect to the headless host to list its skills."))
            }
            Text("Install, update and remove skills with vorra skills. Assign one skill with Assign to…, or choose none or all in the agent editor.")
                .font(.caption2).foregroundStyle(.secondary)
        }
        .padding(.top, 8)
        .confirmationDialog("Change this agent to a skill list?", isPresented: Binding(get: { removal != nil }, set: { if !$0 { removal = nil } }), titleVisibility: .visible, presenting: removal) { removal in
            Button("Use only the listed skills", role: .destructive) { Task { await model.assignSkill(removal.skill, to: removal.agent, assign: false) } }
        } message: { removal in
            Text("Agent " + DisplayText.escaped(removal.agent.name) + " has all skills. Removing " + DisplayText.escaped(removal.skill.name)
                + " changes it to only: " + keptSkills(removal) + ". This skill and every skill not in this list, including project skills of other workspaces and skills installed later, stop being assigned.")
        }
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

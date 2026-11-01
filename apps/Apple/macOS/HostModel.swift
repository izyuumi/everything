import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class HostModel {
    var executable: String
    var stateDirectory: String
    var connectionVersion: UUID
    @ObservationIgnored private var activeConnection: OwnerConnection
    var connected = false
    var draining = false
    var quarantined = false
    var refreshing = false
    var acting = false
    var closing = false
    var message: String?
    var lastRefresh: Date?
    var scheduleEngine: HostScheduleEngine?
    var scheduleJobs: [HostScheduleJob] = []
    var scheduleCursor: OwnerID?
    var schedulesLoading = false
    var schedulesLoaded = false
    var schedulesBrowsing = false
    var schedulesError: String?
    var schedulesUpdated: Date?
    var selectedSchedule: HostScheduleJob?
    var scheduleRuns: [HostScheduleRun] = []
    var scheduleHistoryJob: OwnerID?
    var scheduleHistoryCursor: JSONValue = .null
    var scheduleHistoryLoading = false
    var scheduleHistoryLoaded = false
    var scheduleHistoryBrowsing = false
    var scheduleHistoryError: String?
    @ObservationIgnored private var scheduleGeneration = UUID()
    @ObservationIgnored private var historyGeneration = UUID()
    var tasks: [HostTask] = []
    var conversations: [HostConversation] = []
    var agents: HostAgents?
    var agentsError: String?
    var newChatAgent: OwnerID?
    var editingAgent: HostAgent?
    var creatingAgent = false
    @ObservationIgnored private var agentOperationApplied: Bool?
    var skills: HostSkills?
    var skillsError: String?
    var skillsLoading = false
    var approvals: [PendingApproval] = []
    var approvalsLimited = false
    var approvalLoading = false
    var openConversation: HostConversation?
    var transcript: [TranscriptEntry] = []
    var transcriptCursor: UInt64 = 0
    var transcriptTrimmed = false
    var transcriptLoading = false
    var transcriptError: String?
    var taskConversation: HostConversation?
    var selectedApproval: ApprovalDetails?
    var selectedTask: HostTask?
    var taskDetails: JSONValue = .null
    var settings: JSONValue = .null
    var workspace = ""
    var executionMode = "read_only"
    var toolsEnabled = false
    var contextTokens = "32768"
    var serviceStatus: String?
    var chatGPTStatus: JSONValue = .null
    var chatGPTBusy = false
    var pendingAction: OwnerRequest?
    var pendingActionLabel = ""
    @ObservationIgnored private let client: OwnerIPCClient
    @ObservationIgnored private let defaults = UserDefaults.standard
    @ObservationIgnored let settingsSurfaceID = UUID()
    let thisMacSettings: ThisMacSettingsStore?
    let nativeSettings: NativeSettingsStore
    let settingsNavigation: SettingsAssistantNavigation
    let settingsAssistant: SettingsAssistantController
    let vault: VaultSettingsModel
    @ObservationIgnored private var settingsWindowCovered = false
    @ObservationIgnored private var settingsPresentationCovered = false
    @ObservationIgnored private var surfaceDrain: Task<Void, Never>?
    @ObservationIgnored private var surfaceDrainRequested = false

    init() {
        let executable = UserDefaults.standard.string(forKey: "hostExecutable") ?? ""
        let directory = UserDefaults.standard.string(forKey: "hostStateDirectory") ?? NSHomeDirectory() + "/.vorra"
        let client = OwnerIPCClient()
        let connection = OwnerConnection(executable: executable, stateDirectory: directory)
        let connectionKey = UUID()
        self.client = client
        self.executable = executable
        stateDirectory = directory
        activeConnection = connection
        connectionVersion = connectionKey
        thisMacSettings = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first.map {
            ThisMacSettingsStore(directory: $0.appendingPathComponent("Vorra", isDirectory: true).appendingPathComponent("DevicePreferences", isDirectory: true))
        }
        let nativeSettings = NativeSettingsStore(client: client, connection: connection, connectionKey: connectionKey)
        self.nativeSettings = nativeSettings
        vault = VaultSettingsModel(client: client, connection: connection)
        let navigation = SettingsAssistantNavigation()
        settingsNavigation = navigation
        let assistant = SettingsAssistantController(store: nativeSettings, connectionKey: connectionKey,
            navigation: SettingsAssistantController.Navigation(open: { [weak navigation] section in
                guard let navigation else { return .dismissed }
                return await navigation.open(section)
            }, retire: { [weak navigation] in navigation?.retire() }))
        settingsAssistant = assistant
        nativeSettings.beforeRetire = { [weak assistant] in assistant?.parentWillRetire() }
    }

    var connection: OwnerConnection { activeConnection }
    var connectionKey: UUID { connectionVersion }
    var connectionEdited: Bool { OwnerConnection(executable: executable, stateDirectory: stateDirectory) != activeConnection }
    var status: String { closing ? "Closing app" : connected ? (draining ? "Host draining" : "Host connected") : "Host unavailable" }
    var canMutate: Bool { connected && !draining && !acting && !closing && pendingAction == nil && !connectionEdited }

    func rememberConnection() {
        guard !acting, pendingAction == nil, !closing else { return }
        let selected = OwnerConnection(executable: executable, stateDirectory: stateDirectory)
        do { try selected.validate() }
        catch { message = error.localizedDescription; return }
        let connectionKey = UUID()
        nativeSettings.replaceConnection(selected, connectionKey: connectionKey)
        vault.replaceConnection(selected)
        activeConnection = selected
        connectionVersion = connectionKey
        settingsAssistant.applyConnectionKey(connectionKey)
        defaults.set(executable, forKey: "hostExecutable")
        defaults.set(stateDirectory, forKey: "hostStateDirectory")
        connected = false
        scheduleGeneration = UUID()
        historyGeneration = UUID()
        scheduleEngine = nil
        scheduleJobs = []
        scheduleCursor = nil
        selectedSchedule = nil
        scheduleRuns = []
        scheduleHistoryJob = nil
        scheduleHistoryCursor = .null
        schedulesLoading = false
        scheduleHistoryLoading = false
        schedulesLoaded = false
        scheduleHistoryLoaded = false
        schedulesBrowsing = false
        scheduleHistoryBrowsing = false
        schedulesError = nil
        scheduleHistoryError = nil
        schedulesUpdated = nil
        tasks = []
        conversations = []
        agents = nil
        agentsError = nil
        newChatAgent = nil
        editingAgent = nil
        creatingAgent = false
        skills = nil
        skillsError = nil
        approvals = []
        approvalsLimited = false
        openConversation = nil
        transcript = []
        taskConversation = nil
        settings = .null
        selectedTask = nil
        selectedApproval = nil
        taskDetails = .null
        serviceStatus = nil
        message = nil
    }

    func monitor() async {
        while !Task.isCancelled && !closing {
            await refresh()
            do { try await Task.sleep(for: .seconds(15)) }
            catch { return }
        }
    }

    func refresh() async {
        guard !refreshing, !closing, !connection.executable.isEmpty else { return }
        refreshing = true
        defer { refreshing = false }
        let original = connection
        do {
            let runtime = try await client.request(.query("runtime"), connection: original)
            guard runtime["kind"].string == "runtime",
                  let draining = runtime["state"]["draining"].bool,
                  let quarantined = runtime["state"]["broker_quarantined"].bool else { throw OwnerClientError.invalidResponse }
            let rows = try await client.request(.query("tasks", fields: ["conversation_id": .null, "limit": .integer(10)]), connection: original)
            guard rows["kind"].string == "tasks", let rows = rows["tasks"].array, rows.count <= 10 else { throw OwnerClientError.invalidResponse }
            let tasks = try rows.map { try HostTask($0, connection: original) }
            let list = try await client.request(.query("conversations", fields: ["limit": .integer(100)]), connection: original)
            guard list["kind"].string == "conversations", let list = list["conversations"].array, list.count <= 100 else { throw OwnerClientError.invalidResponse }
            let conversations = try list.map(HostConversation.init)
            let agents: Result<HostAgents, Error>
            do { agents = .success(try HostAgents(await client.request(.query("agents"), connection: original))) }
            catch { agents = .failure(error) }
            guard !Task.isCancelled, original == connection, !closing else { return }
            switch agents {
            case .success(let agents):
                self.agents = agents
                agentsError = nil
                if let selected = newChatAgent, !agents.agents.contains(where: { $0.id == selected }) { newChatAgent = nil }
            case .failure(let error):
                self.agents = nil
                newChatAgent = nil
                agentsError = "Cannot read agents: " + error.localizedDescription
            }
            self.draining = draining
            self.quarantined = quarantined
            self.tasks = tasks
            self.conversations = conversations
            connected = true
            lastRefresh = Date()
        } catch {
            guard original == connection, !Task.isCancelled, !closing else { return }
            connected = false
            message = error.localizedDescription
        }
    }

    func loadSchedules(more: Bool = false, automatic: Bool = false) async {
        guard !closing, !schedulesLoading, !more || (scheduleCursor != nil && scheduleJobs.count < 100) else { return }
        let generation = UUID()
        scheduleGeneration = generation
        schedulesLoading = true
        defer { if scheduleGeneration == generation { schedulesLoading = false } }
        let original = connection
        let version = connectionKey
        let after = more ? scheduleCursor : nil
        do {
            if !more {
                let status = try await client.request(.query("schedule_engine"), connection: original)
                let engine = try HostScheduleEngine(status)
                guard original == connection, version == connectionKey, generation == scheduleGeneration, !closing, !Task.isCancelled else { return }
                scheduleEngine = engine
                if automatic && schedulesBrowsing { return }
            }
            let query: JSONValue = .object(["kind": .string("list"), "after_id": after.map { .string($0.string) } ?? .null, "enabled": .null, "include_deleted": .bool(true), "limit": .integer(10)])
            let response = try await client.request(.query("schedule", fields: ["query": query]), connection: original)
            let page = try HostSchedulePage(response, connection: original, after: after)
            guard original == connection, version == connectionKey, generation == scheduleGeneration, !closing, !Task.isCancelled else { return }
            let previous = more ? scheduleJobs : []
            guard previous.count + page.jobs.count <= 100,
                  Set((previous + page.jobs).map(\.id)).count == previous.count + page.jobs.count else { throw OwnerClientError.invalidResponse }
            scheduleJobs = previous + page.jobs
            scheduleCursor = page.next
            schedulesLoaded = true
            schedulesBrowsing = more
            schedulesError = nil
            schedulesUpdated = Date()
        } catch {
            if original == connection, version == connectionKey, generation == scheduleGeneration, !closing, !Task.isCancelled {
                schedulesError = error.localizedDescription
            }
        }
    }

    func selectSchedule(_ job: HostScheduleJob) {
        guard !closing, job.source == connection else { return }
        historyGeneration = UUID()
        scheduleHistoryJob = job.id
        scheduleRuns = []
        scheduleHistoryCursor = .null
        scheduleHistoryLoading = false
        scheduleHistoryLoaded = false
        scheduleHistoryBrowsing = false
        scheduleHistoryError = nil
        selectedSchedule = job
    }

    func loadScheduleHistory(_ job: HostScheduleJob, more: Bool = false, automatic: Bool = false) async {
        guard !closing, job.source == connection, selectedSchedule?.id == job.id,
              !automatic || !scheduleHistoryBrowsing,
              !more || (scheduleHistoryJob == job.id && !scheduleHistoryLoading && scheduleHistoryCursor != .null && scheduleRuns.count < 100) else { return }
        let generation = UUID()
        historyGeneration = generation
        scheduleHistoryLoading = true
        defer { if historyGeneration == generation { scheduleHistoryLoading = false } }
        let original = connection
        let version = connectionKey
        let before = more ? scheduleHistoryCursor : .null
        if !more && scheduleHistoryJob != job.id {
            scheduleHistoryJob = job.id
            scheduleRuns = []
            scheduleHistoryCursor = .null
            scheduleHistoryLoaded = false
            scheduleHistoryError = nil
        }
        do {
            let query: JSONValue = .object(["kind": .string("history"), "job_id": .string(job.id.string), "before": before, "limit": .integer(10)])
            let response = try await client.request(.query("schedule", fields: ["query": query]), connection: original)
            let page = try HostScheduleHistory(response, job: job.id, before: before)
            guard original == connection, version == connectionKey, generation == historyGeneration,
                  selectedSchedule?.id == job.id, !closing, !Task.isCancelled else { return }
            let previous = more ? scheduleRuns : []
            guard scheduleHistoryJob == job.id, previous.allSatisfy({ $0.jobID == job.id }),
                  previous.count + page.runs.count <= 100,
                  Set((previous + page.runs).map(\.id)).count == previous.count + page.runs.count else { throw OwnerClientError.invalidResponse }
            scheduleRuns = previous + page.runs
            scheduleHistoryCursor = page.next
            scheduleHistoryLoaded = true
            scheduleHistoryBrowsing = more
            scheduleHistoryError = nil
        } catch {
            if original == connection, version == connectionKey, generation == historyGeneration,
               selectedSchedule?.id == job.id, !closing, !Task.isCancelled { scheduleHistoryError = error.localizedDescription }
        }
    }

    func loadApprovals() async {
        guard !approvalLoading, !closing else { return }
        approvalLoading = true
        defer { approvalLoading = false }
        let original = connection
        var collected: [PendingApproval] = []
        var limited = false
        do {
            search: for conversation in conversations {
                var cursor: JSONValue = .null
                repeat {
                    let result = try await client.request(.query("approvals", fields: ["conversation_id": .string(conversation.id.string), "limit": .integer(100), "cursor": cursor]), connection: original)
                    guard result["kind"].string == "approvals", let rows = result["approvals"].array,
                          rows.count <= 100, try result.encoded().count <= 256 * 1024 else { throw OwnerClientError.invalidResponse }
                    collected += try rows.map { try PendingApproval($0, conversation: conversation.id, connection: original) }
                    guard original == connection, !closing, !Task.isCancelled else { return }
                    if collected.count >= 500 { limited = true; break search }
                    cursor = result["next_cursor"]
                } while cursor != .null
            }
            guard Set(collected.map(\.id)).count == collected.count else { throw OwnerClientError.invalidResponse }
            approvals = collected.sorted { $0.requestedAt < $1.requestedAt }
            approvalsLimited = limited
        } catch {
            if original == connection, !closing, !Task.isCancelled { message = error.localizedDescription }
        }
    }

    func open(_ conversation: HostConversation) {
        guard !closing else { return }
        transcript = []
        transcriptCursor = 0
        transcriptTrimmed = false
        transcriptError = nil
        openConversation = conversation
    }

    func loadTranscript(_ conversation: HostConversation) async {
        guard !closing, !transcriptLoading, openConversation?.id == conversation.id else { return }
        transcriptLoading = true
        defer { transcriptLoading = false }
        let original = connection
        do {
            let head = try await client.request(.query("conversation", fields: ["conversation_id": .string(conversation.id.string)]), connection: original)
            guard head["kind"].string == "conversation", head["information"]["id"].string == conversation.id.string,
                  let through = head["information"]["last_sequence"].unsigned else { throw OwnerClientError.invalidResponse }
            while transcriptCursor < through {
                let after = transcriptCursor
                let response = try await client.request(.synchronize(conversation: conversation.id, after: after), connection: original)
                let page = try TranscriptPage(response, conversation: conversation.id, after: after)
                guard original == connection, openConversation?.id == conversation.id, transcriptCursor == after,
                      !closing, !Task.isCancelled else { return }
                guard page.through > after else { throw OwnerClientError.invalidResponse }
                transcript += page.entries
                if transcript.count > 2000 {
                    transcript.removeFirst(transcript.count - 2000)
                    transcriptTrimmed = true
                }
                transcriptCursor = page.through
            }
            transcriptError = nil
        } catch {
            if original == connection, openConversation?.id == conversation.id, !closing, !Task.isCancelled { transcriptError = error.localizedDescription }
        }
    }

    func inspect(_ approval: PendingApproval) async {
        guard approval.source == connection else { return }
        let original = connection
        do {
            let result = try await client.request(.query("approval", fields: ["conversation_id": .string(approval.conversationID.string), "lookup": .object(["by": .string("approval"), "id": .string(approval.id)])]), connection: original)
            guard result["kind"].string == "approval", result["approval"] != .null else { throw OwnerClientError.invalidResponse }
            if original == connection, !closing { selectedApproval = try ApprovalDetails(summary: approval, value: result["approval"]) }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
    }

    func inspect(_ task: HostTask) async {
        guard task.source == connection else { return }
        let original = connection
        do {
            let result = try await client.request(.query("task", fields: ["task_id": .string(task.id.string)]), connection: original)
            guard ["task", "legacy_foreground"].contains(result["kind"].string ?? ""),
                  result["task"]["id"].string == task.id.string,
                  result["task"]["conversation_id"].string == task.conversationID.string else { throw OwnerClientError.invalidResponse }
            let head = try await client.request(.query("conversation", fields: ["conversation_id": .string(task.conversationID.string)]), connection: original)
            guard head["kind"].string == "conversation" else { throw OwnerClientError.invalidResponse }
            let conversation = try HostConversation(head["information"])
            guard conversation.id == task.conversationID else { throw OwnerClientError.invalidResponse }
            if original == connection, !closing { taskDetails = result; taskConversation = conversation; selectedTask = task }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
    }

    func loadSkills() async {
        guard !closing, !skillsLoading, !connection.executable.isEmpty else { return }
        skillsLoading = true
        defer { skillsLoading = false }
        let original = connection
        do {
            let result = try HostSkills(await client.request(.query("skills"), connection: original))
            guard original == connection, !closing, !Task.isCancelled else { return }
            skills = result
            skillsError = nil
        } catch {
            if original == connection, !closing, !Task.isCancelled { skillsError = error.localizedDescription }
        }
    }

    func knownModels(_ provider: String) -> [String] {
        let configured = settings["default_conversation"]
        let pairs = [(configured["provider"].string, configured["model"].string)]
            + conversations.map { (Optional($0.provider), Optional($0.model)) }
            + (agents?.agents ?? []).map { ($0.definition["model"]["provider"].string, $0.definition["model"]["model"].string) }
        let models = pairs.compactMap { $0.0 == provider ? $0.1 : nil }.filter(HostAgent.identifier)
        return Array(HostModel.sortedSet(models).prefix(64))
    }

    static func sortedSet(_ values: some Sequence<String>) -> [String] {
        Set(values).sorted { $0.utf8.lexicographicallyPrecedes($1.utf8) }
    }

    func assignSkill(_ skill: HostSkill, to agent: HostAgent, assign: Bool) async {
        guard canMutate, let name = skill.assignableName, case .object(var definition) = agent.definition else { return }
        var names: [String] = switch agent.definition["skills"] {
        case .string("all"): (skills?.skills ?? []).compactMap(\.assignableName)
        case .string("none"): []
        default: (agent.definition["skills"]["only"].array ?? []).compactMap(\.string)
        }
        if assign { names.append(name) } else { names.removeAll(where: skill.matches) }
        let sorted = HostModel.sortedSet(names)
        definition["skills"] = sorted.isEmpty ? .string("none") : .object(["only": .array(sorted.map(JSONValue.string))])
        let skillName: String = DisplayText.escaped(name)
        let agentName: String = DisplayText.escaped(agent.name)
        let label: String = assign ? "Skill \(skillName) for \(agentName)" : "Skill removal \(skillName) from \(agentName)"
        await saveAgent(.object(definition), expectedRevision: agent.revision, label: label)
    }

    func saveAgent(_ definition: JSONValue, expectedRevision: UInt64?, label: String? = nil) async {
        guard canMutate else { return }
        let label = label ?? "Agent " + DisplayText.escaped(definition["name"].string ?? "")
        await apply(.operation("agent_put", fields: ["expected_revision": expectedRevision.map(JSONValue.unsigned) ?? .null, "definition": definition]), label: label, expectedKind: "agents")
        if pendingAction == nil && agentOperationApplied != false { editingAgent = nil; creatingAgent = false }
    }

    func removeAgent(_ agent: HostAgent) async {
        guard canMutate else { return }
        await apply(.operation("agent_remove", fields: ["id": .string(agent.id.string), "expected_revision": .unsigned(agent.revision)]), label: "Agent removal", expectedKind: "agents")
        if pendingAction == nil && agentOperationApplied != false { editingAgent = nil }
    }

    func setDefaultAgent(_ agent: HostAgent) async {
        guard canMutate else { return }
        await apply(.operation("agent_set_default", fields: ["id": .string(agent.id.string), "expected_revision": .unsigned(agent.revision)]), label: "Default agent", expectedKind: "agents")
    }

    func newConversation(agent: HostAgent, asDefault: Bool = false) async {
        guard pendingAction == nil else { message = "Resolve the pending action first."; return }
        guard canMutate else { return }
        let original = connection
        let id = OwnerID()
        do {
            let current = try await client.request(.query("agent", fields: ["id": .string(agent.id.string)]), connection: original)
            guard current["kind"].string == "agent", try HostAgent(current["agent"]) == agent,
                  !asDefault || current["default"] == .bool(true) else {
                message = "Agent " + DisplayText.escaped(agent.name) + " changed or was removed. Review it and try again."
                await refresh()
                return
            }
            let settings = try await client.request(.query("settings"), connection: original)
            guard settings["kind"].string == "settings" else { throw OwnerClientError.invalidResponse }
            var configuration = settings["settings"]["default_conversation"]
            let own = agent.definition["model"]
            if configuration == .null, own != .null {
                configuration = .object([
                    "provider": own["provider"], "model": own["model"], "endpoint": own["endpoint"],
                    "instructions": .string("You are Vorra, a helpful assistant."),
                    "max_output_tokens": own["max_output_tokens"] == .null ? .unsigned(4096) : own["max_output_tokens"],
                    "temperature": own["temperature"]
                ])
            }
            guard configuration != .null else {
                message = "Configure a default model in Settings or give the agent a model before starting a conversation."
                return
            }
            guard original == connection, !closing else { return }
            let request = OwnerRequest.initialize(conversation: id, title: "New conversation", configuration: configuration, agent: agent.id)
            await apply(request, label: "New conversation with " + DisplayText.escaped(agent.name), expectedKind: "command")
            if let created = conversations.first(where: { $0.id == id }) { open(created) }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
    }

    func reconcileAgentOperation(_ request: OwnerRequest, note: String? = nil) async {
        guard !acting, !closing, pendingAction?.id == request.id else { return }
        acting = true
        defer { acting = false }
        let original = connection
        do {
            let current = try HostAgents(await client.request(.query("agents"), connection: original))
            guard original == connection, pendingAction?.id == request.id, !closing, !Task.isCancelled else { return }
            let operation = request.body["operation"]
            let id = operation["id"].string ?? operation["definition"]["id"].string
            let agent = current.agents.first { $0.id.string == id }
            let applied: Bool
            switch operation["kind"].string {
            case "agent_put":
                let keys = ["name", "description", "persona", "model", "traits", "tools", "workspace", "memory", "skills", "messaging"]
                applied = agent.map { agent in
                    agent.revision > (operation["expected_revision"].unsigned ?? 0) && keys.allSatisfy { operation["definition"][$0] == agent.definition[$0] }
                } ?? false
            case "agent_remove": applied = agent == nil
            case "agent_set_default": applied = current.defaultID.string == id
            default: applied = false
            }
            agents = current
            agentOperationApplied = applied
            if !applied, let editing = editingAgent, let fresh = current.agents.first(where: { $0.id == editing.id }) { editingAgent = fresh }
            pendingAction = nil
            message = (note.map { $0 + " " } ?? "") + (applied
                ? pendingActionLabel + " is applied in the current Host state."
                : pendingActionLabel + " is not reflected in the current Host state. Review the agent and try again.")
        } catch { if !Task.isCancelled { message = (note.map { $0 + " " } ?? "") + error.localizedDescription } }
    }

    func newConversation() async {
        guard pendingAction == nil else { message = "Resolve the pending action first."; return }
        if let selected = newChatAgent {
            if let agent = agents?.agents.first(where: { $0.id == selected }) { await newConversation(agent: agent) }
            else { message = "The selected agent is no longer available." }
            return
        }
        if let agents, let agent = agents.agents.first(where: { $0.id == agents.defaultID }) {
            await newConversation(agent: agent, asDefault: true)
            return
        }
        let id = OwnerID()
        do {
            let request = try OwnerRequest.command(conversation: id, payload: ["kind": .string("create_conversation"), "title": .string("New conversation")])
            await apply(request, label: "New conversation", expectedKind: "command")
            if let created = conversations.first(where: { $0.id == id }) { open(created) }
        } catch { message = error.localizedDescription }
    }

    func cancel(_ task: HostTask) async {
        guard task.source == connection else { return }
        do {
            let request = try OwnerRequest.command(conversation: task.conversationID, payload: ["kind": .string("cancel_task"), "task_id": .string(task.id.string)])
            await apply(request, label: "Task cancellation", expectedKind: "command")
        } catch { message = error.localizedDescription }
    }

    func decide(_ approval: ApprovalDetails, decision: String) async {
        guard ["allow", "deny", "allow_for_task"].contains(decision), decision != "allow_for_task" || approval.leasable else { return }
        guard canMutate, approval.summary.source == connection else { return }
        let original = connection
        do {
            let result = try await client.request(.query("approval", fields: [
                "conversation_id": .string(approval.summary.conversationID.string),
                "lookup": .object(["by": .string("approval"), "id": .string(approval.id)])
            ]), connection: original)
            let current = try ApprovalDetails(summary: approval.summary, value: result["approval"])
            guard result["kind"].string == "approval", current.pending,
                  current.request == approval.request, original == connection else { throw OwnerClientError.invalidResponse }
            let request = try OwnerRequest.command(conversation: approval.summary.conversationID, payload: [
                "kind": .string("resolve_permission"), "operation_id": .string(approval.summary.operationID.string),
                "decision": .string(decision)
            ])
            let label = switch decision {
            case "deny": "Operation denial"
            case "allow_for_task": "Approval for this task"
            default: "Operation approval"
            }
            await apply(request, label: label, expectedKind: "command")
            await loadApprovals()
        } catch { message = error.localizedDescription }
    }

    func wake(_ task: HostTask) async {
        guard task.source == connection else { return }
        await apply(.operation("wake_task", fields: ["task_id": .string(task.id.string)]), label: "Wake task", expectedKind: "applied")
    }

    func apply(_ request: OwnerRequest, label: String, expectedKind: String) async {
        guard !acting, !closing, connected, !draining, !connectionEdited else { return }
        guard pendingAction == nil || pendingAction?.id == request.id else { return }
        acting = true
        pendingAction = request
        pendingActionLabel = label
        agentOperationApplied = nil
        defer { acting = false }
        let original = connection
        var rejected = false
        do {
            let result = try await client.request(request, connection: original)
            guard original == connection, !closing, !Task.isCancelled else { return }
            var settled = true
            if expectedKind == "settings" {
                settled = try adoptSettingsReceipt(result, request: request)
            } else if expectedKind == "command" {
                guard result["command_id"].string == request.id.string,
                      result["conversation_id"] == request.body["command"]["conversation_id"],
                      let phase = result["phase"].string, ["completed", "rejected"].contains(phase) else { throw OwnerClientError.invalidResponse }
                message = phase == "completed" ? label + " applied." : label + " was rejected. Inspect the task and request."
            } else {
                guard result["kind"].string == expectedKind else { throw OwnerClientError.invalidResponse }
                message = label + " applied."
            }
            if settled { pendingAction = nil }
            await refresh()
        } catch OwnerClientError.hostRejected(let reason) where request.isAgentOperation || request.isCommand {
            let shown = DisplayText.escaped(reason)
            message = label + " was rejected: " + shown + (shown.hasSuffix(".") ? "" : ".")
            rejected = true
        } catch { message = error.localizedDescription }
        guard rejected, original == connection, !closing, !Task.isCancelled else { return }
        acting = false
        if request.isAgentOperation { await reconcileAgentOperation(request, note: message) }
        else { await inspectPendingCommand(note: message) }
    }

    private func adoptSettingsReceipt(_ result: JSONValue, request: OwnerRequest) throws -> Bool {
        guard request.body["operation"]["kind"].string == "replace_settings",
              let expected = request.body["operation"]["expected_revision"].unsigned,
              expected > 0, expected < UInt64(Int64.max) else { throw OwnerClientError.invalidResponse }
        if result["kind"].string == "settings" {
            guard result["settings"]["revision"].unsigned == expected + 1 else { throw OwnerClientError.invalidResponse }
            try adoptSettings(result["settings"])
            message = result["restart_required"].bool == true
                ? "Settings saved. Restart the host to apply configuration changes."
                : "Settings saved."
            return true
        }
        let operation = result["operation"]
        let binding = operation["binding"]
        guard result["kind"].string == "settings_receipt",
              binding["request_id"].string == request.id.string,
              binding["client_session_id"].string == request.id.string,
              binding["expected_revision"].unsigned == expected,
              binding["scope"]["kind"].string == "host",
              let hostID = binding["scope"]["host_id"].string,
              operation["origin"]["kind"].string == "local_owner",
              operation["origin"]["request_id"].string == request.id.string,
              let restartRequired = result["restart_required"].bool else { throw OwnerClientError.invalidResponse }
        _ = try OwnerID(hostID)
        let phase = operation["phase"]
        if phase["phase"].string == "applying" {
            guard phase["expected_revision"].unsigned == expected,
                  phase["revision"].unsigned == expected + 1,
                  result["settings"] == .null, !restartRequired else { throw OwnerClientError.invalidResponse }
            message = "Settings are still being reconciled. The original request remains pending."
            return false
        }
        let outcome = phase["result"]
        guard phase["phase"].string == "finished",
              outcome["contract_version"].unsigned == 1,
              outcome["request_id"].string == request.id.string,
              outcome["scope"] == binding["scope"],
              let state = outcome["state"]["kind"].string else { throw OwnerClientError.invalidResponse }
        switch state {
        case "applied", "pending_application", "restart_required":
            let write = operation["write"]
            guard write["resolution"].string == "applied",
                  write["expected_revision"].unsigned == expected,
                  write["revision"].unsigned == expected + 1,
                  outcome["state"]["revision"].unsigned == expected + 1,
                  write["restart_required"].bool == restartRequired else { throw OwnerClientError.invalidResponse }
            if result["settings"] != .null {
                guard result["settings"]["revision"].unsigned == expected + 1 else { throw OwnerClientError.invalidResponse }
                try adoptSettings(result["settings"])
            }
            if state == "pending_application" {
                switch operation["application"]["state"].string {
                case nil: message = "Settings saved. Application is pending."
                case "applied": message = "Settings saved and applied."
                case "restart_required": message = "Settings saved. Restart the host to apply configuration changes."
                case "failed": message = "Settings saved. Application failed; inspect current state."
                case "unavailable": message = "Settings saved. The requested feature is unavailable."
                default: throw OwnerClientError.invalidResponse
                }
            } else {
                message = restartRequired
                    ? "Settings saved. Restart the host to apply configuration changes."
                    : "Settings saved."
            }
            return true
        case "unknown":
            guard result["settings"] == .null, !restartRequired else { throw OwnerClientError.invalidResponse }
            message = "The settings outcome is uncertain. Inspect or reconcile the original request before another edit."
            return false
        case "host_action_required", "conflict", "unavailable", "failed", "cancelled":
            guard result["settings"] == .null, !restartRequired else { throw OwnerClientError.invalidResponse }
            message = switch state {
            case "host_action_required": "This change requires native confirmation."
            case "conflict": "Settings changed elsewhere. Reload before editing."
            case "unavailable": "The settings operation is unavailable."
            case "cancelled": "The settings operation was cancelled."
            default: "The settings operation failed. Reload current state."
            }
            return true
        default: throw OwnerClientError.invalidResponse
        }
    }

    func repeatPending() async {
        guard let request = pendingAction else { return }
        if request.isAgentOperation {
            await reconcileAgentOperation(request)
            return
        }
        let kind = request.isCommand ? "command" : request.body["operation"]["kind"].string == "replace_settings" ? "settings" : "applied"
        await apply(request, label: pendingActionLabel, expectedKind: kind)
    }

    func inspectPendingCommand(note: String? = nil) async {
        guard let request = pendingAction, request.isCommand, !acting, !closing else { return }
        acting = true
        defer { acting = false }
        let original = connection
        do {
            let response = try await client.request(.query("command", fields: ["command_id": .string(request.id.string)]), connection: original)
            guard original == connection, pendingAction?.id == request.id, !closing, !Task.isCancelled else { return }
            guard response["kind"].string == "command" else { throw OwnerClientError.invalidResponse }
            let receipt = response["receipt"]
            if receipt == .null {
                message = (note.map { $0 + " " } ?? "") + "This host has no durable record of the command. Its exact original request remains available for an explicit retry."
                return
            }
            guard receipt["command_id"].string == request.id.string,
                  receipt["conversation_id"] == request.body["command"]["conversation_id"],
                  let phase = receipt["phase"].string,
                  ["accepted", "applied", "completed", "rejected"].contains(phase) else { throw OwnerClientError.invalidResponse }
            message = (note.map { $0 + " " } ?? "") + "Command state: " + phase + ". " + receipt["result"].formatted
            if ["completed", "rejected"].contains(phase) { pendingAction = nil }
            await refresh()
        } catch { if !Task.isCancelled { message = (note.map { $0 + " " } ?? "") + error.localizedDescription } }
    }

    func loadSettings() async {
        let original = connection
        do {
            let result = try await client.request(.query("settings"), connection: original)
            guard result["kind"].string == "settings" else { throw OwnerClientError.invalidResponse }
            if original == connection, !closing, !Task.isCancelled { try adoptSettings(result["settings"]) }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
        await refreshChatGPT()
    }

    private func adoptSettings(_ value: JSONValue) throws {
        guard value["schema_version"].unsigned == 1, let revision = value["revision"].unsigned,
              revision > 0, revision <= UInt64(Int64.max),
              let tools = value["tools_enabled"].bool, let tokens = value["context_tokens"].unsigned else { throw OwnerClientError.invalidResponse }
        workspace = try value.requiringString("workspace_root")
        executionMode = try value.requiringString("execution_mode", maximum: 32)
        guard ["read_only", "trusted_workspace", "full_host"].contains(executionMode) else { throw OwnerClientError.invalidResponse }
        toolsEnabled = tools
        contextTokens = String(tokens)
        settings = value
    }

    func saveSettings() async {
        guard let revision = settings["revision"].unsigned, let tokens = UInt64(contextTokens),
              revision > 0, tokens > 0, workspace.hasPrefix("/"), workspace.utf8.count <= 4096 else {
            message = "Choose an existing physical workspace and a valid context token limit."
            return
        }
        guard revision < UInt64(Int64.max) else {
            message = "The settings revision limit has been reached. Another edit cannot be accepted."
            return
        }
        do {
            var updated = try settings.replacing("workspace_root", with: .string(workspace))
            updated = try updated.replacing("execution_mode", with: .string(executionMode))
            updated = try updated.replacing("tools_enabled", with: .bool(toolsEnabled))
            updated = try updated.replacing("context_tokens", with: .unsigned(tokens))
            await apply(.operation("replace_settings", fields: ["expected_revision": .unsigned(revision), "settings": updated]), label: "Settings", expectedKind: "settings")
        } catch { message = error.localizedDescription }
    }

    var chatGPTBinding: JSONValue? {
        settings["model_credentials"].array?.first { $0["source"].string == "chatgpt" }
    }

    var chatGPTSelectable: Bool {
        settings["model_credentials"].array?.contains {
            $0["provider"].string == "openai" && $0["endpoint"]["kind"].string == "official"
        } == false
    }

    func refreshChatGPT() async {
        guard let binding = chatGPTBinding else { chatGPTStatus = .null; return }
        let original = connection
        do {
            let status = try await chatGPT(["kind": .string("status"), "binding": binding], connection: original)
            if original == connection, !closing { chatGPTStatus = status }
        } catch { if original == connection, !closing { message = error.localizedDescription } }
    }

    func continueWithChatGPT() async {
        guard canMutate, !chatGPTBusy, let revision = settings["revision"].unsigned else { return }
        let configured = chatGPTBinding
        guard configured != nil || chatGPTSelectable else {
            message = "The official OpenAI endpoint already uses another credential reference. Remove it before using your ChatGPT plan."
            return
        }
        let binding = configured ?? .object([
            "provider": .string("openai"), "endpoint": .object(["kind": .string("official")]),
            "reference": .string(UUID().uuidString.lowercased()), "storage": .string("native"), "source": .string("chatgpt")
        ])
        chatGPTBusy = true
        defer { chatGPTBusy = false }
        let original = connection
        do {
            var status = try await chatGPT(["kind": .string("sign_in"), "binding": binding, "timeout_seconds": .unsigned(600)], connection: original)
            guard let attempt = status["attempt"]["attempt_id"].string,
                  let url = status["attempt"]["authorize_url"].string.flatMap(URL.init(string:)),
                  url.scheme == "https", url.host == "auth.openai.com" else { throw OwnerClientError.invalidResponse }
            NSWorkspace.shared.open(url)
            message = "Continue with ChatGPT in your browser."
            while status["attempt"]["phase"]["kind"].string == "pending" {
                do {
                    status = try await chatGPT(["kind": .string("wait"), "attempt_id": .string(attempt), "timeout_ms": .unsigned(25_000)], connection: original)
                } catch {
                    _ = try? await chatGPT(["kind": .string("cancel"), "attempt_id": .string(attempt)], connection: original)
                    throw error
                }
            }
            guard original == connection, !closing else { return }
            chatGPTStatus = status
            let phase = status["attempt"]["phase"]
            guard phase["kind"].string == "completed" else {
                message = "ChatGPT sign-in did not complete: " + (phase["reason"].string ?? phase["kind"].string ?? "unknown")
                return
            }
            message = status["plan_usage_enabled"].bool == true ? "Using ChatGPT plan." : "Signed in, but ChatGPT plan usage was not granted. API keys remain available."
            if configured == nil, let credentials = settings["model_credentials"].array {
                let updated = try settings.replacing("model_credentials", with: .array(credentials + [binding]))
                await apply(.operation("replace_settings", fields: ["expected_revision": .unsigned(revision), "settings": updated]), label: "ChatGPT plan selection", expectedKind: "settings")
            }
        } catch { if original == connection, !closing { message = error.localizedDescription } }
    }

    func signOutChatGPT() async {
        guard canMutate, !chatGPTBusy, let binding = chatGPTBinding else { return }
        chatGPTBusy = true
        defer { chatGPTBusy = false }
        let original = connection
        do {
            let status = try await chatGPT(["kind": .string("sign_out"), "binding": binding], connection: original)
            guard original == connection, !closing else { return }
            chatGPTStatus = status
            message = status["revocation"].string == "unconfirmed" ? "Signed out locally. OpenAI did not confirm revocation; you can disconnect Vorra in ChatGPT settings." : "Signed out of ChatGPT."
        } catch { if original == connection, !closing { message = error.localizedDescription } }
    }

    func stopUsingChatGPT() async {
        guard canMutate, let revision = settings["revision"].unsigned, let credentials = settings["model_credentials"].array else { return }
        do {
            let updated = try settings.replacing("model_credentials", with: .array(credentials.filter { $0["source"].string != "chatgpt" }))
            await apply(.operation("replace_settings", fields: ["expected_revision": .unsigned(revision), "settings": updated]), label: "API-key credential selection", expectedKind: "settings")
            await refreshChatGPT()
        } catch { message = error.localizedDescription }
    }

    private func chatGPT(_ operation: [String: JSONValue], connection original: OwnerConnection) async throws -> JSONValue {
        let result = try await client.request(.operation("chatgpt", fields: ["operation": .object(operation)]), connection: original)
        guard result["kind"].string == "chatgpt", result["status"]["signed_in"].bool != nil else { throw OwnerClientError.invalidResponse }
        return result["status"]
    }

    func manageService(_ action: String) async {
        guard !acting, !closing, pendingAction == nil, !connectionEdited else { return }
        acting = true
        defer { acting = false }
        let original = connection
        let expected = ["install": "installed", "start": "started", "stop": "stopped", "status": "status", "enable": "enabled", "disable": "enabled"]
        do {
            let result = try await client.service(action, connection: original)
            guard result["kind"].string == expected[action],
                  action != "enable" || result["enabled"].bool == true,
                  action != "disable" || result["enabled"].bool == false else { throw OwnerClientError.invalidResponse }
            guard original == connection, !closing else { return }
            if action != "status" { message = "Service \(action) completed." }
            let status = action == "status" ? result : try await client.service("status", connection: original)
            guard original == connection, !closing else { return }
            serviceStatus = try Self.describeService(status)
        } catch { if original == connection, !closing { message = error.localizedDescription } }
        await refresh()
    }

    private static func describeService(_ result: JSONValue) throws -> String {
        let status = result["status"]
        guard result["kind"].string == "status", let installed = status["installed"].bool,
              let enabled = status["enabled"].bool, let loaded = status["loaded"].bool else { throw OwnerClientError.invalidResponse }
        let pid = status["process_id"]
        let exit = status["last_exit_status"]
        guard pid == .null || pid.unsigned.map({ $0 > 0 && $0 <= UInt64(UInt32.max) }) == true,
              pid == .null || loaded else { throw OwnerClientError.invalidResponse }
        var parts = [installed ? "Installed" : "Not installed", enabled ? "enabled at login" : "disabled at login"]
        if let process = pid.unsigned { parts.append("running as PID \(process)") }
        else { parts.append(loaded ? "loaded, not running" : "not loaded") }
        if case .integer(let code) = exit { parts.append("last exit \(code)") }
        else if exit != .null { throw OwnerClientError.invalidResponse }
        return parts.joined(separator: ", ")
    }

    func settingsWindowAttached(_ window: NSWindow?) {
        settingsNavigation.attachWindow(window)
        settingsAssistant.attachWindow(window)
    }

    func settingsPresentationChanged(covered: Bool) {
        settingsPresentationCovered = covered
        settingsAssistant.setCovered(settingsWindowCovered || settingsPresentationCovered)
        if covered { settingsNavigation.retire() }
    }

    func settingsWindowChanged(visible: Bool, active: Bool, covered: Bool) {
        settingsWindowCovered = covered
        settingsAssistant.setCovered(settingsWindowCovered || settingsPresentationCovered)
        if !visible || !active || covered { settingsNavigation.retire() }
        settingsAssistant.setSurfaceVisible(visible && !closing)
        settingsAssistant.setApplicationActive(active && !closing)
        guard !closing else {
            thisMacSettings?.setSurfaceVisible(false)
            nativeSettings.setSurfaceVisible(false)
            return
        }
        if !visible {
            thisMacSettings?.setSurfaceVisible(false)
            nativeSettings.setSurfaceVisible(false)
        }
        thisMacSettings?.setApplicationActive(active)
        nativeSettings.setApplicationActive(active)
        if visible {
            thisMacSettings?.setSurfaceVisible(true)
            thisMacSettings?.open()
            nativeSettings.setSurfaceVisible(true)
        }
    }

    func settingsWindowClosed() {
        settingsAssistant.setSurfaceVisible(false)
        settingsAssistant.parentWillRetire()
        settingsNavigation.retire()
        settingsPresentationCovered = false
        thisMacSettings?.setSurfaceVisible(false)
        nativeSettings.setSurfaceVisible(false)
        nativeSettings.close()
        guard !closing, let store = thisMacSettings else { return }
        surfaceDrainRequested = true
        guard surfaceDrain == nil else { return }
        surfaceDrain = Task { [self, store] in
            while surfaceDrainRequested {
                surfaceDrainRequested = false
                await store.drainSurface()
            }
            surfaceDrain = nil
        }
    }

    func shutdownClient() async {
        closing = true
        settingsAssistant.invalidate()
        settingsNavigation.retire()
        nativeSettings.invalidate()
        thisMacSettings?.invalidate()
        let assistantReports = await settingsAssistant.drain()
        if let surfaceDrain { await surfaceDrain.value }
        if let store = thisMacSettings { await store.drain() }
        let reports = await nativeSettings.drain()
        if reports.contains(where: { $0.hasIssues }) { message = "Native Settings drain reported issues. Original session reports were retained until app shutdown." }
        if assistantReports.contains(where: { !$0.closed || $0.issue != nil }) {
            message = "Ask Settings closure has unconfirmed outcomes. Original session metadata was retained until app shutdown."
        }
        await client.shutdown()
    }
}

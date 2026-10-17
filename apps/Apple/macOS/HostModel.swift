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
    var selectedConversation: OwnerID?
    var approvals: [PendingApproval] = []
    var approvalCursor: JSONValue = .null
    var approvalLoading = false
    var selectedApproval: ApprovalDetails?
    var selectedTask: HostTask?
    var taskDetails: JSONValue = .null
    var settings: JSONValue = .null
    var workspace = ""
    var executionMode = "read_only"
    var toolsEnabled = false
    var contextTokens = "32768"
    var serviceStatus: String?
    var pendingAction: OwnerRequest?
    var pendingActionLabel = ""
    @ObservationIgnored private let client: OwnerIPCClient
    @ObservationIgnored private let defaults = UserDefaults.standard
    @ObservationIgnored let settingsSurfaceID = UUID()
    let thisMacSettings: ThisMacSettingsStore?
    let nativeSettings: NativeSettingsStore
    let settingsNavigation: SettingsAssistantNavigation
    let settingsAssistant: SettingsAssistantController
    @ObservationIgnored private var settingsWindowCovered = false
    @ObservationIgnored private var settingsPresentationCovered = false
    @ObservationIgnored private var surfaceDrain: Task<Void, Never>?
    @ObservationIgnored private var surfaceDrainRequested = false

    init() {
        let executable = UserDefaults.standard.string(forKey: "hostExecutable") ?? ""
        let directory = UserDefaults.standard.string(forKey: "hostStateDirectory") ?? NSHomeDirectory() + "/.everything"
        let client = OwnerIPCClient()
        let connection = OwnerConnection(executable: executable, stateDirectory: directory)
        let connectionKey = UUID()
        self.client = client
        self.executable = executable
        stateDirectory = directory
        activeConnection = connection
        connectionVersion = connectionKey
        thisMacSettings = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first.map {
            ThisMacSettingsStore(directory: $0.appendingPathComponent("Everything", isDirectory: true).appendingPathComponent("DevicePreferences", isDirectory: true))
        }
        let nativeSettings = NativeSettingsStore(client: client, connection: connection, connectionKey: connectionKey)
        self.nativeSettings = nativeSettings
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
        approvals = []
        selectedConversation = nil
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
            guard !Task.isCancelled, original == connection, !closing else { return }
            self.draining = draining
            self.quarantined = quarantined
            self.tasks = tasks
            self.conversations = conversations
            connected = true
            lastRefresh = Date()
            if !conversations.contains(where: { $0.id == selectedConversation }) {
                selectedConversation = conversations.first?.id
            }
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

    func loadApprovals(more: Bool = false) async {
        guard let conversation = selectedConversation, !approvalLoading, !closing else { return }
        approvalLoading = true
        defer { approvalLoading = false }
        let original = connection
        let cursor = more ? approvalCursor : .null
        if !more { approvals = []; approvalCursor = .null }
        do {
            let result = try await client.request(.query("approvals", fields: ["conversation_id": .string(conversation.string), "limit": .integer(20), "cursor": cursor]), connection: original)
            guard result["kind"].string == "approvals", let rows = result["approvals"].array,
                  rows.count <= 20, try result.encoded().count <= 256 * 1024 else { throw OwnerClientError.invalidResponse }
            let items = try rows.map { try PendingApproval($0, conversation: conversation, connection: original) }
            guard original == connection, conversation == selectedConversation, !closing, !Task.isCancelled else { return }
            guard Set(items.map(\.id)).count == items.count else { throw OwnerClientError.invalidResponse }
            let previous = more ? approvals : []
            guard items.allSatisfy({ item in !previous.contains(where: { $0.id == item.id }) }) else { throw OwnerClientError.invalidResponse }
            approvals = previous + items
            approvalCursor = result["next_cursor"]
        } catch {
            if !Task.isCancelled { message = error.localizedDescription }
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
            if original == connection, !closing { taskDetails = result; selectedTask = task }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
    }

    func cancel(_ task: HostTask) async {
        guard task.source == connection else { return }
        do {
            let request = try OwnerRequest.command(conversation: task.conversationID, payload: ["kind": .string("cancel_task"), "task_id": .string(task.id.string)])
            await apply(request, label: "Task cancellation", expectedKind: "command")
        } catch { message = error.localizedDescription }
    }

    func decide(_ approval: ApprovalDetails, allow: Bool) async {
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
                "decision": .string(allow ? "allow" : "deny")
            ])
            await apply(request, label: allow ? "Operation approval" : "Operation denial", expectedKind: "command")
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
        defer { acting = false }
        let original = connection
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
        } catch { message = error.localizedDescription }
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
        let kind = request.body["kind"].string == "command" ? "command" : request.body["operation"]["kind"].string == "replace_settings" ? "settings" : "applied"
        await apply(request, label: pendingActionLabel, expectedKind: kind)
    }

    func inspectPendingCommand() async {
        guard let request = pendingAction, request.body["kind"].string == "command", !acting, !closing else { return }
        acting = true
        defer { acting = false }
        let original = connection
        do {
            let response = try await client.request(.query("command", fields: ["command_id": .string(request.id.string)]), connection: original)
            guard original == connection, pendingAction?.id == request.id, !closing, !Task.isCancelled else { return }
            guard response["kind"].string == "command" else { throw OwnerClientError.invalidResponse }
            let receipt = response["receipt"]
            if receipt == .null {
                message = "This host has no durable record of the command. Its exact original request remains available for an explicit retry."
                return
            }
            guard receipt["command_id"].string == request.id.string,
                  receipt["conversation_id"] == request.body["command"]["conversation_id"],
                  let phase = receipt["phase"].string,
                  ["accepted", "applied", "completed", "rejected"].contains(phase) else { throw OwnerClientError.invalidResponse }
            message = "Command state: " + phase + ". " + receipt["result"].formatted
            if ["completed", "rejected"].contains(phase) { pendingAction = nil }
            await refresh()
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
    }

    func loadSettings() async {
        let original = connection
        do {
            let result = try await client.request(.query("settings"), connection: original)
            guard result["kind"].string == "settings" else { throw OwnerClientError.invalidResponse }
            if original == connection, !closing, !Task.isCancelled { try adoptSettings(result["settings"]) }
        } catch { if !Task.isCancelled { message = error.localizedDescription } }
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

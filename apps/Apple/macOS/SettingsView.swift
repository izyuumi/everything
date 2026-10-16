import AppKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Bindable var model: HostModel
    @State private var confirmingStop = false
    @State private var confirmingInstall = false
    @State private var choosingPath = false

    var body: some View {
        TabView(selection: Binding(get: { model.settingsNavigation.selected }, set: { model.settingsNavigation.select($0) })) {
            hostSettings
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(SettingsAssistantNavigation.Destination.general)
                .onAppear { model.settingsNavigation.destinationAppeared(.general) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.general) }
            NativeSettingsControls(store: model.nativeSettings, section: .models, connectionEdited: model.connectionEdited)
                .tabItem { Label("Models", systemImage: "cpu") }
                .tag(SettingsAssistantNavigation.Destination.models)
                .onAppear { model.settingsNavigation.destinationAppeared(.models) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.models) }
            NativeSettingsControls(store: model.nativeSettings, section: .memory, connectionEdited: model.connectionEdited)
                .tabItem { Label("Memory", systemImage: "brain") }
                .tag(SettingsAssistantNavigation.Destination.memory)
                .onAppear { model.settingsNavigation.destinationAppeared(.memory) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.memory) }
            NativeSettingsControls(store: model.nativeSettings, section: .toolSelection, connectionEdited: model.connectionEdited)
                .tabItem { Label("Tool Selection", systemImage: "wrench.and.screwdriver") }
                .tag(SettingsAssistantNavigation.Destination.toolSelection)
                .onAppear { model.settingsNavigation.destinationAppeared(.toolSelection) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.toolSelection) }
            NativeDevicesControls(store: model.nativeSettings, connectionEdited: model.connectionEdited)
                .tabItem { Label("Devices", systemImage: "desktopcomputer.and.iphone") }
                .tag(SettingsAssistantNavigation.Destination.devices)
                .onAppear { model.settingsNavigation.destinationAppeared(.devices) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.devices) }
            NativeHostSettingsPane(store: model.nativeSettings, connectionEdited: model.connectionEdited)
                .tabItem { Label("Host settings", systemImage: "slider.horizontal.3") }
                .tag(SettingsAssistantNavigation.Destination.host)
                .onAppear { model.settingsNavigation.destinationAppeared(.host) }
                .onDisappear { model.settingsNavigation.destinationDisappeared(.host) }
            Group {
                if let store = model.thisMacSettings {
                    ThisMacSettingsView(store: store)
                } else {
                    Text("This Mac preferences are unavailable because Application Support could not be located.")
                        .padding()
                }
            }
            .tabItem { Label("Notifications", systemImage: "bell") }
            .tag(SettingsAssistantNavigation.Destination.notifications)
            .onAppear { model.settingsNavigation.destinationAppeared(.notifications) }
            .onDisappear { model.settingsNavigation.destinationDisappeared(.notifications) }
        }
        .frame(minWidth: 700, minHeight: 800)
        .inspector(isPresented: Binding(get: { model.settingsAssistant.expanded }, set: { model.settingsAssistant.setExpanded($0) })) {
            SettingsAssistantView(controller: model.settingsAssistant)
        }
        .toolbar {
            Button {
                model.settingsAssistant.setExpanded(!model.settingsAssistant.expanded)
            } label: {
                Label("Ask Settings", systemImage: "sidebar.right")
            }
            .help("Show or hide the optional Settings assistant")
            .accessibilityValue(model.settingsAssistant.expanded ? "Expanded" : "Collapsed")
        }
        .onChange(of: model.connected) { _, connected in model.nativeSettings.hostAvailabilityChanged(connected) }
        .onChange(of: model.nativeSettings.sessionPresent) { _, _ in model.settingsAssistant.ownershipChanged() }
        .onChange(of: model.nativeSettings.information) { _, _ in model.settingsAssistant.ownershipChanged() }
        .onChange(of: model.nativeSettings.cleanup.count) { _, _ in model.settingsAssistant.ownershipChanged() }
        .onChange(of: model.nativeSettings.deviceOperations.filter(\.retired).map(\.id)) { _, _ in model.settingsAssistant.ownershipChanged() }
        .background {
            SettingsWindowObserver(bindingID: model.settingsSurfaceID,
                                   onChange: { visible, active, covered in model.settingsWindowChanged(visible: visible, active: active, covered: covered) },
                                   onWindow: { model.settingsWindowAttached($0) },
                                   onClose: { model.settingsWindowClosed() })
                .frame(width: 0, height: 0)
        }
    }

    private var hostSettings: some View {
        Form {
            Section("Owner connection: Connect applies edits") {
                TextField("Everything executable", text: $model.executable)
                Button("Choose executable…") { choose(directory: false) }
                TextField("Private state directory", text: $model.stateDirectory)
                Button("Choose state directory…") { choose(directory: true) }
                Button("Connect") { model.rememberConnection(); Task { await model.refresh(); await model.loadSettings() } }
                    .disabled(model.acting || model.closing || model.pendingAction != nil)
                Text("Choose the physical trusted Rust CLI executable. The app connects to its existing headless host through owner IPC.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Defaults for new tasks") {
                TextField("Workspace", text: $model.workspace)
                Picker("Execution mode", selection: $model.executionMode) {
                    Text("Read only").tag("read_only")
                    Text("Trusted workspace").tag("trusted_workspace")
                    Text("Full host").tag("full_host")
                }
                Toggle("Enable tools", isOn: $model.toolsEnabled)
                TextField("Context token limit", text: $model.contextTokens)
                HStack {
                    Button("Reload defaults") { Task { await model.loadSettings() } }
                    Button("Save defaults") { Task { await model.saveSettings() } }
                        .disabled(!model.canMutate || model.settings == .null)
                }
                Text("Existing tasks keep their captured authority. Other settings and credential references are preserved by revision-checked updates. Use Models or Tool Selection for native credential entry.")
                    .font(.caption).foregroundStyle(.secondary)
            }.disabled(!model.connected || model.draining)
            Section("Per-user background service") {
                if let status = model.serviceStatus { Text(status) }
                HStack {
                    Button("Inspect service") { Task { await model.manageService("status") } }
                    Button("Install…") { setInstallConfirmation(true) }
                    Button("Start") { Task { await model.manageService("start") } }
                    Button("Stop service…", role: .destructive) { setStopConfirmation(true) }
                }
                HStack {
                    Button("Enable at login") { Task { await model.manageService("enable") } }
                    Button("Disable at login") { Task { await model.manageService("disable") } }
                }
                Text("These explicit controls manage com.everything.host in your launchd login domain. They do not control a separately started foreground host.")
                    .font(.caption).foregroundStyle(.secondary)
            }.disabled(model.acting || model.closing || model.connection.executable.isEmpty || model.connectionEdited || model.pendingAction != nil)
            if let message = model.message { Text(message).font(.caption).textSelection(.enabled) }
        }
        .formStyle(.grouped)
        .padding(12).frame(width: 660, height: 740)
        .confirmationDialog("Stop the per-user background service?", isPresented: Binding(get: { confirmingStop }, set: { setStopConfirmation($0) }), titleVisibility: .visible) {
            Button("Stop service", role: .destructive) { Task { await model.manageService("stop") } }
        } message: {
            Text("This is an explicit host stop request. Quitting the menu bar app leaves the host running.")
        }
        .confirmationDialog("Install the per-user background service?", isPresented: Binding(get: { confirmingInstall }, set: { setInstallConfirmation($0) }), titleVisibility: .visible) {
            Button("Install service") { Task { await model.manageService("install") } }
        } message: {
            Text("The selected CLI writes your launchd user-agent configuration for this state directory. No administrator access is used.")
        }
    }

    private func choose(directory: Bool) {
        guard !choosingPath else { return }
        model.settingsPresentationChanged(covered: true)
        choosingPath = true
        let panel = NSOpenPanel()
        panel.canChooseDirectories = directory
        panel.canChooseFiles = !directory
        panel.allowsMultipleSelection = false
        panel.begin { response in
            defer {
                choosingPath = false
                refreshCoverage()
            }
            guard response == .OK, let selected = panel.url else { return }
            let physical = selected.resolvingSymlinksInPath().path
            if directory { model.stateDirectory = physical }
            else { model.executable = physical }
        }
    }

    private func setStopConfirmation(_ value: Bool) {
        if value { model.settingsPresentationChanged(covered: true) }
        confirmingStop = value
        if !value { refreshCoverage() }
    }

    private func setInstallConfirmation(_ value: Bool) {
        if value { model.settingsPresentationChanged(covered: true) }
        confirmingInstall = value
        if !value { refreshCoverage() }
    }

    private func refreshCoverage() {
        model.settingsPresentationChanged(covered: confirmingStop || confirmingInstall || choosingPath)
    }
}

@MainActor
private struct NativeHostSettingsPane: View {
    let store: NativeSettingsStore
    let connectionEdited: Bool

    var body: some View {
        Form {
            Section("Native Host settings: read only") {
                LabeledContent("Session", value: store.status)
                HStack {
                    Button("Open Host settings") { store.open() }
                        .disabled(!store.canOpen || connectionEdited)
                    Button("Refresh") { store.refresh() }
                        .disabled(!store.canRefresh || connectionEdited)
                    Button("Close session") { store.close() }
                        .disabled(!store.sessionPresent)
                }
                if store.isOpening { ProgressView("Opening the original native session…") }
                if store.isRefreshing { ProgressView("Reading Host settings…") }
                if connectionEdited { Text("Apply connection edits with Connect in General before opening Host settings.").foregroundStyle(.secondary) }
                if let issue = store.issue { Text(issue.errorDescription ?? "Native Host settings are unavailable.").foregroundStyle(.red) }
                if let information = store.information {
                    LabeledContent("Host", value: information.hostID.uuidString.lowercased())
                    LabeledContent("Original session expiry", value: Date(timeIntervalSince1970: Double(information.expiresAt) / 1000).formatted())
                }
                if let observedAt = store.observedAt { Text("Last read \(observedAt.formatted())").font(.caption).foregroundStyle(.secondary) }
                Text("Open creates a temporary session for the applied General connection. Refresh does not renew it. Closing Settings closes the session and leaves the independent Host running.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if let snapshot = store.snapshot {
                Section("Last observed Host values") {
                    LabeledContent("Settings revision", value: String(snapshot.revision))
                    if store.snapshotIsStale { Text("These values are stale. An explicit refresh requires the original active session.").foregroundStyle(.secondary) }
                    ForEach(snapshot.entries, id: \.key.rawValue) { entry in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(label(entry.key)).font(.headline)
                            LabeledContent("Saved", value: value(entry.desired))
                            LabeledContent("Effective", value: value(entry.effective))
                            LabeledContent("State", value: state(entry.state))
                            if let reason = entry.unavailableReason { Text(unavailable(reason)).font(.caption).foregroundStyle(.secondary) }
                        }
                        .padding(.vertical, 4)
                    }
                }
            }
            if let registry = store.registry {
                Section("Host setting descriptions") {
                    ForEach(registry.descriptors.filter { $0.scope == .host }, id: \.key.rawValue) { descriptor in
                        DisclosureGroup(label(descriptor.key)) {
                            LabeledContent("Default", value: value(descriptor.defaultValue))
                            LabeledContent("Application", value: descriptor.applyBehavior.rawValue.replacingOccurrences(of: "_", with: " "))
                            if case .optionalDimensions(let choices) = descriptor.constraints {
                                if let choices { Text("Allowed dimensions: \(choices.map { String($0) }.joined(separator: ", "))") }
                                else { Text("Embedding model metadata is unavailable.") }
                            }
                        }
                    }
                }
            }
            if !store.cleanup.isEmpty || store.capacityReached {
                Section("Original session cleanup") {
                    ForEach(store.cleanup) { cleanup in
                        VStack(alignment: .leading, spacing: 4) {
                            if let report = cleanup.report {
                                if cleanup.hasIssues {
                                    Text("Earlier session cleanup reported issues. Its original state is retained.")
                                } else {
                                    Text("The original native cleanup report is retained.")
                                }
                                ForEach(Array(report.failures.prefix(4).enumerated()), id: \.offset) { _, failure in
                                    Text(failure.errorDescription ?? "The original drain has an unconfirmed outcome.")
                                        .font(.caption)
                                }
                                if report.failures.count > 4 { Text("\(report.failures.count) total drain issues are retained.").font(.caption) }
                                if !report.unresolvedOperationIDs.isEmpty { Text("\(report.unresolvedOperationIDs.count) original operations remain unresolved.").font(.caption) }
                            } else { ProgressView("Closing the original Host session…") }
                            if !cleanup.unlinkedCredentialOperationIDs.isEmpty {
                                Text("Unlinked initial storage receipts: \(cleanup.unlinkedCredentialOperationIDs.count)")
                                Text("Original storage completed without an observed metadata link. Review its record in Models or Tool Selection. A historical receipt does not establish current key presence.")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Text("Original connection: \(cleanup.connectionKey.uuidString.lowercased())").font(.caption).textSelection(.enabled)
                        }
                    }
                    if store.capacityReached { Text("The retained session limit is reached. Existing reports are preserved; a new session cannot be opened.").foregroundStyle(.secondary) }
                }
            }
        }
        .formStyle(.grouped)
        .padding(12)
        .textSelection(.enabled)
    }

    private func label(_ key: SettingKey) -> String { NativeSettingsText.label(key) }
    private func value(_ value: SettingValue?) -> String { NativeSettingsText.value(value) }
    private func state(_ state: SettingState) -> String { NativeSettingsText.state(state) }
    private func unavailable(_ reason: SettingsUnavailableReason) -> String { NativeSettingsText.unavailable(reason) }
}

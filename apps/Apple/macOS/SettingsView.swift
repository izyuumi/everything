import AppKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Bindable var model: HostModel
    @State private var confirmingStop = false
    @State private var confirmingInstall = false

    var body: some View {
        TabView {
            hostSettings
                .tabItem { Label("General", systemImage: "gearshape") }
            Group {
                if let store = model.thisMacSettings {
                    ThisMacSettingsView(store: store)
                } else {
                    Text("This Mac preferences are unavailable because Application Support could not be located.")
                        .padding()
                }
            }
            .tabItem { Label("Notifications", systemImage: "bell") }
        }
        .frame(width: 700, height: 800)
        .background {
            SettingsWindowObserver(bindingID: model.settingsSurfaceID,
                                   onChange: { visible, active in model.settingsWindowChanged(visible: visible, active: active) },
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
                Text("Existing tasks keep their captured authority. Other settings and credential references are preserved by revision-checked updates; credentials are managed through the CLI.")
                    .font(.caption).foregroundStyle(.secondary)
            }.disabled(!model.connected || model.draining)
            Section("Per-user background service") {
                if let status = model.serviceStatus { Text(status) }
                HStack {
                    Button("Inspect service") { Task { await model.manageService("status") } }
                    Button("Install…") { confirmingInstall = true }
                    Button("Start") { Task { await model.manageService("start") } }
                    Button("Stop service…", role: .destructive) { confirmingStop = true }
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
        .confirmationDialog("Stop the per-user background service?", isPresented: $confirmingStop, titleVisibility: .visible) {
            Button("Stop service", role: .destructive) { Task { await model.manageService("stop") } }
        } message: {
            Text("This is an explicit host stop request. Quitting the menu bar app leaves the host running.")
        }
        .confirmationDialog("Install the per-user background service?", isPresented: $confirmingInstall, titleVisibility: .visible) {
            Button("Install service") { Task { await model.manageService("install") } }
        } message: {
            Text("The selected CLI writes your launchd user-agent configuration for this state directory. No administrator access is used.")
        }
    }

    private func choose(directory: Bool) {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = directory
        panel.canChooseFiles = !directory
        panel.allowsMultipleSelection = false
        panel.begin { response in
            guard response == .OK, let selected = panel.url else { return }
            let physical = selected.resolvingSymlinksInPath().path
            if directory { model.stateDirectory = physical }
            else { model.executable = physical }
        }
    }
}

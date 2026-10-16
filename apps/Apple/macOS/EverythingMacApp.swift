import AppKit
import SwiftUI

@MainActor
final class EverythingAppDelegate: NSObject, NSApplicationDelegate {
    var model: HostModel?
    private var terminating = false

    func bindLocalPreferences(to model: HostModel) {
        guard !terminating else { return }
        self.model = model
        model.thisMacSettings?.open()
        synchronizeLocalActivity()
    }

    func synchronizeLocalActivity() {
        guard !terminating else { return }
        model?.thisMacSettings?.setApplicationActive(NSApplication.shared.isActive)
    }

    func applicationDidBecomeActive(_ notification: Notification) { synchronizeLocalActivity() }
    func applicationDidResignActive(_ notification: Notification) { synchronizeLocalActivity() }
    func applicationDidHide(_ notification: Notification) { synchronizeLocalActivity() }

    func applicationDidUnhide(_ notification: Notification) {
        synchronizeLocalActivity()
        if !terminating { model?.thisMacSettings?.refresh() }
    }

    func applySavedAppearance(_ saved: DeviceAppearance?, model: HostModel) {
        guard !terminating else { return }
        guard let saved else { return }
        let name: NSAppearance.Name?
        switch saved {
        case .system: name = nil
        case .light: name = .aqua
        case .dark: name = .darkAqua
        }
        let appearance = name.flatMap { NSAppearance(named: $0) }
        guard name == nil || appearance != nil else {
            model.message = "The saved local appearance could not be applied."
            return
        }
        NSApplication.shared.appearance = appearance
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model else { return .terminateNow }
        if terminating { return .terminateLater }
        terminating = true
        Task {
            await model.shutdownClient()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}

@main
@MainActor
struct EverythingMacApp: App {
    @NSApplicationDelegateAdaptor(EverythingAppDelegate.self) private var delegate
    @State private var model = HostModel()

    var body: some Scene {
        MenuBarExtra {
            MenuContent(model: model)
        } label: {
            Label(model.status, systemImage: model.connected ? "sparkles" : "sparkle.magnifyingglass")
                .onChange(of: model.thisMacSettings?.savedAppearance, initial: true) { _, saved in
                    delegate.bindLocalPreferences(to: model)
                    delegate.applySavedAppearance(saved, model: model)
                }
                .task(id: model.connectionKey) {
                    delegate.bindLocalPreferences(to: model)
                    await model.monitor()
                }
        }
        .menuBarExtraStyle(.window)
        Settings {
            SettingsView(model: model)
                .task {
                    delegate.bindLocalPreferences(to: model)
                    if !model.executable.isEmpty { await model.loadSettings() }
                }
        }
    }
}

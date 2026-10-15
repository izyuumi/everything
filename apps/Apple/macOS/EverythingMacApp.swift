import AppKit
import SwiftUI

@MainActor
final class EverythingAppDelegate: NSObject, NSApplicationDelegate {
    var model: HostModel?
    private var terminating = false

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
                .task(id: model.connectionKey) {
                    delegate.model = model
                    await model.monitor()
                }
        }
        .menuBarExtraStyle(.window)
        Settings {
            SettingsView(model: model)
                .task {
                    delegate.model = model
                    if !model.executable.isEmpty { await model.loadSettings() }
                }
        }
    }
}

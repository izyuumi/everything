import SwiftUI

@main
@MainActor
struct EverythingIOSApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .privacySensitive()
                .overlay {
                    if scenePhase != .active {
                        Rectangle().fill(.background).ignoresSafeArea().overlay { Label("Everything", systemImage: "lock.fill").font(.title) }
                    }
                }
                .task(id: scenePhase) { await model.scene(active: scenePhase == .active) }
        }
    }
}

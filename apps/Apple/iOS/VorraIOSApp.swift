import SwiftUI
import UIKit

@main
@MainActor
struct VorraIOSApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase
    @State private var appliedAppearance: DeviceAppearance?

    var body: some Scene {
        WindowGroup {
            RootView(model: model)
                .modifier(InactivePrivacyCover())
                .preferredColorScheme(colorScheme)
                .onChange(of: model.localSettings?.savedAppearance, initial: true) { _, saved in
                    if let saved { appliedAppearance = saved }
                }
                .onChange(of: scenePhase, initial: true) { _, phase in
                    model.localSceneChanged(active: phase == .active)
                }
                .onReceive(NotificationCenter.default.publisher(for: UIApplication.willResignActiveNotification)) { _ in
                    model.localApplicationWillResignActive()
                }
                .onReceive(NotificationCenter.default.publisher(for: UIApplication.didBecomeActiveNotification)) { _ in
                    model.localApplicationDidBecomeActive()
                }
                .task(id: scenePhase) { await model.scene(active: scenePhase == .active) }
                .onOpenURL { model.openPairingLink($0) }
        }
    }

    private var colorScheme: ColorScheme? {
        switch appliedAppearance {
        case .light: .light
        case .dark: .dark
        case .system, nil: nil
        }
    }
}

struct InactivePrivacyCover: ViewModifier {
    @Environment(\.scenePhase) private var scenePhase

    func body(content: Content) -> some View {
        content.privacySensitive().overlay {
            if scenePhase != .active {
                Rectangle().fill(.background).ignoresSafeArea().overlay { Label("Vorra", systemImage: "lock.fill").font(.title) }
            }
        }
    }
}

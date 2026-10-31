import AppKit
import Observation
import Sparkle
import SwiftUI

@MainActor
enum MacUpdates {
    private static let configuredKey = "VorraUpdatesConfigured"
    nonisolated static let betaKey = "VorraBetaUpdates"
    nonisolated static let betaFeedKey = "VorraBetaFeedURL"
    nonisolated static let betaFeedPrefix = "https://github.com/izyuumi/everything/releases/download/"
    private static let releasesURL = URL(string: "https://api.github.com/repos/izyuumi/everything/releases?per_page=100")!
    private static let maxReleasesBytes = 4 << 20
    private static var refresher: Task<Void, Never>?

    static let controller: SPUStandardUpdaterController? = {
        let info = Bundle.main.infoDictionary ?? [:]
        guard let feed = info["SUFeedURL"] as? String, !feed.isEmpty,
              let key = info["SUPublicEDKey"] as? String, !key.isEmpty else { return nil }
        let controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: delegate, userDriverDelegate: nil)
        if !UserDefaults.standard.bool(forKey: configuredKey) {
            controller.updater.automaticallyChecksForUpdates = true
            controller.updater.automaticallyDownloadsUpdates = true
            controller.updater.updateCheckInterval = 3600
            UserDefaults.standard.set(true, forKey: configuredKey)
        }
        return controller
    }()

    static func start() {
        guard controller != nil, refresher == nil else { return }
        refresher = Task {
            while !Task.isCancelled {
                await refreshBetaFeed()
                try? await Task.sleep(for: .seconds(3600))
            }
        }
    }

    static func checkForUpdates() {
        guard let controller else { return }
        Task {
            await refreshBetaFeed()
            controller.checkForUpdates(nil)
        }
    }

    static func betaChanged() {
        guard let controller else { return }
        Task {
            await refreshBetaFeed()
            controller.updater.checkForUpdatesInBackground()
        }
    }

    private static func refreshBetaFeed() async {
        guard UserDefaults.standard.bool(forKey: betaKey) else { return }
        var request = URLRequest(url: releasesURL, timeoutInterval: 30)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        guard let (data, response) = try? await URLSession.shared.data(for: request),
              (response as? HTTPURLResponse)?.statusCode == 200, data.count <= maxReleasesBytes,
              let releases = try? JSONDecoder().decode([Release].self, from: data) else { return }
        let beta = releases.first { !$0.draft && $0.prerelease && $0.tag_name.wholeMatch(of: /v[0-9]+\.[0-9]+\.[0-9]+-beta/) != nil }
        if let beta, let url = beta.assets.first(where: { $0.name == "appcast.xml" })?.browser_download_url,
           url == "\(betaFeedPrefix)\(beta.tag_name)/appcast.xml" {
            UserDefaults.standard.set(url, forKey: betaFeedKey)
        } else {
            UserDefaults.standard.removeObject(forKey: betaFeedKey)
        }
    }

    private struct Release: Decodable {
        let tag_name: String
        let draft: Bool
        let prerelease: Bool
        let assets: [Asset]
    }

    private struct Asset: Decodable {
        let name: String
        let browser_download_url: String
    }

    @Observable final class Status {
        var lastCheck: Date?
        var message: String?
    }

    static let status = Status()

    private static let delegate = Delegate()

    private final class Delegate: NSObject, SPUUpdaterDelegate {
        func feedURLString(for updater: SPUUpdater) -> String? {
            let defaults = UserDefaults.standard
            guard defaults.bool(forKey: MacUpdates.betaKey),
                  let url = defaults.string(forKey: MacUpdates.betaFeedKey),
                  url.hasPrefix(MacUpdates.betaFeedPrefix) else { return nil }
            return url
        }

        func updaterDidNotFindUpdate(_ updater: SPUUpdater) {
            MainActor.assumeIsolated { MacUpdates.status.message = String(localized: "You’re up to date.") }
        }

        func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
            let version = item.displayVersionString
            MainActor.assumeIsolated { MacUpdates.status.message = String(localized: "Version \(version) is available.") }
        }

        func updater(_ updater: SPUUpdater, didFinishUpdateCycleFor updateCheck: SPUUpdateCheck, error: (any Error)?) {
            let date = updater.lastUpdateCheckDate
            let failure = (error as NSError?).flatMap { $0.code == 1001 ? nil : $0.localizedDescription }
            MainActor.assumeIsolated {
                MacUpdates.status.lastCheck = date
                if let failure { MacUpdates.status.message = failure }
            }
        }
    }
}

@MainActor
struct UpdateSettings: View {
    let updater: SPUUpdater
    @AppStorage(MacUpdates.betaKey) private var betaUpdates = false
    @State private var autoCheck = false
    @State private var autoInstall = false
    @State private var canCheck = false
    private let status = MacUpdates.status

    var body: some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading, spacing: 8) {
                Toggle(String(localized: "Automatically Check for Updates"), isOn: $autoCheck)
                    .onChange(of: autoCheck) { updater.automaticallyChecksForUpdates = autoCheck }
                Toggle(String(localized: "Install Updates Automatically"), isOn: $autoInstall)
                    .onChange(of: autoInstall) { updater.automaticallyDownloadsUpdates = autoInstall }
                    .disabled(!autoCheck)
                Picker(String(localized: "Update to:"), selection: $betaUpdates) {
                    Text("Stable Versions").tag(false)
                    Text("Beta Versions").tag(true)
                }
                .fixedSize()
                .onChange(of: betaUpdates) { MacUpdates.betaChanged() }
                .help(String(localized: "Beta builds have the newest changes before a stable release."))
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 4) {
                Button(String(localized: "Check Now")) { MacUpdates.checkForUpdates() }
                    .disabled(!canCheck)
                if let last = status.lastCheck {
                    Text("Last check: \(last.formatted(date: .numeric, time: .shortened))").foregroundStyle(.secondary)
                }
                if let message = status.message {
                    Text(message).foregroundStyle(.secondary).multilineTextAlignment(.trailing)
                }
            }
        }
        .onAppear {
            autoCheck = updater.automaticallyChecksForUpdates
            autoInstall = updater.automaticallyDownloadsUpdates
            if status.lastCheck == nil { status.lastCheck = updater.lastUpdateCheckDate }
        }
        .onReceive(updater.publisher(for: \.canCheckForUpdates)) { canCheck = $0 }
    }
}

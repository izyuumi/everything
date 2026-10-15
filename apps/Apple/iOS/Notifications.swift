import Foundation
import UserNotifications

private final class PrivacyNotificationDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}

actor NotificationService {
    private let delegate = PrivacyNotificationDelegate()
    private var lastActivity: ContinuousClock.Instant?

    init() { UNUserNotificationCenter.current().delegate = delegate }

    func authorize() async throws -> Bool {
        try await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound])
    }

    func activity() async throws {
        if let lastActivity, lastActivity.duration(to: .now) < .seconds(30) { return }
        let content = UNMutableNotificationContent()
        content.title = "Everything"
        content.body = "New activity is available."
        content.sound = .default
        try await UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
        lastActivity = .now
    }
}

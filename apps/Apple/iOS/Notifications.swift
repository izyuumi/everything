import Foundation
import UserNotifications

private final class PrivacyNotificationDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}

actor NotificationService {
    private let delegate = PrivacyNotificationDelegate()

    init() { UNUserNotificationCenter.current().delegate = delegate }
}

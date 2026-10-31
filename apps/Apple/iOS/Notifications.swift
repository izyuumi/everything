import Foundation
import UserNotifications

private final class PrivacyNotificationDelegate: NSObject, UNUserNotificationCenterDelegate, Sendable {
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}

enum NotificationDeliveryError: Error {
    case capacity
    case submissionFailed
}

enum NotificationDeliveryOutcome {
    case submitted
    case suppressedPermission
    case suppressedPreparation
    case suppressedEligibility
    case retiredAfterSubmission
}

@MainActor
final class NotificationService {
    private let delegate = PrivacyNotificationDelegate()
    private let center = UNUserNotificationCenter.current()
    private var submitting = false

    init() { UNUserNotificationCenter.current().delegate = delegate }

    func deliver(_ notice: ReplicaNotificationNotice,
                 prepare: @MainActor () async throws -> Bool,
                 isEligible: @MainActor () -> Bool) async throws -> NotificationDeliveryOutcome {
        guard !submitting else { throw NotificationDeliveryError.capacity }
        guard notice.identifier.utf8.count == 64,
              notice.identifier.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
            throw NotificationDeliveryError.submissionFailed
        }
        submitting = true
        defer { submitting = false }
        let settings = await center.notificationSettings()
        guard !Task.isCancelled, isEligible() else { return .suppressedEligibility }
        guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .ephemeral,
              settings.alertSetting == .enabled else { return .suppressedPermission }
        guard try await prepare() else { return .suppressedPreparation }
        guard !Task.isCancelled, isEligible() else { return .suppressedEligibility }
        let content = UNMutableNotificationContent()
        content.title = "Vorra"
        switch notice.category {
        case .completion: content.body = "A task completed. Open Vorra to review it."
        case .attention: content.body = "A permission request needs attention. Open Vorra to review it."
        }
        if settings.soundSetting == .enabled { content.sound = .default }
        let request = UNNotificationRequest(identifier: notice.identifier, content: content, trigger: nil)
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            center.add(request) { error in
                if error != nil { continuation.resume(throwing: NotificationDeliveryError.submissionFailed) }
                else { continuation.resume() }
            }
        }
        guard !Task.isCancelled, isEligible() else {
            center.removePendingNotificationRequests(withIdentifiers: [notice.identifier])
            return .retiredAfterSubmission
        }
        return .submitted
    }
}

import Foundation
import UserNotifications

#if os(iOS)
import UIKit
#endif

enum DeviceNotificationAuthorization: String, Sendable {
    case notDetermined
    case denied
    case authorized
    case provisional
    case ephemeral
    case unavailable
}

enum DeviceNotificationFeature: Equatable, Sendable {
    case enabled
    case disabled
    case unsupported
}

struct DeviceNotificationObservation: Equatable, Sendable {
    let authorization: DeviceNotificationAuthorization
    let alerts: DeviceNotificationFeature
    let sounds: DeviceNotificationFeature
    let notificationCenter: DeviceNotificationFeature
    let observedAt: Date

    var canPresentAlerts: Bool {
        [.authorized, .ephemeral].contains(authorization) && alerts == .enabled
    }
}

enum DeviceNotificationGuidance: Equatable, Sendable {
    case none
    case systemSettingsOpened
    case instructions
    case settingsUnavailable

    var instructions: String? {
        guard self == .instructions || self == .settingsUnavailable else { return nil }
        #if os(iOS)
        return "Open Settings, choose Notifications, then Everything to review notification permissions."
        #else
        return "Open System Settings, choose Notifications, then Everything to review notification permissions."
        #endif
    }
}

@MainActor
final class NotificationAuthorizationService {
    private let center = UNUserNotificationCenter.current()

    func observe() async -> DeviceNotificationObservation {
        let settings = await center.notificationSettings()
        let authorization: DeviceNotificationAuthorization
        switch settings.authorizationStatus {
        case .notDetermined: authorization = .notDetermined
        case .denied: authorization = .denied
        case .authorized: authorization = .authorized
        case .provisional: authorization = .provisional
        #if os(iOS)
        case .ephemeral: authorization = .ephemeral
        #endif
        @unknown default: authorization = .unavailable
        }
        return DeviceNotificationObservation(authorization: authorization, alerts: Self.feature(settings.alertSetting), sounds: Self.feature(settings.soundSetting), notificationCenter: Self.feature(settings.notificationCenterSetting), observedAt: Date())
    }

    func request(session: DeviceForegroundSession, generation: UInt64, expiresAt: Date, deadline: ContinuousClock.Instant) async throws -> DeviceNotificationObservation {
        try session.validate(generation: generation, expiresAt: expiresAt, deadline: deadline)
        _ = try await center.requestAuthorization(options: [.alert, .sound])
        return await observe()
    }

    func openSystemSettings(session: DeviceForegroundSession, generation: UInt64, expiresAt: Date, deadline: ContinuousClock.Instant) async throws -> DeviceNotificationGuidance {
        try session.validate(generation: generation, expiresAt: expiresAt, deadline: deadline)
        #if os(iOS)
        guard let url = URL(string: UIApplication.openNotificationSettingsURLString), UIApplication.shared.canOpenURL(url) else {
            return .settingsUnavailable
        }
        return await UIApplication.shared.open(url) ? .systemSettingsOpened : .settingsUnavailable
        #else
        return .instructions
        #endif
    }

    private static func feature(_ value: UNNotificationSetting) -> DeviceNotificationFeature {
        switch value {
        case .enabled: .enabled
        case .disabled: .disabled
        case .notSupported: .unsupported
        @unknown default: .unsupported
        }
    }
}

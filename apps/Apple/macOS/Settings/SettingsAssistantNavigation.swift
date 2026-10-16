import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class SettingsAssistantNavigation {
    enum Destination: String, Hashable {
        case general, models, memory, toolSelection, devices, host, notifications

        init?(_ section: SettingsSection) {
            switch section {
            case .general: self = .general
            case .models: self = .models
            case .memory: self = .memory
            case .toolSelection: self = .toolSelection
            case .devices: self = .devices
            case .notifications: self = .notifications
            case .ownerSecurity: return nil
            }
        }
    }

    private struct Pending {
        let section: SettingsSection
        let destination: Destination
        let continuation: CheckedContinuation<NativeSettingsStore.AssistantSectionResult, Never>
    }

    private(set) var selected = Destination.general
    @ObservationIgnored private weak var window: NSWindow?
    @ObservationIgnored private var appeared: Destination?
    @ObservationIgnored private var pending: Pending?

    func attachWindow(_ selected: NSWindow?) {
        if window !== selected { retire() }
        window = selected
    }

    func select(_ destination: Destination) {
        if let pending, pending.destination != destination { retire() }
        if selected != destination { appeared = nil }
        selected = destination
    }

    func destinationAppeared(_ destination: Destination) {
        guard selected == destination else { return }
        appeared = destination
        acknowledge()
    }

    func destinationDisappeared(_ destination: Destination) {
        if appeared == destination { appeared = nil }
    }

    func open(_ section: SettingsSection) async -> NativeSettingsStore.AssistantSectionResult {
        guard !Task.isCancelled, pending == nil, eligible,
              let destination = Destination(section) else { return .dismissed }
        return await withCheckedContinuation { continuation in
            select(destination)
            pending = Pending(section: section, destination: destination, continuation: continuation)
            acknowledge()
        }
    }

    func retire() {
        guard let original = pending else { return }
        pending = nil
        original.continuation.resume(returning: .dismissed)
    }

    private var eligible: Bool {
        guard let window else { return false }
        return NSApplication.shared.isActive && !NSApplication.shared.isHidden
            && window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible)
            && window.attachedSheet == nil && window.sheets.isEmpty
    }

    private func acknowledge() {
        guard let original = pending, selected == original.destination,
              appeared == original.destination, eligible else { return }
        pending = nil
        original.continuation.resume(returning: .opened(original.section))
    }
}

import AppKit
import CoreImage.CIFilterBuiltins
import Foundation
import Observation
import SwiftUI

@MainActor
@Observable
final class NativeDeviceCeremony: NSObject {
    enum Stage: Equatable { case preparing, confirmation, confirming, invitation, review, approving, acceptance, finished, unconfirmed }

    struct Choice {
        let candidate: NativePairingCandidate
        let label: String
        let fingerprint: String
        let access: NativeSettingsWire.PairingAccess
    }

    let handle: NativeSettingsStore.DeviceHandle
    let request: SettingsDeviceRequest
    private(set) var stage = Stage.preparing
    private(set) var hostFingerprint: String?
    private(set) var invitationText = ""
    private(set) var invitationImage: NSImage?
    private(set) var acceptanceText = ""
    private(set) var review: NativePairingCandidate.Review?
    private(set) var issue: String?
    private(set) var retired = false
    private(set) var uiRetired = false
    private(set) var retirementUncertain = false
    var requestText = ""
    var label = ""
    var compared = false
    var allConversations = false
    var conversationIDs = ""
    var submitCommands = false
    var resolvePermissions = false
    @ObservationIgnored private weak var store: NativeSettingsStore?
    @ObservationIgnored private weak var parent: NSWindow?
    @ObservationIgnored private var panel: NSPanel?
    @ObservationIgnored private var candidate: NativePairingCandidate?
    @ObservationIgnored private var expiresAt: UInt64
    @ObservationIgnored private var deadline: ContinuousClock.Instant
    @ObservationIgnored private var pairingID: UUID?
    @ObservationIgnored private var pairingExpiresAt: UInt64?
    @ObservationIgnored private var sheetStarted = false
    @ObservationIgnored private var sheetEnded = false
    @ObservationIgnored private var endingSheet = false
    @ObservationIgnored private var waiters: [CheckedContinuation<Void, Never>] = []

    override var description: String { "NativeDeviceCeremony([private])" }
    override var debugDescription: String { description }

    init(store: NativeSettingsStore, handle: NativeSettingsStore.DeviceHandle, request: SettingsDeviceRequest,
         parent: NSWindow, expiresAt: UInt64, deadline: ContinuousClock.Instant) {
        self.store = store
        self.handle = handle
        self.request = request
        self.parent = parent
        self.expiresAt = expiresAt
        self.deadline = deadline
        super.init()
    }

    deinit { NotificationCenter.default.removeObserver(self) }

    static func requireParent(_ parent: NSWindow?) throws -> NSWindow {
        guard let parent, NSApplication.shared.isActive, !NSApplication.shared.isHidden,
              parent.isVisible, !parent.isMiniaturized, parent.occlusionState.contains(.visible),
              parent.attachedSheet == nil, parent.sheets.isEmpty else { throw NativeSettingsSessionError.inactive }
        return parent
    }

    func present() throws {
        guard !retired, !sheetStarted else { throw NativeSettingsSessionError.inactive }
        let parent = try Self.requireParent(parent)
        try validate()
        let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 620, height: 680),
                            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        panel.title = request.action == .pair ? "Pair a device" : "Confirm device revocation"
        panel.isReleasedWhenClosed = false
        panel.contentView = NSHostingView(rootView: NativeDeviceCeremonyView(ceremony: self))
        panel.standardWindowButton(.closeButton)?.target = self
        panel.standardWindowButton(.closeButton)?.action = #selector(closeRequested(_:))
        _ = try Self.requireParent(parent)
        self.panel = panel
        let center = NotificationCenter.default
        for name in [NSApplication.didResignActiveNotification, NSApplication.didHideNotification] {
            center.addObserver(self, selector: #selector(activityChanged(_:)), name: name, object: NSApplication.shared)
        }
        for name in [NSWindow.willCloseNotification, NSWindow.didMiniaturizeNotification, NSWindow.didChangeOcclusionStateNotification] {
            center.addObserver(self, selector: #selector(parentChanged(_:)), name: name, object: parent)
        }
        sheetStarted = true
        parent.beginSheet(panel) { [self] _ in
            sheetEnded = true
            if !retired { dismiss() }
            panel.orderOut(nil)
            acknowledgeRetirement()
        }
    }

    func validate() throws {
        guard !retired, NSApplication.shared.isActive, !NSApplication.shared.isHidden,
              let parent, parent.isVisible, !parent.isMiniaturized,
              parent.occlusionState.contains(.visible) else { throw NativeSettingsSessionError.inactive }
        guard try NativeSettingsGate.wallTime() < expiresAt, ContinuousClock.now < deadline else {
            throw NativeSettingsSessionError.expired
        }
    }

    func confirmation(expiresAt: UInt64, deadline: ContinuousClock.Instant) throws {
        self.expiresAt = min(self.expiresAt, expiresAt)
        self.deadline = min(self.deadline, deadline)
        try validate()
        stage = .confirmation
    }

    func showInvitation(_ presentation: NativeSettingsSession.PairingPresentation) async throws {
        try validate()
        guard request.action == .pair, presentation.operationID == handle.id,
              presentation.invitation.utf8.count <= 4096 else {
            throw NativeSettingsSessionError.invalidResponse
        }
        let invitation = try PairingInvitation(Data(presentation.invitation.utf8), now: NativeSettingsGate.wallTime())
        guard invitation.id == presentation.pairingID, invitation.inviter.deviceID == request.scope.identifier,
              invitation.inviter.fingerprint == presentation.fingerprint,
              invitation.expires == presentation.expiresAt else { throw NativeSettingsSessionError.invalidResponse }
        expiresAt = min(expiresAt, presentation.expiresAt)
        pairingID = presentation.pairingID
        pairingExpiresAt = presentation.expiresAt
        hostFingerprint = presentation.fingerprint
        invitationText = presentation.invitation
        invitationImage = qrImage(Data(presentation.invitation.utf8))
        stage = .invitation
        await waitForRetirement()
    }

    func showAcceptance(_ presentation: NativeSettingsSession.PairingAcceptancePresentation) async throws {
        try validate()
        guard presentation.pairingID == pairingID, presentation.expiresAt == pairingExpiresAt,
              presentation.operationID != handle.id else { throw NativeSettingsSessionError.invalidResponse }
        let data = try presentation.transferData()
        guard data.count <= 4096, let text = String(data: data, encoding: .utf8) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        invitationText = ""
        invitationImage = nil
        requestText = ""
        acceptanceText = text
        stage = .acceptance
        await waitForRetirement()
    }

    func setRequestText(_ value: String) {
        guard !retired, stage == .invitation || stage == .review else { return }
        guard value.utf8.count <= 4096 else { requestText = ""; issue = "The signed request exceeds 4096 bytes."; return }
        requestText = value
    }

    func reviewRequest() {
        do {
            try validate()
            guard stage == .invitation || stage == .review else { throw NativeSettingsSessionError.invalidRequest }
            let bytes = Data(requestText.utf8)
            requestText = ""
            candidate?.discard()
            candidate = nil
            review = nil
            resetAccess()
            let candidate = try NativePairingCandidate(privateRequest: bytes)
            guard candidate.review.hostID == request.scope.identifier, candidate.review.pairingID == pairingID,
                  candidate.review.expiresAt == pairingExpiresAt else { candidate.discard(); throw NativeSettingsSessionError.invalidRequest }
            self.candidate = candidate
            review = candidate.review
            stage = .review
            issue = nil
        } catch { issue = "The original ceremony cannot use this signed request. Check its identity and expiry." }
    }

    func takeChoice() throws -> Choice {
        try validate()
        guard stage == .review, compared, let candidate, candidate.review == review else {
            throw NativeSettingsSessionError.invalidRequest
        }
        try SettingsWire.text(label, maximum: 256)
        let conversations: NativeSettingsWire.PairingAccess.Conversations
        if allConversations { conversations = .all }
        else {
            guard conversationIDs.utf8.count <= 4096 else { throw NativeSettingsSessionError.invalidRequest }
            let parts = conversationIDs.split(whereSeparator: { $0.isWhitespace || $0 == "," })
            guard parts.count <= 64 else { throw NativeSettingsSessionError.invalidRequest }
            let ids = try parts.map { value -> UUID in
                guard let id = UUID(uuidString: String(value)) else { throw NativeSettingsSessionError.invalidRequest }
                try SettingsWire.identifier(id)
                return id
            }
            guard Set(ids).count == ids.count else { throw NativeSettingsSessionError.invalidRequest }
            conversations = .selected(Set(ids))
        }
        let access = try NativeSettingsWire.PairingAccess(conversations: conversations, submitCommands: submitCommands, resolvePermissions: resolvePermissions)
        let choice = Choice(candidate: candidate, label: label, fingerprint: candidate.review.responder.fingerprint, access: access)
        self.candidate = nil
        requestText = ""
        stage = .approving
        issue = nil
        return choice
    }

    func confirm() { store?.confirmDeviceAction(handle, ceremony: self) }
    func approve() { store?.approveDevicePairing(handle, ceremony: self) }
    func dismiss() {
        if let store { store.dismissDeviceCeremony(handle) }
        else { retire() }
    }
    func inspectApproval() { store?.inspectPairingApproval(handle) }

    func inspectOriginal() { store?.inspectDeviceOperation(handle) }

    func confirming() { stage = .confirming }

    func failure(_ error: NativeSettingsSessionError) {
        issue = error.errorDescription
        if stage == .preparing || stage == .confirming || stage == .approving { stage = .unconfirmed }
    }

    func completed() {
        guard !retired else { return }
        stage = .finished
    }

    func expireIfNeeded() {
        do { try validate() }
        catch { dismiss() }
    }

    func retire() {
        if !retired {
            retired = true
            candidate?.discard()
            candidate = nil
            review = nil
            invitationText = ""
            invitationImage = nil
            acceptanceText = ""
            requestText = ""
            resetAccess()
            NotificationCenter.default.removeObserver(self)
        }
        guard !uiRetired else { return }
        guard sheetStarted else { sheetEnded = true; acknowledgeRetirement(); return }
        guard let panel else { retirementUncertain = true; return }
        if !sheetEnded, !endingSheet {
            guard let parent, panel.sheetParent === parent else { retirementUncertain = true; return }
            endingSheet = true
            parent.endSheet(panel, returnCode: .cancel)
        }
        if sheetEnded { panel.orderOut(nil); acknowledgeRetirement() }
    }

    func waitForRetirement() async {
        if uiRetired { return }
        await withCheckedContinuation { continuation in waiters.append(continuation) }
    }

    private func acknowledgeRetirement() {
        guard retired, sheetEnded, panel?.isVisible != true, panel?.sheetParent == nil else {
            retirementUncertain = retired
            return
        }
        uiRetired = true
        retirementUncertain = false
        panel?.contentView = nil
        panel = nil
        let owned = waiters
        waiters.removeAll()
        for waiter in owned { waiter.resume() }
    }

    private func resetAccess() {
        label = ""
        compared = false
        allConversations = false
        conversationIDs = ""
        submitCommands = false
        resolvePermissions = false
    }

    @objc private func closeRequested(_ sender: Any?) { dismiss() }
    @objc private func activityChanged(_ notification: Notification) { dismiss() }
    @objc private func parentChanged(_ notification: Notification) {
        if notification.name == NSWindow.willCloseNotification || notification.name == NSWindow.didMiniaturizeNotification {
            dismiss()
        } else { expireIfNeeded() }
    }

    private func qrImage(_ data: Data) -> NSImage? {
        guard !data.isEmpty, data.count <= 4096 else { return nil }
        let generator = CIFilter.qrCodeGenerator()
        generator.message = data
        generator.correctionLevel = "M"
        guard let output = generator.outputImage,
              let image = CIContext().createCGImage(output.transformed(by: CGAffineTransform(scaleX: 6, y: 6)),
                                                   from: output.extent.applying(CGAffineTransform(scaleX: 6, y: 6))) else { return nil }
        return NSImage(cgImage: image, size: NSSize(width: CGFloat(image.width), height: CGFloat(image.height)))
    }
}

import AppKit
import SwiftUI

@MainActor
struct SettingsWindowObserver: NSViewRepresentable {
    let bindingID: UUID
    let onChange: @MainActor (Bool, Bool, Bool) -> Void
    let onWindow: @MainActor (NSWindow?) -> Void
    let onClose: @MainActor () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(bindingID: bindingID, onChange: onChange, onWindow: onWindow, onClose: onClose)
    }

    func makeNSView(context: Context) -> WindowView {
        let view = WindowView()
        view.observer = context.coordinator
        return view
    }

    func updateNSView(_ view: WindowView, context: Context) {
        context.coordinator.rebind(bindingID: bindingID, onChange: onChange, onWindow: onWindow, onClose: onClose)
    }

    static func dismantleNSView(_ view: WindowView, coordinator: Coordinator) {
        view.observer = nil
        coordinator.stop()
    }

    @MainActor
    final class WindowView: NSView {
        weak var observer: Coordinator?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            observer?.attach(window)
        }
    }

    @MainActor
    final class Coordinator: NSObject {
        private var bindingID: UUID
        private var onChange: @MainActor (Bool, Bool, Bool) -> Void
        private var onWindow: @MainActor (NSWindow?) -> Void
        private var onClose: @MainActor () -> Void
        private weak var window: NSWindow?
        private var closing = false
        private var stopped = false
        private var closeDelivered = false
        private var sheetBeginning = false
        private var previous: State?

        private struct State: Equatable {
            let visible: Bool
            let active: Bool
            let covered: Bool
        }

        init(bindingID: UUID, onChange: @escaping @MainActor (Bool, Bool, Bool) -> Void,
             onWindow: @escaping @MainActor (NSWindow?) -> Void, onClose: @escaping @MainActor () -> Void) {
            self.bindingID = bindingID
            self.onChange = onChange
            self.onWindow = onWindow
            self.onClose = onClose
            super.init()
            let center = NotificationCenter.default
            for name in [NSApplication.didBecomeActiveNotification, NSApplication.didResignActiveNotification,
                         NSApplication.didHideNotification, NSApplication.didUnhideNotification] {
                center.addObserver(self, selector: #selector(applicationChanged(_:)), name: name, object: NSApplication.shared)
            }
        }

        deinit {
            NotificationCenter.default.removeObserver(self)
        }

        func rebind(bindingID selected: UUID, onChange: @escaping @MainActor (Bool, Bool, Bool) -> Void,
                    onWindow: @escaping @MainActor (NSWindow?) -> Void, onClose: @escaping @MainActor () -> Void) {
            guard !stopped else { return }
            if bindingID != selected {
                closeSurface()
                self.onWindow(nil)
                bindingID = selected
                self.onChange = onChange
                self.onWindow = onWindow
                self.onClose = onClose
                closing = false
                closeDelivered = false
                previous = nil
                self.onWindow(window)
                publish()
            } else {
                self.onChange = onChange
                self.onWindow = onWindow
                self.onClose = onClose
            }
        }

        func attach(_ selected: NSWindow?) {
            guard !stopped else { return }
            if window !== selected {
                if window != nil { closeSurface() }
                if let window { NotificationCenter.default.removeObserver(self, name: nil, object: window) }
                window = selected
                sheetBeginning = false
                closing = false
                closeDelivered = false
                if let selected {
                    let center = NotificationCenter.default
                    for name in [NSWindow.didResignKeyNotification,
                                 NSWindow.didMiniaturizeNotification, NSWindow.didDeminiaturizeNotification,
                                 NSWindow.didChangeOcclusionStateNotification] {
                        center.addObserver(self, selector: #selector(windowChanged(_:)), name: name, object: selected)
                    }
                    center.addObserver(self, selector: #selector(windowBecameKey(_:)), name: NSWindow.didBecomeKeyNotification, object: selected)
                    center.addObserver(self, selector: #selector(windowWillClose(_:)), name: NSWindow.willCloseNotification, object: selected)
                    center.addObserver(self, selector: #selector(sheetWillBegin(_:)), name: NSWindow.willBeginSheetNotification, object: selected)
                    center.addObserver(self, selector: #selector(sheetDidEnd(_:)), name: NSWindow.didEndSheetNotification, object: selected)
                }
                onWindow(selected)
            }
            publish()
        }

        func stop() {
            guard !stopped else { return }
            stopped = true
            NotificationCenter.default.removeObserver(self)
            closeSurface()
            window = nil
            onWindow(nil)
        }

        @objc private func applicationChanged(_ notification: Notification) {
            publish()
        }

        @objc private func windowChanged(_ notification: Notification) {
            guard let source = notification.object as? NSWindow, source === window else { return }
            publish()
        }

        @objc private func windowWillClose(_ notification: Notification) {
            guard let source = notification.object as? NSWindow, source === window else { return }
            closeSurface()
        }

        @objc private func sheetWillBegin(_ notification: Notification) {
            guard let source = notification.object as? NSWindow, source === window else { return }
            sheetBeginning = true
            publish()
        }

        @objc private func sheetDidEnd(_ notification: Notification) {
            guard let source = notification.object as? NSWindow, source === window else { return }
            sheetBeginning = false
            publish()
        }

        @objc private func windowBecameKey(_ notification: Notification) {
            guard !stopped, let source = notification.object as? NSWindow, source === window else { return }
            if source.isVisible && !source.isMiniaturized {
                closing = false
                closeDelivered = false
            }
            publish()
        }

        private func closeSurface() {
            closing = true
            publish()
            guard !closeDelivered else { return }
            closeDelivered = true
            onClose()
        }

        private func publish() {
            let covered = sheetBeginning || window.map { $0.attachedSheet != nil || !$0.sheets.isEmpty } == true
            let visible = !stopped && !closing && !NSApplication.shared.isHidden
                && window.map { $0.isVisible && !$0.isMiniaturized && ($0.occlusionState.contains(.visible) || covered) } == true
            let state = State(visible: visible, active: NSApplication.shared.isActive, covered: covered)
            guard state != previous else { return }
            previous = state
            onChange(state.visible, state.active, state.covered)
        }
    }
}

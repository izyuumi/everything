import AppKit
import SwiftUI

@MainActor
struct SettingsAssistantView: View {
    @Bindable var controller: SettingsAssistantController

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    sessionControls
                    if controller.controlsAreStale {
                        Text("Host settings are stale or offline. Read fresh Host settings in the conventional controls before confirming changes.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    if controller.controlsConflict {
                        Text("The shared Host draft has a conflict. Reload and review it in the conventional controls; Ask Settings does not rebase your edits.")
                            .font(.caption).foregroundStyle(.orange)
                    }
                    if let issue = controller.issue {
                        Text(verbatim: issue.errorDescription ?? "Ask Settings is unavailable.")
                            .foregroundStyle(.red)
                    }
                    if controller.canPublish {
                        privateContent
                    }
                    originalTracking
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(16)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .inspectorColumnWidth(min: 300, ideal: 360, max: 420)
        .background {
            SettingsAssistantWindowReader { controller.attachWindow($0) }
                .frame(width: 0, height: 0)
        }
        .privacySensitive()
    }

    private var header: some View {
        HStack {
            Text("Ask Settings").font(.headline).accessibilityAddTraits(.isHeader)
            Spacer()
            Button {
                controller.setExpanded(false)
            } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Close Ask Settings")
            .help("Close Ask Settings and request cleanup of its original child session")
        }
        .padding(16)
    }

    private var sessionControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(verbatim: phaseDescription).font(.subheadline.bold())
            Text("Ask Settings is optional. Native Settings controls remain available independently.")
                .font(.caption).foregroundStyle(.secondary)
            if controller.phase == .unavailable {
                Text("The Host reported Ask Settings unavailable: no usable model and credential are configured, or it cannot start another Settings chat now. No other provider is selected automatically. Check Models, then open Ask Settings again.")
                    .font(.caption).foregroundStyle(.orange)
            }
            Button("Open Ask Settings") { controller.open() }
                .disabled(!controller.canOpen)
            Text("Open reads the configured recipient and tools without sending a question or starting inference. First open Host settings using the conventional controls.")
                .font(.caption).foregroundStyle(.secondary)
            if controller.hasCurrentContext {
                HStack {
                    Button("Inspect original Open") { controller.inspectOriginalOpen() }
                        .disabled(!controller.canInspectOpen)
                    Button("Cancel") { controller.cancel() }
                }
                if controller.canRecoverOpenedChild {
                    Button("Recover original Open") { controller.recoverOpenedChild() }
                }
            }
        }
    }

    @ViewBuilder
    private var privateContent: some View {
        if let disclosure = controller.disclosure {
            disclosureView(disclosure)
            composer
            if let review = controller.reviewText {
                reviewQuestion(review, disclosure: disclosure)
            }
        }
        if let question = controller.submittedQuestion {
            textGroup("Current question", question)
        }
        if !controller.committed.isEmpty {
            textGroup("Committed assistant text", controller.committed)
        }
        if !controller.provisional.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text(controller.provisionalState == .interrupted ? "Interrupted provisional text" : "Streaming provisional text")
                    .font(.subheadline.bold())
                Text(verbatim: controller.provisional)
                Text("Provisional text does not establish a completed answer or a saved change.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        if let summary = controller.summary { usage(summary) }
        if !controller.cards.isEmpty {
            VStack(alignment: .leading, spacing: 10) {
                Text("Native actions and receipts").font(.headline).accessibilityAddTraits(.isHeader)
                Text("Only authenticated service receipts can provide these actions. Assistant text does not authorize changes.")
                    .font(.caption).foregroundStyle(.secondary)
                ForEach(controller.cards) { card in
                    SettingsAssistantOperationView(controller: controller, card: card)
                }
            }
        }
        if !controller.recent.isEmpty {
            VStack(alignment: .leading, spacing: 12) {
                Text("Recent completed turns").font(.headline).accessibilityAddTraits(.isHeader)
                ForEach(controller.recent) { turn in
                    GroupBox {
                        VStack(alignment: .leading, spacing: 8) {
                            textGroup("Question", turn.question)
                            textGroup("Committed answer", turn.answer)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
        }
        HStack {
            Button("Refresh original status") { controller.refresh() }
            Button("Clear and close") { controller.clear() }
        }
        Text("Questions and answers are temporary in this pane. Closing, hiding or leaving the foreground clears them and requests original-session cleanup. This does not undo an admitted change.")
            .font(.caption).foregroundStyle(.secondary)
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Question").font(.subheadline.bold())
            TextEditor(text: Binding(get: { controller.composer }, set: { controller.setComposer($0) }))
                .font(.body)
                .frame(minHeight: 90, maxHeight: 160)
                .scrollContentBackground(.hidden)
                .padding(6)
                .background(.background, in: RoundedRectangle(cornerRadius: 6))
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color(nsColor: .separatorColor)))
                .disabled(!controller.canReview)
                .accessibilityLabel("Ask Settings question")
            Text(verbatim: String(controller.composer.utf8.count) + " of 8192 UTF-8 bytes")
                .font(.caption).foregroundStyle(.secondary)
            if let issue = controller.inputIssue {
                Text(verbatim: issue).font(.caption).foregroundStyle(.red)
            }
            Button("Review question and recipient") { controller.reviewQuestion() }
                .disabled(!controller.canReview || controller.composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
    }

    private func disclosureView(_ disclosure: NativeSettingsAssistantWire.Opened) -> some View {
        GroupBox("Configured recipient") {
            VStack(alignment: .leading, spacing: 8) {
                labeledText("Provider", disclosure.provider.rawValue)
                labeledText("Model", disclosure.model)
                labeledText("Recipient", disclosure.recipient)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Restricted tools").font(.caption).foregroundStyle(.secondary)
                    ForEach(disclosure.tools, id: \.self) { tool in
                        Text(verbatim: tool).font(.caption.monospaced())
                    }
                }
                Text("Every Send shares your question, restricted recent Settings context and service results with this recipient. Review and confirm each question separately.")
                    .font(.caption).foregroundStyle(.secondary)
                Text("Do not paste API keys, passwords or other secrets. Use native credential entry in Models or Tool Selection.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func reviewQuestion(_ text: String, disclosure: NativeSettingsAssistantWire.Opened) -> some View {
        GroupBox("Review this Send") {
            VStack(alignment: .leading, spacing: 10) {
                Text(verbatim: text)
                disclosureView(disclosure)
                Text("Confirm sending this exact question and restricted Settings context to the displayed recipient. Editing the question requires a new review.")
                    .font(.caption).foregroundStyle(.secondary)
                Button("Send to displayed recipient") { controller.sendReviewedQuestion() }
                    .disabled(!controller.canSend)
                Button("Discard review") { controller.discardReview() }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func usage(_ summary: SettingsAssistantController.Summary) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Original turn status").font(.subheadline.bold())
            Text(verbatim: "Provider complete: " + (summary.providerComplete ? "Yes" : "No"))
            Text(verbatim: "Usage complete: " + (summary.usageComplete ? "Yes" : "No"))
            Text(verbatim: "Model attempts: " + String(summary.attempts))
            Text(verbatim: "Charged tokens: " + String(summary.chargedTokens))
            Text(verbatim: "Reserved output tokens: " + String(summary.reservedOutputTokens))
            if let context = summary.context {
                Text(verbatim: "Context revision: " + String(context.revision))
                Text(verbatim: "Context read: " + Date(timeIntervalSince1970: Double(context.observedAt) / 1000).formatted())
                Text(context.stale ? "This turn used a stale point-in-time context." : "This turn used a point-in-time context; it is not a current Settings read.")
            }
            Text("Provider and usage completion do not establish that a native action succeeded. Read the original operation receipt.")
                .foregroundStyle(.secondary)
        }
        .font(.caption)
    }

    @ViewBuilder
    private var originalTracking: some View {
        let retained = controller.cleanup
        if !retained.isEmpty && (!controller.canPublish || retained.contains(where: { controller.canRetryOriginalClose($0.id) })) {
            GroupBox("Original request tracking") {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(retained) { original in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(verbatim: original.requestID.uuidString.lowercased()).font(.caption.monospaced())
                            Text(original.closed ? "Closure acknowledged for this original request." : "This original request is retained; closure has not been acknowledged.")
                                .font(.caption)
                            if original.parentRetired {
                                Text("The retired native Settings owner owns its remaining cleanup.")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            if let issue = original.issue {
                                Text(verbatim: issue.errorDescription ?? "The original cleanup outcome is unknown.")
                                    .font(.caption).foregroundStyle(.red)
                            }
                            if controller.canRetryOriginalClose(original.id) {
                                Button("Close original request") { controller.retryOriginalClose(original.id) }
                            }
                        }
                    }
                    Button("Read retained cleanup metadata") { controller.reconcileCleanup() }
                    Text("An unknown outcome cannot be replaced by another child on the same native Settings owner.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private func textGroup(_ label: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(verbatim: label).font(.subheadline.bold())
            Text(verbatim: text)
        }
    }

    private func labeledText(_ label: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(verbatim: label).font(.caption).foregroundStyle(.secondary)
            Text(verbatim: text)
        }
    }

    private var phaseDescription: String {
        if controller.hasCurrentContext, !controller.canPublish, controller.phase == .ready || controller.phase == .completed {
            return "Original session retained; private presentation is unavailable"
        }
        return switch controller.phase {
        case .idle: "Closed; open explicitly to read the recipient"
        case .opening: "Opening the original child session"
        case .ready: "Ready for a reviewed question"
        case .sending: "Sending the original question"
        case .accepted: "The original question was accepted; waiting for its status"
        case .running: "The original turn is running"
        case .completed: "The original turn completed"
        case .paused: "Private presentation is paused"
        case .closing: "Closing the original child session"
        case .closed: "Original child closure acknowledged"
        case .unavailable: "Ask Settings is unavailable"
        case .stoppedUncertain: "Original provider outcome uncertain; no automatic resend"
        case .stoppedUnusable: "Original provider response unusable; no automatic resend"
        case .unknown: "Original outcome unknown; inspect before continuing"
        }
    }
}

@MainActor
private struct SettingsAssistantWindowReader: NSViewRepresentable {
    let onChange: @MainActor (NSWindow?) -> Void

    func makeNSView(context: Context) -> WindowView {
        let view = WindowView()
        view.onChange = onChange
        return view
    }

    func updateNSView(_ view: WindowView, context: Context) { view.onChange = onChange }

    static func dismantleNSView(_ view: WindowView, coordinator: ()) {
        view.onChange?(nil)
        view.onChange = nil
    }

    @MainActor
    final class WindowView: NSView {
        var onChange: (@MainActor (NSWindow?) -> Void)?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            onChange?(window)
        }
    }
}

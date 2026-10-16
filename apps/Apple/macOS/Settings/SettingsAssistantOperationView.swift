import SwiftUI

@MainActor
struct SettingsAssistantOperationView: View {
    let controller: SettingsAssistantController
    @Bindable var card: SettingsAssistantController.Card

    var body: some View {
        if controller.canPublish {
            GroupBox {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Authenticated Settings operation").font(.headline)
                    Text(verbatim: card.id.uuidString.lowercased())
                        .font(.caption.monospaced())
                    if let inspection = card.inspection {
                        Text("Last scoped observation").font(.caption).foregroundStyle(.secondary)
                        Text(verbatim: phase(inspection))
                        if let write = inspection.write {
                            Text(verbatim: "Original write revision: " + String(write.expectedRevision) + " → " + String(write.revision))
                                .font(.caption)
                            Text(verbatim: writeResolution(write.resolution)).font(.caption)
                        }
                        if let application = inspection.application {
                            Text(verbatim: NativeSettingsText.application(application)).font(.caption)
                        }
                    } else {
                        Text("Inspect this original operation to read its authenticated status and available action.")
                            .foregroundStyle(.secondary)
                    }
                    if let operation = controller.nativeOperation(card), case .patch(let patch) = operation.target {
                        patchDetails(patch, operation: operation)
                    } else if let receipt = card.receipt {
                        action(receipt.details.target)
                    }
                    if let issue = card.issue {
                        Text(verbatim: issue.errorDescription ?? "The original operation could not be inspected.")
                            .foregroundStyle(.red)
                    }
                    if card.isWorking { ProgressView("Reading the original operation…") }
                    Button("Inspect authenticated operation") { controller.inspect(card) }
                        .disabled(card.isWorking || !controller.canPublish)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .privacySensitive()
        }
    }

    @ViewBuilder
    private func action(_ target: SettingsActionTarget) -> some View {
        switch target {
        case .patch:
            if let operation = controller.nativeOperation(card), case .patch(let patch) = operation.target {
                patchDetails(patch, operation: operation)
            } else {
                Text("The original settings snapshot is unavailable. Refresh Host settings in the conventional controls, then inspect this original operation again.")
                    .foregroundStyle(.secondary)
            }
        case .device(let request):
            deviceDetails(request)
        case .native(let request):
            if case .openSection(let section) = request.action {
                Text(verbatim: "Requested section: " + sectionName(section))
                Button("Open Settings section…") { controller.presentSection(card) }
                    .disabled(!controller.navigationAvailable || !controller.canPublish || card.isWorking)
                if !controller.navigationAvailable {
                    Text("Use the native Settings sections to continue. Section navigation is unavailable in this presentation.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            } else {
                Text("Continue through the conventional native Settings controls.")
                    .foregroundStyle(.secondary)
            }
        case .ownerReplacement:
            Text("Continue through the conventional native Settings controls.")
                .foregroundStyle(.secondary)
        }
    }

    private func patchDetails(_ patch: SettingsPatch, operation: NativeSettingsOperation) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(verbatim: "Original revision: " + String(patch.expectedRevision))
                .font(.caption)
            ForEach(patch.changes, id: \.key.rawValue) { change in
                VStack(alignment: .leading, spacing: 4) {
                    Text(verbatim: NativeSettingsText.label(change.key)).font(.subheadline.bold())
                    value("Original saved value", NativeSettingsText.value(operation.originalSnapshot.entry(change.key)?.desired))
                    value("Proposed value", NativeSettingsText.value(change.value))
                    if let descriptor = operation.originalRegistry.descriptors.first(where: { $0.key == change.key }) {
                        Text(verbatim: "Applies: " + applyBehavior(descriptor.applyBehavior))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            Text("Native original operation").font(.caption).foregroundStyle(.secondary)
            Text(verbatim: NativeSettingsText.operation(operation))
            if let issue = operation.issue {
                Text(verbatim: issue.errorDescription ?? "Inspect the original native operation.")
                    .foregroundStyle(.red)
            }
            if operation.isWorking { ProgressView("Waiting for the original native operation…") }
            if let reference = operation.confirmation {
                Button("Confirm original changes") { controller.confirm(card) }
                    .disabled(!controller.canConfirm(card) || operation.isWorking)
                Text(verbatim: "Original confirmation expires " + Date(timeIntervalSince1970: Double(reference.expiresAt) / 1000).formatted() + ". These exact changes use the original revision.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            VStack(alignment: .leading, spacing: 6) {
                Button("Inspect original write") { controller.inspectNative(card) }
                    .disabled(operation.isWorking || !operation.wasSubmitted || !controller.canPublish)
                if operation.canReconcile {
                    Button("Reconcile original write") { controller.reconcileNative(card) }
                        .disabled(!controller.canPublish)
                }
            }
            if operation.blocksNewChanges {
                Button("Cancel original request") { controller.cancelNative(card) }
                    .disabled(operation.isWorking || !controller.canPublish)
            }
        }
    }

    @ViewBuilder
    private func deviceDetails(_ request: SettingsDeviceRequest) -> some View {
        switch request.action {
        case .pair:
            Text("Pairing requires the native invitation, fingerprint, access review and separate final approval.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review device pairing…") { controller.presentDevice(card) }
                .disabled(!controller.canPublish || card.isWorking)
        case .revoke(let deviceID, let publicKeys):
            Text("Requested device revocation").font(.subheadline.bold())
            Text(verbatim: deviceID.uuidString.lowercased()).font(.caption.monospaced())
            Text(verbatim: publicKeys.fingerprint).font(.caption.monospaced())
            Text("Review the exact device and keys in the native ceremony before confirming.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Review device revocation…") { controller.presentDevice(card) }
                .disabled(!controller.canPublish || card.isWorking)
        }
    }

    private func value(_ label: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(verbatim: label).font(.caption).foregroundStyle(.secondary)
            Text(verbatim: text)
        }
    }

    private func writeResolution(_ resolution: SettingsWriteResolution?) -> String {
        guard let resolution else {
            return "Original write outcome unresolved. Inspect or reconcile the retained original operation before another save."
        }
        return switch resolution {
        case .applied: "Original write applied; application status is separate"
        case .notApplied: "Original write was not applied"
        }
    }

    private func phase(_ inspection: SettingsOperationInspection) -> String {
        switch inspection.phase {
        case .awaitingConfirmation: "Awaiting confirmation of the original changes"
        case .awaitingAction: "Awaiting a native action"
        case .inFlight: "Original native action is in flight"
        case .applying(let expected, let revision): "Saving original revision \(expected) → \(revision); inspect before another save"
        case .finished(let result): NativeSettingsText.result(result.state)
        }
    }

    private func applyBehavior(_ behavior: SettingApplyBehavior) -> String {
        switch behavior {
        case .subsequentRun: "Subsequent runs"
        case .subsequentSelection: "Subsequent tool selection"
        case .stopNewWorkThenApply: "After new work stops"
        case .rebuildRequired: "Index rebuild required"
        case .readOnly: "Read-only observation"
        case .thisDevice: "This device"
        }
    }

    private func sectionName(_ section: SettingsSection) -> String {
        switch section {
        case .general: "General"
        case .models: "Models"
        case .memory: "Memory"
        case .toolSelection: "Tool Selection"
        case .notifications: "Notifications"
        case .devices: "Devices"
        case .ownerSecurity: "Owner Security"
        }
    }
}

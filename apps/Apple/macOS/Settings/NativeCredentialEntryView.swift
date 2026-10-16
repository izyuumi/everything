import SwiftUI

@MainActor
struct NativeCredentialEntryControls: View {
    enum Purpose { case model, systemOne }

    let store: NativeSettingsStore
    let purpose: Purpose
    let connectionEdited: Bool
    @State private var storage = SettingsCredentialStorage.session

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let metadata = store.metadata, metadata.revision == store.snapshot?.revision {
                switch purpose {
                case .model:
                    if let selected = metadata.defaultModel,
                       !metadata.modelCredentials.contains(where: { $0.provider == selected.provider && $0.endpoint == selected.endpoint }) {
                        Picker("Storage for new credential", selection: $storage) {
                            Text("This Host session").tag(SettingsCredentialStorage.session)
                            Text("Native credential store").tag(SettingsCredentialStorage.native)
                        }
                        Button("Add credential for saved model endpoint…") { store.beginCredentialEntry(.selectedModel(storage: storage)) }
                            .disabled(!store.canEnterCredential || connectionEdited)
                    }
                    ForEach(Array(metadata.modelCredentials.enumerated()), id: \.offset) { _, binding in
                        Button("Replace \(binding.provider.rawValue) credential…") { store.beginCredentialEntry(.configuredModel(binding)) }
                            .disabled(!store.canEnterCredential || connectionEdited)
                            .help("Replaces the exact configured reference and storage shown above; no metadata write.")
                    }
                case .systemOne:
                    if let configured = metadata.systemOne {
                        Button(configured.reference == nil ? "Add System One credential…" : "Replace System One credential…") {
                            store.beginCredentialEntry(.systemOne)
                        }
                        .disabled(!store.canEnterCredential || connectionEdited)
                    }
                }
            }
            if let issue = store.credentialIssue { Text(issue.message).foregroundStyle(.red) }
            if store.capacityReached {
                Text("The eight retained-owner limit is reached. Original cleanup and historical unlinked references are preserved; a new Host session cannot be opened.")
                    .foregroundStyle(.orange)
            }
            Text("Save or discard configuration drafts first. Key entry uses the exact saved binding and a separate native action; it does not change a provider, endpoint or enable state.")
                .font(.caption).foregroundStyle(.secondary)
            ForEach(store.credentialOperations.filter { matches($0) }) { operation in credentialOperation(operation) }
        }
        .sheet(item: presentationBinding) { presentation in
            NativeCredentialEntrySheet(store: store, presentation: presentation)
        }
    }

    private var presentationBinding: Binding<NativeSettingsStore.CredentialPresentation?> {
        Binding(get: {
            guard let presentation = store.credentialPresentation, matches(presentation.request) else { return nil }
            return presentation
        }, set: { value in
            if value == nil, let original = store.credentialPresentation, matches(original.request) { store.dismissCredential(original) }
        })
    }

    private func matches(_ operation: NativeCredentialOperation) -> Bool { matches(operation.request) }

    private func matches(_ request: SettingsActionRequest) -> Bool {
        guard case .credentialEntry(let binding) = request.action else { return false }
        switch (purpose, binding) {
        case (.model, .model), (.systemOne, .systemOne): return true
        default: return false
        }
    }

    private func credentialOperation(_ operation: NativeCredentialOperation) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(NativeSettingsText.operation(operation.operation)).font(.headline)
            Text("Original storage operation: \(operation.id.uuidString.lowercased())").font(.caption).textSelection(.enabled)
            Text("Original Host: \(operation.request.scope.identifier.uuidString.lowercased())").font(.caption).textSelection(.enabled)
            if let binding = operation.binding {
                switch binding {
                case .model(let model):
                    Text("\(model.provider.rawValue) · \(endpoint(model.endpoint)) · \(model.storage.rawValue)").font(.caption)
                    Text("Original reference: \(model.reference.uuidString.lowercased())").font(.caption).textSelection(.enabled)
                case .systemOne(let endpoint, let reference):
                    Text("System One · \(endpoint) · native storage").font(.caption)
                    Text("Original reference: \(reference.uuidString.lowercased())").font(.caption).textSelection(.enabled)
                }
            }
            if operation.storageCompleted {
                Text(operation.initial ? "Original storage completed; metadata linking is separate." : "Original configured replacement completed; Host settings were not changed.")
                Text("This historical receipt does not establish current credential presence. Session credentials can be lost on Host restart.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let issue = operation.issue { Text(issue.message).foregroundStyle(.red) }
            if let issue = operation.operation.issue { Text(issue.errorDescription ?? "Inspect the original storage operation.").foregroundStyle(.red) }
            linkControls(operation)
            Text("Reference and original result are metadata. No key deletion or presence query is provided.").font(.caption).foregroundStyle(.secondary)
        }
        .padding(.vertical, 8)
    }

    private func endpoint(_ value: SettingsModelEndpoint) -> String {
        switch value {
        case .official: "Official provider endpoint"
        case .loopback(let url): url
        }
    }

    @ViewBuilder
    private func linkControls(_ operation: NativeCredentialOperation) -> some View {
        switch operation.linkState {
        case .notRequested:
            if operation.initial && operation.storageCompleted {
                Text("Stored, not linked. The key may remain stored if metadata cannot be linked.").foregroundStyle(.orange)
                Button("Link this stored reference to original revision") { store.linkCredentialMetadata(operation.handle) }
                    .disabled(!store.canLinkCredential(operation.handle) || connectionEdited)
            }
        case .notSubmitted:
            Text("The original metadata candidate was not submitted. The stored key has not been rolled back.").foregroundStyle(.orange)
        case .working: ProgressView("Waiting for the original metadata operation…")
        case .unconfirmed: Text("Metadata outcome is unconfirmed. Inspect the original owner operation; do not submit another key.").foregroundStyle(.orange)
        case .observed(let receipt):
            switch receipt.fileOutcome {
            case .unsettled: Text("The original metadata file outcome remains unknown.").foregroundStyle(.orange)
            case .notApplied: Text("The metadata link was not applied. The stored key may remain unlinked.").foregroundStyle(.orange)
            case .applied(let revision): Text("Original reference metadata linked at revision \(revision).")
            }
            if let application = receipt.application { Text(NativeSettingsText.application(application)).font(.caption) }
            else if receipt.applicationPending { Text("Saved metadata; application is still pending.").font(.caption) }
        }
        if let id = operation.linkRequestID {
            Text("Original metadata operation: \(id.uuidString.lowercased())").font(.caption).textSelection(.enabled)
            HStack {
                Button("Inspect original metadata") { store.inspectCredentialLink(operation.handle) }
                    .disabled(!store.canRecoverCredentialLink(operation.handle))
                if case .observed(let receipt) = operation.linkState, receipt.operation.write != nil, receipt.operation.write?.resolution == nil {
                    Button("Reconcile original metadata") { store.reconcileCredentialLink(operation.handle) }
                        .disabled(!store.canRecoverCredentialLink(operation.handle))
                }
            }
        }
    }
}

@MainActor
private struct NativeCredentialEntrySheet: View {
    let store: NativeSettingsStore
    let presentation: NativeSettingsStore.CredentialPresentation
    @State private var field = NativeCredentialField()
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Enter credential").font(.title2)
            if case .credentialEntry(let binding) = presentation.request.action {
                switch binding {
                case .model(let model):
                    LabeledContent("Provider", value: model.provider.rawValue)
                    LabeledContent("Endpoint", value: endpoint(model.endpoint))
                    LabeledContent("Reference", value: model.reference.uuidString.lowercased())
                    LabeledContent("Storage", value: model.storage == .session ? "Owning Host memory" : "Native credential store")
                case .systemOne(let endpoint, let reference):
                    LabeledContent("Purpose", value: "System One")
                    LabeledContent("Endpoint", value: endpoint)
                    LabeledContent("Reference", value: reference.uuidString.lowercased())
                    LabeledContent("Storage", value: "Native credential store")
                }
            }
            LabeledContent("Original settings revision", value: String(presentation.request.expectedRevision))
            LabeledContent("Original expiry", value: Date(timeIntervalSince1970: Double(presentation.reference.expiresAt) / 1000).formatted())
            SecureField("Credential", text: Binding(get: { field.value }, set: { value in field.value = value }))
                .textContentType(.password)
                .privacySensitive()
                .focused($focused)
                .onSubmit { submit() }
            Text("The field is masked. Submission uses the original native action and private pipe. Clearing the field does not guarantee erasure of Swift or OS memory.")
                .font(.caption).foregroundStyle(.secondary)
            if let issue = store.credentialPresentationIssue { Text(issue.message).foregroundStyle(.red) }
            HStack {
                Button("Cancel", role: .cancel) { field.clear(); store.dismissCredential(presentation) }.keyboardShortcut(.cancelAction)
                Spacer()
                Button("Store credential") { submit() }.keyboardShortcut(.defaultAction)
                    .disabled(field.value.isEmpty || store.snapshotIsStale)
            }
        }
        .padding(24)
        .frame(width: 570)
        .onAppear { store.attachCredentialField(field, to: presentation); focused = true }
        .onDisappear { field.clear(); store.dismissCredential(presentation) }
        .background {
            TimelineView(.periodic(from: Date(), by: 1)) { context in
                Color.clear.onChange(of: context.date) { _, _ in store.expireCredentialPresentation(presentation) }
            }
        }
    }

    private func submit() {
        let secret = field.take()
        store.submitCredential(presentation, secret: secret)
    }

    private func endpoint(_ value: SettingsModelEndpoint) -> String {
        switch value {
        case .official: "Official provider endpoint"
        case .loopback(let url): url
        }
    }
}

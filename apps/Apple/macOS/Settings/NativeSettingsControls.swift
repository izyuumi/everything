import SwiftUI

struct NativeSettingsNotice: View {
    enum Kind {
        case error
        case warning
    }

    let text: String
    let kind: Kind

    init(_ text: String, kind: Kind = .error) {
        self.text = text
        self.kind = kind
    }

    var body: some View {
        Label(text, systemImage: kind == .error ? "exclamationmark.octagon" : "exclamationmark.triangle")
            .foregroundStyle(kind == .error ? Color.red : Color.orange)
            .accessibilityLabel((kind == .error ? "Error: " : "Warning: ") + text)
    }
}

@MainActor
struct NativeSettingsControls: View {
    enum SectionKind {
        case models
        case memory
        case toolSelection
    }

    let store: NativeSettingsStore
    let section: SectionKind
    let connectionEdited: Bool

    var body: some View {
        Form {
            sessionControls
            if let draft = store.draft {
                Group {
                    switch section {
                    case .models: models(draft)
                    case .memory: memory(draft)
                    case .toolSelection: tools(draft)
                    }
                }
                .disabled(!store.canEdit || connectionEdited)
                saveControls(draft)
            } else {
                Section {
                    Text("Open Host settings to read the current configuration before editing. Local notification preferences remain available independently.")
                        .foregroundStyle(.secondary)
                }
            }
            switch section {
            case .models:
                Section("Native credential entry") {
                    NativeCredentialEntryControls(store: store, purpose: .model, connectionEdited: connectionEdited)
                }
            case .toolSelection:
                Section("Native credential entry") {
                    NativeCredentialEntryControls(store: store, purpose: .systemOne, connectionEdited: connectionEdited)
                }
            case .memory: EmptyView()
            }
            operationControls
        }
        .formStyle(.grouped)
        .padding(12)
    }

    private var sessionControls: some View {
        Section("Host connection") {
            LabeledContent("Session", value: store.status)
            HStack {
                Button("Open Host settings") { store.open() }
                    .disabled(!store.canOpen || connectionEdited)
                Button("Refresh") { store.refresh() }
                    .disabled(!store.canRefresh || connectionEdited)
                Button("Close session") { store.close() }.disabled(!store.sessionPresent)
            }
            if store.isOpening || store.isRefreshing { ProgressView("Reading the original Host session…") }
            if let issue = store.issue { NativeSettingsNotice(issue.errorDescription ?? "Host settings are unavailable.") }
            if connectionEdited { Text("Apply connection edits with Connect in Host before accessing Host settings.").foregroundStyle(.secondary) }
            if let snapshot = store.snapshot {
                LabeledContent("Observed revision", value: String(snapshot.revision))
                if store.snapshotIsStale { Text("Offline or stale observation. Saving is disabled until the active original session reads fresh Host settings.").foregroundStyle(.secondary) }
            }
            if let observed = store.observedAt { Text("Last read \(observed.formatted())").font(.caption).foregroundStyle(.secondary) }
            Text("Open is explicit and expires. Refresh and foreground changes do not renew authority. Closing Settings leaves the independent Host running.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func models(_ draft: NativeSettingsDraft) -> some View {
        Section("Default model for new runs") {
            Toggle("Select a default model", isOn: binding(\.modelEnabled, fallback: false))
            Group {
                Picker("Provider", selection: binding(\.provider, fallback: .anthropic)) {
                    Text("Anthropic").tag(SettingsProviderKind.anthropic)
                    Text("OpenAI").tag(SettingsProviderKind.openAI)
                }
                Picker("Endpoint", selection: binding(\.endpoint, fallback: .official)) {
                    Text("Official").tag(NativeSettingsDraft.Endpoint.official)
                    Text("Local loopback").tag(NativeSettingsDraft.Endpoint.loopback)
                }
                if draft.endpoint == .loopback { TextField("Loopback URL", text: binding(\.loopbackURL, fallback: "")) }
                TextField("Model ID", text: binding(\.model, fallback: ""))
                Text("Use the provider's model ID. The Host validates local endpoint policy. Changes affect subsequent runs; existing tasks retain their captured configuration.")
                    .font(.caption).foregroundStyle(.secondary)
            }.disabled(!draft.modelEnabled)
            observed(.modelSelection)
            Text("Effective here describes configuration. Provider availability and successful inference are not established by this settings read.")
                .font(.caption).foregroundStyle(.secondary)
        }
        Section("Configured credential references") {
            if let metadata = store.metadata, metadata.revision == store.snapshot?.revision {
                if metadata.modelCredentials.isEmpty { Text("No model credential references are configured in this observed revision.") }
                ForEach(Array(metadata.modelCredentials.enumerated()), id: \.offset) { _, credential in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(credential.provider.rawValue + " · " + endpoint(credential.endpoint))
                        Text(credential.reference.uuidString.lowercased()).font(.caption).textSelection(.enabled)
                        Text("Storage: \(credential.storage.rawValue). Secret presence was not queried.").font(.caption).foregroundStyle(.secondary)
                    }
                }
            } else { Text("Credential reference metadata is unavailable for the observed revision.").foregroundStyle(.secondary) }
            Text("This pane edits public model configuration. Credential entry is a separate native action.").font(.caption).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func memory(_ draft: NativeSettingsDraft) -> some View {
        Section("Local semantic retrieval") {
            Toggle("Enable local embeddings", isOn: binding(\.embeddingsEnabled, fallback: false))
            if let choices = draft.dimensionChoices {
                Picker("Dimensions", selection: binding(\.dimensions, fallback: nil)) {
                    Text("Model default").tag(Optional<UInt32>.none)
                    ForEach(choices, id: \.self) { dimension in Text(String(dimension)).tag(Optional(dimension)) }
                }
            } else {
                LabeledContent("Dimensions", value: draft.dimensions.map { String($0) } ?? "Model default")
                Text("The Host has not reported model metadata. Dimension changes are unavailable.").foregroundStyle(.secondary)
            }
            if let behavior = draft.registry.descriptors.first(where: { $0.key == .embeddingsDimensions })?.applyBehavior {
                LabeledContent("Dimension change", value: NativeSettingsText.applyBehavior(behavior))
            }
            Text("Embeddings are off by default. When enabled, the Host uses its optional local embedding helper only if it is available; the states below report actual readiness. Lexical retrieval remains available when the semantic backend is unavailable.")
                .font(.caption).foregroundStyle(.secondary)
            observed(.embeddingsEnabled)
            observed(.embeddingsDimensions)
            observed(.embeddingsModel)
        }
    }

    @ViewBuilder
    private func tools(_ draft: NativeSettingsDraft) -> some View {
        Section("Tool discovery and selection") {
            Picker("System One provider", selection: binding(\.systemOneChoice, fallback: .local)) {
                ForEach(SystemOneChoice.allCases) { choice in Text(choice.title).tag(Optional(choice)) }
            }
            .disabled(systemOneConfigurationUnknown(draft))
            if let rejection = store.systemOneRejection { NativeSettingsNotice(rejection) }
            if systemOneConfigurationUnknown(draft) {
                Text("The retained System One configuration was not observed for this draft's revision. Reload before choosing a provider so a saved endpoint and its credential reference are not replaced.")
                    .foregroundStyle(.secondary)
            }
            Text("Local is the default and sends nothing. Other providers are optional remote tool selection: each request is shown for your approval before the task summary and tool descriptions are sent.")
                .font(.caption).foregroundStyle(.secondary)
            if draft.systemOneEnabled { systemOneFields(draft) }
            observed(.toolSelection)
        }
        Section("Configured System One credential reference") {
            if let metadata = store.metadata, metadata.revision == store.snapshot?.revision {
                if let configured = metadata.systemOne {
                    LabeledContent("Configured endpoint", value: configured.endpoint)
                    LabeledContent("Reference", value: configured.reference?.uuidString.lowercased() ?? "None configured")
                    Text("Secret presence was not queried. A disabled System One configuration can retain its reference.").font(.caption).foregroundStyle(.secondary)
                } else { Text("No System One configuration was observed.") }
            } else { Text("Reference metadata is unavailable for the observed revision.").foregroundStyle(.secondary) }
        }
    }

    private func saveControls(_ draft: NativeSettingsDraft) -> some View {
        Section("Shared Host draft") {
            LabeledContent("Draft baseline", value: String(draft.baseline.revision))
            if store.draftConflict { NativeSettingsNotice("Host settings or descriptors changed since these edits began. Reload and review; these drafts have not been rebased or submitted.", kind: .warning) }
            if let message = draft.validationMessage, draft.isDirty { NativeSettingsNotice(message, kind: .warning) }
            if let message = store.draftMessage { NativeSettingsNotice(message) }
            if draft.baseline.revision == UInt64(Int64.max) { Text("The settings revision is exhausted; further saves are unavailable.").foregroundStyle(.secondary) }
            if store.actionCapacityReached { Text("The original operation limit is reached. Retained operations are preserved; fresh requests require a new explicit session after cleanup.").foregroundStyle(.secondary) }
            HStack {
                Button("Save all Host changes") { store.save() }.disabled(!store.canSave || connectionEdited)
                Button("Reload and discard drafts") { store.reloadDraft() }.disabled(!store.canRefresh || store.operations.contains(where: \.isWorking) || connectionEdited)
            }
            Text("Models, Memory and Tool Selection share this draft. Saving retains one exact revision-checked request. Protected recipient changes require a separate confirmation of the original batch.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var operationControls: some View {
        Section("Original operations") {
            if store.operations.isEmpty { Text("No changes submitted in this session.").foregroundStyle(.secondary) }
            ForEach(store.operations) { operation in
                VStack(alignment: .leading, spacing: 8) {
                    Text(NativeSettingsText.operation(operation)).font(.headline)
                    Text(operation.id.uuidString.lowercased()).font(.caption).textSelection(.enabled)
                    if case .patch(let patch) = operation.target {
                        Text("Original revision \(patch.expectedRevision)").font(.caption)
                        ForEach(patch.changes, id: \.key.rawValue) { change in
                            LabeledContent(NativeSettingsText.label(change.key), value: NativeSettingsText.value(change.value))
                        }
                    }
                    if let application = operation.inspection?.application { Text(NativeSettingsText.application(application)).font(.caption) }
                    if let issue = operation.issue { NativeSettingsNotice(issue.errorDescription ?? "Inspect the original operation.") }
                    if operation.isWorking { ProgressView("Waiting for the original operation…") }
                    HStack {
                        if operation.confirmation != nil {
                            Button("Confirm original changes") { store.confirm(operation.id) }
                                .disabled(!store.canAct || operation.issue != nil || operation.target.expectedRevision != store.snapshot?.revision || connectionEdited)
                        }
                        Button("Inspect") { store.inspect(operation.id) }.disabled(operation.isWorking || !operation.wasSubmitted)
                        if operation.canReconcile { Button("Reconcile original write") { store.reconcile(operation.id) }.disabled(connectionEdited) }
                        if operation.blocksNewChanges { Button("Cancel original request") { store.cancel(operation.id) }.disabled(operation.isWorking) }
                    }
                    if let reference = operation.confirmation {
                        Text("Original approval expires \(Date(timeIntervalSince1970: Double(reference.expiresAt) / 1000).formatted()). Confirmation uses the original request, not subsequent draft edits.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                .padding(.vertical, 6)
            }
        }
    }

    @ViewBuilder
    private func observed(_ key: SettingKey) -> some View {
        if let entry = store.snapshot?.entry(key) {
            VStack(alignment: .leading, spacing: 4) {
                Text(NativeSettingsText.label(key)).font(.headline)
                LabeledContent("Saved", value: NativeSettingsText.value(entry.desired))
                LabeledContent("Effective", value: NativeSettingsText.value(entry.effective))
                LabeledContent("State", value: NativeSettingsText.state(entry.state))
                if let reason = entry.unavailableReason { Text(NativeSettingsText.unavailable(reason)).font(.caption).foregroundStyle(.secondary) }
            }
            .padding(.vertical, 4)
        }
    }

    @ViewBuilder
    private func systemOneFields(_ draft: NativeSettingsDraft) -> some View {
        let choice = draft.systemOneChoice
        if choice == nil {
            NativeSettingsNotice("The saved provider \u{201C}" + (draft.systemOneProvider ?? "") + "\u{201D} is not known to this app. Choose a provider above to replace it.", kind: .warning)
        }
        switch choice {
        case .openrouter?:
            LabeledContent("Endpoint", value: SystemOneChoice.openrouterEndpoint).textSelection(.enabled)
        case .openaiCompatible?:
            TextField("Base URL", text: binding(\.systemOneEndpoint, fallback: ""), prompt: Text("https://llm.example.com/v1"))
            Text("Enter the base URL, not the /chat/completions URL. The Host sends requests to <base URL>/chat/completions.")
                .font(.caption).foregroundStyle(.secondary)
        case .localModel?:
            TextField("Loopback base URL", text: binding(\.systemOneEndpoint, fallback: ""), prompt: Text("http://127.0.0.1:11434/v1"))
            Text("A model server on this Mac at a literal loopback IP, such as Ollama, llama.cpp or LM Studio. Use the base URL, not the /chat/completions URL.")
                .font(.caption).foregroundStyle(.secondary)
        default:
            TextField("Endpoint", text: binding(\.systemOneEndpoint, fallback: ""), prompt: Text(SystemOneChoice.typesafeEndpoint))
        }
        TextField("Model ID", text: binding(\.systemOneModel, fallback: ""),
                  prompt: Text(choice == .openrouter ? "openai/gpt-4.1-mini" : choice == .localModel ? "llama3.2" : "MODEL_ID"))
        if let issue = draft.systemOneIssue { NativeSettingsNotice(issue, kind: .warning) }
        Group {
            if !(choice?.usesKey ?? true) {
                Text("A local model uses no key.")
            } else if let key = systemOneKeyNote(draft) {
                Text(key)
            } else {
                Text("Add or replace the key under Native credential entry below." + (choice == .openaiCompatible ? " A key is optional for this provider." : ""))
            }
        }
        .font(.caption).foregroundStyle(.secondary)
        Text("The model ID is opaque. The Host validates endpoint policy again and has the final say.")
            .font(.caption).foregroundStyle(.secondary)
    }

    private func systemOneKeyNote(_ draft: NativeSettingsDraft) -> String? {
        guard let metadata = store.metadata, metadata.revision == draft.baseline.revision, let saved = metadata.systemOne else {
            return "After this change is confirmed, enter a key under Native credential entry below."
        }
        guard draft.systemOneTargetDiffers(from: saved) else { return nil }
        return saved.reference == nil
            ? "After this change is confirmed, enter a key under Native credential entry below."
            : "Saving a new provider or endpoint unlinks the current key. After this change is confirmed, enter a new key under Native credential entry below."
    }

    private func systemOneConfigurationUnknown(_ draft: NativeSettingsDraft) -> Bool {
        guard !draft.systemOneEnabled, draft.baseline.entry(.toolSelection)?.desired == .toolSelection(selection: .local) else { return false }
        guard let metadata = store.metadata, metadata.revision == draft.baseline.revision else { return true }
        return (metadata.systemOne?.endpoint ?? SystemOneChoice.typesafeEndpoint) != draft.systemOneEndpoint
            || (metadata.systemOne?.model ?? SystemOneChoice.typesafeModel) != draft.systemOneModel
    }

    private func binding<Value>(_ keyPath: WritableKeyPath<NativeSettingsDraft, Value>, fallback: Value) -> Binding<Value> {
        Binding(get: { store.draft?[keyPath: keyPath] ?? fallback }, set: { value in store.edit { $0[keyPath: keyPath] = value } })
    }

    private func endpoint(_ endpoint: SettingsModelEndpoint) -> String {
        switch endpoint {
        case .official: "Official endpoint"
        case .loopback(let url): url
        }
    }
}

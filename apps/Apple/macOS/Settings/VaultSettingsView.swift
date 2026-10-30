import AppKit
import SwiftUI

struct VaultProviderRow: Identifiable, Sendable {
    let id: String
    let state: String
    let detail: String?

    init(_ value: JSONValue) throws {
        id = try value.requiringString("provider", maximum: 32)
        state = try value["status"].requiringString("state", maximum: 32)
        guard ["local", "1password", "protonpass"].contains(id),
              ["disconnected", "locked", "ready", "unavailable"].contains(state) else { throw OwnerClientError.invalidResponse }
        detail = value["status"]["account"].string ?? value["status"]["reason"].string
    }

    var title: String {
        switch id {
        case "1password": "1Password"
        case "protonpass": "Proton Pass"
        default: "This Mac (Keychain)"
        }
    }
}

struct VaultItemRow: Identifiable, Sendable {
    let id: String
    let kind: String
    let label: String
    let sites: [String]
    let username: String?
    let fields: [String]
    let envName: String?
    let service: String?
    let updatedAt: UInt64?

    init(_ value: JSONValue) throws {
        id = try value.requiringString("ref", maximum: 512)
        kind = try value.requiringString("kind", maximum: 32)
        label = try value.requiringString("label", maximum: 512)
        guard id.hasPrefix("vault://local/"), let sites = value["sites"].array?.compactMap(\.string),
              let fields = value["fields"].array?.compactMap(\.string) else { throw OwnerClientError.invalidResponse }
        self.sites = sites
        self.fields = fields
        username = value["username"].string
        envName = value["metadata"]["env_name"].string
        service = value["metadata"]["service"].string
        updatedAt = value["updated_at"].unsigned
    }

    var localID: String { String(id.dropFirst("vault://local/".count)) }
}

struct VaultIncompleteRow: Identifiable, Sendable {
    let id: String
    let kind: String
    let label: String
    let state: String

    init(_ value: JSONValue) throws {
        id = try value.requiringString("ref", maximum: 512)
        kind = try value.requiringString("kind", maximum: 32)
        label = try value.requiringString("label", maximum: 512)
        state = try value.requiringString("state", maximum: 32)
        guard id.hasPrefix("vault://local/") else { throw OwnerClientError.invalidResponse }
    }

    var localID: String { String(id.dropFirst("vault://local/".count)) }
}

struct VaultDraft: Sendable {
    enum Kind: String, CaseIterable, Sendable { case apiSecret = "api_secret", login }
    var kind = Kind.apiSecret
    var label = ""
    var envName = ""
    var service = ""
    var sites = ""
    var username = ""

    var metadata: JSONValue {
        var object: [String: JSONValue] = ["kind": .string(kind.rawValue), "label": .string(label.trimmingCharacters(in: .whitespaces))]
        switch kind {
        case .apiSecret:
            let env = envName.trimmingCharacters(in: .whitespaces)
            let service = service.trimmingCharacters(in: .whitespaces)
            if !env.isEmpty { object["env_name"] = .string(env) }
            if !service.isEmpty { object["service"] = .string(service) }
        case .login:
            object["sites"] = .array(sites.split(separator: ",").map { .string($0.trimmingCharacters(in: .whitespaces).lowercased()) }.filter { $0 != .string("") })
            let username = username.trimmingCharacters(in: .whitespaces)
            if !username.isEmpty { object["username"] = .string(username) }
        }
        return .object(object)
    }

    var field: String { kind == .apiSecret ? "value" : "password" }
}

private struct VaultPutFrame: Encodable {
    struct Item: Encodable {
        let metadata: JSONValue
        let secrets: [String: String]
    }
    let kind = "vault_put"
    let requestID: String
    let item: Item

    enum CodingKeys: String, CodingKey {
        case kind, item
        case requestID = "request_id"
    }
}

private struct VaultConnectFrame: Encodable {
    let kind = "vault_connect"
    let requestID: String
    let binary: String
    let token: String

    enum CodingKeys: String, CodingKey {
        case kind, binary, token
        case requestID = "request_id"
    }
}

@MainActor
@Observable
final class VaultSettingsModel {
    var providers: [VaultProviderRow] = []
    var items: [VaultItemRow] = []
    var incomplete: [VaultIncompleteRow] = []
    var onePasswordAccounts: [JSONValue] = []
    var loaded = false
    var loading = false
    var busy = false
    var message: String?
    @ObservationIgnored private let client: OwnerIPCClient
    @ObservationIgnored private var connection: OwnerConnection

    init(client: OwnerIPCClient, connection: OwnerConnection) {
        self.client = client
        self.connection = connection
    }

    func replaceConnection(_ selected: OwnerConnection) {
        connection = selected
        providers = []
        items = []
        incomplete = []
        onePasswordAccounts = []
        loaded = false
        message = nil
    }

    func provider(_ id: String) -> VaultProviderRow? { providers.first { $0.id == id } }

    func load() async {
        guard !loading, !connection.executable.isEmpty else { return }
        loading = true
        defer { loading = false }
        let original = connection
        do {
            let result = try await client.request(.query("vault_list", fields: ["limit": .integer(100)]), connection: original)
            let listing = result["listing"]
            guard result["kind"].string == "vault_list", let providers = listing["providers"].array,
                  let items = listing["items"].array, let incomplete = listing["incomplete"].array,
                  items.count <= 100, incomplete.count <= 100 else { throw OwnerClientError.invalidResponse }
            let parsed = (try providers.map(VaultProviderRow.init), try items.map(VaultItemRow.init), try incomplete.map(VaultIncompleteRow.init))
            guard original == connection, !Task.isCancelled else { return }
            (self.providers, self.items, self.incomplete) = parsed
            loaded = true
        } catch { if original == connection, !Task.isCancelled { message = error.localizedDescription } }
    }

    func add(_ draft: VaultDraft, secret: String) async -> Bool {
        guard !busy else { return false }
        guard !secret.isEmpty, secret.utf8.count <= 8192,
              !secret.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else {
            message = "Enter a printable value of at most 8192 bytes."
            return false
        }
        busy = true
        defer { busy = false }
        let original = connection
        let frame = VaultPutFrame(requestID: OwnerID().string, item: .init(metadata: draft.metadata, secrets: [draft.field: secret]))
        do {
            var bytes = try JSONEncoder().encode(frame)
            defer { bytes.resetBytes(in: 0..<bytes.count) }
            let response = try JSONValue.decode(await client.requestSensitive(bytes, connection: original))
            guard response["kind"].string == "vault_item_added",
                  response["ref"].string?.hasPrefix("vault://local/") == true else { throw OwnerClientError.invalidResponse }
            guard original == connection else { return true }
            message = "Added \(draft.label.trimmingCharacters(in: .whitespaces))."
        } catch {
            if original == connection { message = "The item was not confirmed as added. Refresh before adding it again. " + error.localizedDescription }
            await load()
            return false
        }
        await load()
        return true
    }

    func remove(localID: String, label: String) async {
        await mutate(.operation("vault_remove", fields: ["id": .string(localID)]), expected: "applied", done: "Removed \(label).")
    }

    func connectOnePassword(binary: String, account: String?) async {
        var fields: [String: JSONValue] = ["provider": .string("1password"), "binary": .string(binary)]
        if let account { fields["account"] = .string(account) }
        guard let result = await mutate(.operation("vault_connect", fields: fields), expected: "vault_connection", done: nil) else { return }
        onePasswordAccounts = result["probe"]["accounts"].array ?? []
        message = result["connected"].bool == true
            ? "1Password connected."
            : "1Password \(result["probe"]["version"].string ?? "") found. Choose an account to connect."
    }

    func connectProtonPass(binary: String, token: String?) async {
        guard let token else {
            guard let result = await mutate(.operation("vault_connect", fields: ["provider": .string("protonpass"), "binary": .string(binary)]),
                                            expected: "vault_proton_pass", done: nil) else { return }
            message = result["connected"].bool == true
                ? "Proton Pass connected."
                : "Proton Pass has no session yet. Log in with the command shown below, then check again."
            return
        }
        guard !busy, token.utf8.count <= 8192, !token.isEmpty else { return }
        busy = true
        defer { busy = false }
        let original = connection
        do {
            var bytes = try JSONEncoder().encode(VaultConnectFrame(requestID: OwnerID().string, binary: binary, token: token))
            defer { bytes.resetBytes(in: 0..<bytes.count) }
            let response = try JSONValue.decode(await client.requestSensitive(bytes, connection: original))
            guard response["kind"].string == "vault_proton_pass" else { throw OwnerClientError.invalidResponse }
            if original == connection { message = response["connected"].bool == true ? "Proton Pass connected." : "Proton Pass did not sign in." }
        } catch { if original == connection { message = error.localizedDescription } }
        await load()
    }

    func disconnect(_ provider: String) async {
        await mutate(.operation("vault_disconnect", fields: ["provider": .string(provider)]), expected: "applied", done: "Disconnected.")
        if provider == "1password" { onePasswordAccounts = [] }
    }

    @discardableResult
    private func mutate(_ request: OwnerRequest, expected: String, done: String?) async -> JSONValue? {
        guard !busy else { return nil }
        busy = true
        defer { busy = false }
        let original = connection
        var outcome: JSONValue?
        do {
            let result = try await client.request(request, connection: original)
            guard result["kind"].string == expected else { throw OwnerClientError.invalidResponse }
            if original == connection, let done { message = done }
            outcome = result
        } catch { if original == connection { message = error.localizedDescription } }
        await load()
        return original == connection ? outcome : nil
    }
}

@MainActor
struct VaultSettingsView: View {
    let vault: VaultSettingsModel
    let enabled: Bool
    @State private var adding = false
    @State private var removing: (id: String, label: String)?
    @State private var opBinary = "/opt/homebrew/bin/op"
    @State private var opAccount = ""
    @State private var protonBinary = "/opt/homebrew/bin/pass-cli"
    @State private var protonToken = ""

    var body: some View {
        Form {
            Section("Vault items on this Mac") {
                if vault.items.isEmpty && vault.loaded { Text("No items yet.").foregroundStyle(.secondary) }
                ForEach(vault.items) { item in
                    HStack(alignment: .top) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(item.label).font(.headline)
                            Text(summary(item)).font(.caption).foregroundStyle(.secondary)
                            Text(item.id).font(.caption.monospaced()).textSelection(.enabled)
                        }
                        Spacer()
                        Button("Remove…", role: .destructive) { removing = (item.localID, item.label) }
                            .disabled(!enabled || vault.busy)
                            .accessibilityLabel("Remove \(item.label)")
                    }
                }
                ForEach(vault.incomplete) { item in
                    HStack {
                        Text("\(item.label) (\(JSONSummary.label(item.state)))")
                        Spacer()
                        Button("Remove…", role: .destructive) { removing = (item.localID, item.label) }
                            .disabled(!enabled || vault.busy)
                    }
                }
                HStack {
                    Button("Add item…") { adding = true }.disabled(!enabled || vault.busy)
                    Button("Refresh") { Task { await vault.load() } }.disabled(vault.loading)
                }
                if let local = vault.provider("local") {
                    LabeledContent("Keychain", value: JSONSummary.label(local.state) + (local.detail.map { " · " + $0 } ?? ""))
                }
                Text("Only labels and other metadata are shown. Values stay in the Keychain and reach a tool only after you approve its vault use.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("1Password") {
                providerStatus("1password")
                TextField("op executable", text: $opBinary)
                TextField("Account UUID (optional)", text: $opAccount)
                ForEach(Array(vault.onePasswordAccounts.enumerated()), id: \.offset) { _, account in
                    if let uuid = account["account_uuid"].string {
                        Button("Use \(account["email"].string ?? uuid) · \(account["url"].string ?? "")") { opAccount = uuid }
                    }
                }
                HStack {
                    Button(opAccount.isEmpty ? "Find accounts" : "Connect") {
                        let account = opAccount.trimmingCharacters(in: .whitespaces)
                        Task { await vault.connectOnePassword(binary: opBinary, account: account.isEmpty ? nil : account) }
                    }
                    .disabled(!enabled || vault.busy || !opBinary.hasPrefix("/"))
                    disconnect("1password")
                }
                Text("Uses the official op CLI with 1Password app integration. The path is pinned as entered; 1Password asks for approval when Everything signs in.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Proton Pass") {
                providerStatus("protonpass")
                TextField("pass-cli executable", text: $protonBinary)
                SecureField("Personal access token", text: $protonToken)
                HStack {
                    Button("Connect with token") {
                        let token = protonToken
                        protonToken = ""
                        Task { await vault.connectProtonPass(binary: protonBinary, token: token) }
                    }
                    .disabled(!enabled || vault.busy || protonToken.isEmpty || !protonBinary.hasPrefix("/"))
                    Button("Check web login") { Task { await vault.connectProtonPass(binary: protonBinary, token: nil) } }
                        .disabled(!enabled || vault.busy || !protonBinary.hasPrefix("/"))
                    disconnect("protonpass")
                }
                Text("Web login needs a terminal: run")
                    .font(.caption).foregroundStyle(.secondary)
                Text("everything vault connect protonpass --web").font(.caption.monospaced()).textSelection(.enabled)
                Text("The token is stored as a local vault item and sent to the Host only over the private owner pipe.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let message = vault.message { Text(message).font(.caption).textSelection(.enabled) }
        }
        .formStyle(.grouped)
        .padding(12)
        .task { await vault.load() }
        .sheet(isPresented: $adding) { VaultAddSheet(vault: vault) }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in protonToken = "" }
        .onDisappear { protonToken = "" }
        .confirmationDialog("Remove this vault item?", isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }), titleVisibility: .visible) {
            Button("Remove", role: .destructive) {
                guard let target = removing else { return }
                Task { await vault.remove(localID: target.id, label: target.label) }
            }
        } message: {
            Text("\(removing?.label ?? "") is deleted from the Keychain. Tasks that use it will fail.")
        }
    }

    private func summary(_ item: VaultItemRow) -> String {
        var parts = [JSONSummary.label(item.kind)]
        if let env = item.envName { parts.append("$" + env) }
        if let service = item.service { parts.append(service) }
        if let username = item.username { parts.append(username) }
        if !item.sites.isEmpty { parts.append(item.sites.joined(separator: ", ")) }
        parts.append("fields: " + item.fields.joined(separator: ", "))
        if let updated = item.updatedAt { parts.append("updated " + JSONSummary.date(updated)) }
        return parts.joined(separator: " · ")
    }

    @ViewBuilder
    private func providerStatus(_ id: String) -> some View {
        if let provider = vault.provider(id) {
            LabeledContent("Status", value: JSONSummary.label(provider.state))
            if let detail = provider.detail { Text(detail).font(.caption).textSelection(.enabled) }
        } else {
            LabeledContent("Status", value: vault.loaded ? "Not reported" : "Unknown")
        }
    }

    private func disconnect(_ id: String) -> some View {
        Button("Disconnect", role: .destructive) { Task { await vault.disconnect(id) } }
            .disabled(!enabled || vault.busy || [nil, "disconnected"].contains(vault.provider(id)?.state))
    }
}

@MainActor
private struct VaultAddSheet: View {
    let vault: VaultSettingsModel
    @Environment(\.dismiss) private var dismiss
    @State private var draft = VaultDraft()
    @State private var secret = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add vault item").font(.title2)
            Form {
                Picker("Kind", selection: $draft.kind) {
                    Text("API secret").tag(VaultDraft.Kind.apiSecret)
                    Text("Login").tag(VaultDraft.Kind.login)
                }
                TextField("Label", text: $draft.label)
                switch draft.kind {
                case .apiSecret:
                    TextField("Environment name (optional)", text: $draft.envName)
                    TextField("Service (optional)", text: $draft.service)
                    SecureField("Value", text: $secret)
                case .login:
                    TextField("Sites, comma separated", text: $draft.sites)
                    TextField("Username (optional)", text: $draft.username)
                    SecureField("Password", text: $secret)
                }
            }
            .formStyle(.grouped)
            Text("Label, sites, username, environment name and service are metadata the agent can see. The value is masked and sent only over the private owner pipe.")
                .font(.caption).foregroundStyle(.secondary)
            if let message = vault.message { Text(message).font(.caption) }
            HStack {
                Spacer()
                Button("Cancel") { secret = ""; dismiss() }.keyboardShortcut(.cancelAction)
                Button("Add") {
                    let value = secret
                    secret = ""
                    let original = draft
                    Task { if await vault.add(original, secret: value) { dismiss() } }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(vault.busy || secret.isEmpty || draft.label.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 480, height: 460)
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in secret = "" }
        .onDisappear { secret = "" }
    }
}

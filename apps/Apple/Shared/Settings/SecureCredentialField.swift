import Observation
import SwiftUI

@MainActor
@Observable
final class SecureCredentialDraft {
    var value = ""

    func clear() { value = "" }
}

@MainActor
struct SecureCredentialField: View {
    let title: String
    @Bindable var draft: SecureCredentialDraft

    var body: some View {
        SecureField(title, text: $draft.value)
            .autocorrectionDisabled()
            .privacySensitive()
            .onDisappear { draft.clear() }
            #if os(iOS)
            .textInputAutocapitalization(.never)
            .keyboardType(.asciiCapable)
            #endif
    }
}

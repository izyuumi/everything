import SwiftUI

struct PairingView: View {
    let model: AppModel
    var link: String?
    @Environment(\.dismiss) private var dismiss
    @State private var scanning = false

    var body: some View {
        VStack(spacing: 24) {
            switch model.pairingPhase {
            case .idle:
                start
            case .connecting:
                Spacer()
                ProgressView("Contacting your Mac…")
                Spacer()
            case .waiting(let attempt):
                Spacer()
                Image(systemName: "desktopcomputer").font(.system(size: 56)).foregroundStyle(.tint)
                Text("Approve on your Mac").font(.title2.bold())
                Text(attempt.code).font(.system(size: 44, weight: .semibold, design: .monospaced)).accessibilityLabel("Code \(attempt.code)")
                Text("\(attempt.hostName) shows this code with the request. Approve it there.").multilineTextAlignment(.center).foregroundStyle(.secondary)
                ProgressView()
                Spacer()
            case .paired(let host):
                Spacer()
                Image(systemName: "checkmark.circle.fill").font(.system(size: 64)).foregroundStyle(.green)
                Text("Paired with \(host)").font(.title2.bold()).multilineTextAlignment(.center)
                Spacer()
                Button("Done") { dismiss() }.buttonStyle(.borderedProminent).controlSize(.large)
            case .failed(let message):
                Spacer()
                Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 56)).foregroundStyle(.orange)
                Text("Couldn’t pair").font(.title2.bold())
                Text(message).multilineTextAlignment(.center).foregroundStyle(.secondary)
                Spacer()
                Button("Try again") { Task { await model.cancelPairing() } }.buttonStyle(.borderedProminent).controlSize(.large)
            }
        }
        .padding()
        .frame(maxWidth: .infinity)
        .privacySensitive()
        .navigationTitle("Pair with Mac")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                if model.pairingPhase != .idle, !isFinished { Button("Cancel") { Task { await model.cancelPairing() } } }
                else { Button("Close") { dismiss() } }
            }
        }
        .sheet(isPresented: $scanning) {
            QRScanner { value in
                scanning = false
                model.startPairing(value)
            }
            .ignoresSafeArea()
            .modifier(InactivePrivacyCover(enabled: model.privacyCoverEnabled))
        }
        .task(id: link) {
            if let link { model.startPairing(link) }
        }
        .onDisappear { Task { await model.cancelPairing() } }
    }

    private var isFinished: Bool {
        if case .paired = model.pairingPhase { return true }
        if case .failed = model.pairingPhase { return true }
        return false
    }

    private var start: some View {
        VStack(spacing: 20) {
            Spacer()
            Image(systemName: "qrcode.viewfinder").font(.system(size: 64)).foregroundStyle(.tint)
            Text("Pair with your Mac").font(.title2.bold())
            Text("On your Mac, run vorra pair, then scan the code it shows. You can also scan it with the Camera app.").multilineTextAlignment(.center).foregroundStyle(.secondary)
            Spacer()
            Button { scanning = true } label: { Label("Scan QR code", systemImage: "camera").frame(maxWidth: .infinity) }
                .buttonStyle(.borderedProminent).controlSize(.large)
            PasteButton(payloadType: String.self) { values in
                guard let value = values.first else { return }
                Task { @MainActor in model.startPairing(value) }
            }
            .labelStyle(.titleAndIcon)
        }
    }
}

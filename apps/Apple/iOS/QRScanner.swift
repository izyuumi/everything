import AVFoundation
import SwiftUI
import UIKit

struct QRScanner: View {
    let onCode: @MainActor (String) -> Void
    @State private var authorized: Bool?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        ZStack(alignment: .topTrailing) {
            if authorized == true {
                CameraPreview(onCode: onCode)
            } else if authorized == false {
                ContentUnavailableView("Camera unavailable", systemImage: "camera", description: Text("Allow camera access in iOS Settings, or paste the invitation manually."))
            } else { ProgressView("Requesting camera access") }
            Button("Close", systemImage: "xmark.circle.fill") { dismiss() }.font(.title).padding().accessibilityLabel("Close QR scanner")
        }
        .task { authorized = await AVCaptureDevice.requestAccess(for: .video) }
        .privacySensitive()
    }
}

private final class CameraCapture: @unchecked Sendable {
    let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "ai.everything.ios.camera")

    func start(delegate: any AVCaptureMetadataOutputObjectsDelegate & Sendable, completion: @escaping @Sendable (Bool) -> Void) {
        queue.async {
            guard let device = AVCaptureDevice.default(for: .video) else { completion(false); return }
            do {
                let input = try AVCaptureDeviceInput(device: device)
                let output = AVCaptureMetadataOutput()
                self.session.beginConfiguration()
                guard self.session.canAddInput(input), self.session.canAddOutput(output) else { self.session.commitConfiguration(); completion(false); return }
                self.session.addInput(input)
                self.session.addOutput(output)
                output.setMetadataObjectsDelegate(delegate, queue: .main)
                output.metadataObjectTypes = [.qr]
                self.session.commitConfiguration()
                self.session.startRunning()
                completion(true)
            } catch { completion(false) }
        }
    }

    func stop() {
        queue.async { if self.session.isRunning { self.session.stopRunning() } }
    }
}

private struct CameraPreview: UIViewControllerRepresentable {
    let onCode: @MainActor (String) -> Void

    func makeUIViewController(context: Context) -> CameraController {
        CameraController(onCode: onCode)
    }

    func updateUIViewController(_ uiViewController: CameraController, context: Context) {}

    static func dismantleUIViewController(_ uiViewController: CameraController, coordinator: ()) { uiViewController.stop() }
}

@MainActor
private final class CameraController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    private let capture = CameraCapture()
    private let onCode: @MainActor (String) -> Void
    private var scanned = false
    private var preview: AVCaptureVideoPreviewLayer?

    init(onCode: @escaping @MainActor (String) -> Void) {
        self.onCode = onCode
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { return nil }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        let layer = AVCaptureVideoPreviewLayer(session: capture.session)
        layer.videoGravity = .resizeAspectFill
        view.layer.addSublayer(layer)
        preview = layer
        capture.start(delegate: self) { [weak self] available in
            Task { @MainActor in
                guard let self, !available else { return }
                let label = UILabel()
                label.text = "Camera capture is unavailable. Paste the invitation manually."
                label.textColor = .white
                label.numberOfLines = 0
                label.frame = self.view.bounds.insetBy(dx: 24, dy: 64)
                label.autoresizingMask = [.flexibleWidth, .flexibleHeight]
                self.view.addSubview(label)
            }
        }
    }

    override func viewDidLayoutSubviews() { super.viewDidLayoutSubviews(); preview?.frame = view.bounds }
    override func viewWillDisappear(_ animated: Bool) { super.viewWillDisappear(animated); stop() }
    func stop() { capture.stop() }

    nonisolated func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
        guard let value = metadataObjects.compactMap({ ($0 as? AVMetadataMachineReadableCodeObject)?.stringValue }).first, value.utf8.count <= 4_096 else { return }
        Task { @MainActor in
            guard !scanned else { return }
            scanned = true
            stop()
            onCode(value)
        }
    }
}

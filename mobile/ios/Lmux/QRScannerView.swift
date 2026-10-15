@preconcurrency import AVFoundation
import SwiftUI
import UIKit

/// Camera viewfinder that decodes one QR code and hands its string to `onCode`.
struct QRScannerView: View {
    private let onCode: (String) -> Void

    @State private var access: Access = .checking

    private enum Access { case checking, granted, unavailable }

    init(onCode: @escaping (String) -> Void) {
        self.onCode = onCode
    }

    var body: some View {
        ZStack {
            Theme.pageBackground.ignoresSafeArea()
            switch access {
            case .checking:
                ProgressView().tint(Theme.secondaryText)
            case .granted:
                QRCameraRepresentable(onCode: onCode, onFailure: { access = .unavailable })
                    .ignoresSafeArea()
                    .overlay(viewfinder)
            case .unavailable:
                unavailableMessage
            }
        }
        .task { await resolveAccess() }
    }

    private var viewfinder: some View {
        GeometryReader { geo in
            let side = min(geo.size.width, geo.size.height) * 0.65
            RoundedRectangle(cornerRadius: 24, style: .continuous)
                .stroke(Theme.accent, lineWidth: 3)
                .frame(width: side, height: side)
                .position(x: geo.size.width / 2, y: geo.size.height / 2)
        }
        .allowsHitTesting(false)
    }

    private var unavailableMessage: some View {
        VStack(spacing: 16) {
            Text("Camera access is off. Allow it in Settings, or paste the link.")
                .font(.body)
                .foregroundStyle(Theme.primaryText)
                .multilineTextAlignment(.center)
            Button("Open Settings") {
                if let url = URL(string: UIApplication.openSettingsURLString) {
                    UIApplication.shared.open(url)
                }
            }
            .foregroundStyle(Theme.accent)
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func resolveAccess() async {
        guard AVCaptureDevice.default(for: .video) != nil else {
            access = .unavailable
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            access = .granted
        case .notDetermined:
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            access = granted ? .granted : .unavailable
        default:
            access = .unavailable
        }
    }
}

private struct QRCameraRepresentable: UIViewControllerRepresentable {
    let onCode: (String) -> Void
    let onFailure: () -> Void

    func makeUIViewController(context: Context) -> QRCameraController {
        let controller = QRCameraController()
        controller.onCode = onCode
        controller.onFailure = onFailure
        return controller
    }

    func updateUIViewController(_ controller: QRCameraController, context: Context) {
        controller.onCode = onCode
        controller.onFailure = onFailure
    }

    static func dismantleUIViewController(_ controller: QRCameraController, coordinator: ()) {
        controller.stop()
    }
}

private final class QRCameraController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var onCode: ((String) -> Void)?
    var onFailure: (() -> Void)?

    private let session = AVCaptureSession()
    private let sessionQueue = DispatchQueue(label: "com.vas.lmux.qrscanner")
    private var previewLayer: AVCaptureVideoPreviewLayer?
    private var configured = false
    private var delivered = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        guard configure() else {
            DispatchQueue.main.async { [weak self] in self?.onFailure?() }
            return
        }
        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        view.layer.addSublayer(layer)
        previewLayer = layer
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        previewLayer?.frame = view.bounds
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        start()
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        stop()
    }

    private func configure() -> Bool {
        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device) else { return false }
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        guard session.canAddInput(input) else { return false }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { return false }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        guard output.availableMetadataObjectTypes.contains(.qr) else { return false }
        output.metadataObjectTypes = [.qr]
        configured = true
        return true
    }

    func start() {
        guard configured, !delivered else { return }
        let session = self.session
        sessionQueue.async {
            if !session.isRunning { session.startRunning() }
        }
    }

    func stop() {
        let session = self.session
        sessionQueue.async {
            if session.isRunning { session.stopRunning() }
        }
    }

    func metadataOutput(_ output: AVCaptureMetadataOutput,
                        didOutput metadataObjects: [AVMetadataObject],
                        from connection: AVCaptureConnection) {
        guard !delivered,
              let code = metadataObjects
                .compactMap({ ($0 as? AVMetadataMachineReadableCodeObject)?.stringValue })
                .first(where: { !$0.isEmpty }) else { return }
        delivered = true
        stop()
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        onCode?(code)
    }
}

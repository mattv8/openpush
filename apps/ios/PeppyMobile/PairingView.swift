import SwiftUI
import AVFoundation

/// Camera presentation is deliberately limited to reading the QR intent. The phone does not
/// generate, serialize, or accept a private key from a QR code; claiming needs the Rust-owned
/// enrollment helper and the approved server challenge contract.
struct PairingView: View {
    let model: AppModel
    @Binding var isPresented: Bool
    @State private var scannedValue: String?
    @State private var claim: EnrollmentClaim?
    @State private var busy = false
    @State private var error: String?
    @State private var pollingTask: Task<Void, Never>?
    @State private var cameraDenied = AVCaptureDevice.authorizationStatus(for: .video) == .denied || AVCaptureDevice.authorizationStatus(for: .video) == .restricted

    var body: some View {
        NavigationStack {
            VStack(spacing: 16) {
                if cameraDenied {
                    ContentUnavailableView("Camera access is required", systemImage: "camera.fill", description: Text("Allow Camera access in Settings, then scan the pairing QR code."))
                        .frame(height: 300).accessibilityIdentifier("qr-camera-denied")
                } else {
                    QRScannerView { value in scannedValue = value }
                        .frame(height: 300).clipShape(RoundedRectangle(cornerRadius: 16))
                        .accessibilityLabel("QR code scanner viewfinder")
                        .accessibilityIdentifier("qr-viewfinder")
                }
                Text("peppy.scan_qr", tableName: "Peppy").font(.headline)
                if let scannedValue {
                    if let claim {
                        LabeledContent("Server", value: claim.origin).accessibilityIdentifier("qr-server-origin")
                        Text(claim.sas).font(.system(.largeTitle, design: .monospaced))
                            .accessibilityLabel("Verification code: \(claim.sas)").accessibilityIdentifier("qr-sas-code")
                        Text("Check this matches your desktop before the owner approves pairing.").foregroundStyle(.secondary)
                        Text("Waiting for owner approval…").foregroundStyle(.secondary)
                    } else {
                        Button("Verify pairing intent") { Task { await start(scannedValue) } }
                            .disabled(busy).accessibilityIdentifier("qr-approve-button")
                    }
                }
                if busy { ProgressView("Verifying with server…").accessibilityIdentifier("qr-progress") }
                if let error { Text(error).foregroundStyle(.red).accessibilityIdentifier("qr-error") }
            }.padding()
            .navigationTitle(Text("peppy.pair_device", tableName: "Peppy"))
            .toolbar { ToolbarItem(placement: .topBarLeading) { Button(role: .cancel) { cancel() } label: { Text("peppy.cancel", tableName: "Peppy") }.accessibilityIdentifier("qr-cancel-button") } }
        }.accessibilityIdentifier("qr-pairing-screen")
        .onDisappear { cancelPolling() }
        .task {
            guard AVCaptureDevice.authorizationStatus(for: .video) == .notDetermined else { return }
            cameraDenied = !(await AVCaptureDevice.requestAccess(for: .video))
        }
    }

    private func start(_ value: String) async {
        busy = true; defer { busy = false }
        do {
            let next = try await model.session.claimPairingIntent(qrData: Data(value.utf8))
            claim = next; error = nil
            pollingTask?.cancel()
            pollingTask = Task { await pollForApproval(next) }
        }
        catch { self.error = (error as? ClientError)?.userMessage ?? "Pairing could not be started." }
    }

    private func pollForApproval(_ claim: EnrollmentClaim) async {
        // Polling is deliberately bounded; closing this view cancels the task and clears secrets.
        for _ in 0..<12 where !Task.isCancelled {
            do {
                _ = try await model.session.completePairing(claim)
                await model.load(); isPresented = false; return
            } catch ClientError.pairingAwaitingApproval {
                try? await Task.sleep(for: .seconds(5))
            } catch {
                self.error = (error as? ClientError)?.userMessage ?? "Pairing could not be completed."
                return
            }
        }
        if !Task.isCancelled {
            error = "Pairing approval did not arrive in time. Scan a new code to try again."
            await model.session.cancelPairing(claim)
        }
        pollingTask = nil
    }

    private func cancel() {
        cancelPolling()
        isPresented = false
    }

    private func cancelPolling() {
        pollingTask?.cancel()
        pollingTask = nil
        if let claim { Task { await model.session.cancelPairing(claim) } }
    }
}

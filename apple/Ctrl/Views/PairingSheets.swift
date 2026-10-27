import CtrlKit
import SwiftUI

/// Shows this Mac's pairing code for another device to enter.
struct ShowCodeSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var code: PairingCode?
    @State private var pairedCount = 0

    var body: some View {
        VStack(spacing: 16) {
            Text("Add a Device")
                .font(.headline)
            Text("On the other device, choose this Mac and enter this code.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Text(formatted)
                .font(.system(.largeTitle, design: .monospaced, weight: .semibold))
                .textSelection(.enabled)
                .accessibilityLabel("Pairing code")
                .accessibilityValue(code?.code.map(String.init).joined(separator: " ") ?? "")
            if let failure = model.pairingFailure {
                Label(failure, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
            } else if let code {
                Text("Expires \(Date(timeIntervalSince1970: TimeInterval(code.expiresAtMs) / 1000), style: .relative).")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Button("Done") { dismiss() }
                .keyboardShortcut(.defaultAction)
                .controlSize(.large)
        }
        .padding(24)
        .frame(width: 340)
        .onAppear {
            pairedCount = model.pairedDevices.count
            code = model.beginPairing()
        }
        .onChange(of: model.pairedDevices.count) { _, count in
            if count > pairedCount { dismiss() }
        }
        .onDisappear { model.cancelPairing() }
    }

    private var formatted: String {
        guard let code = code?.code, code.count == 6 else { return "––– –––" }
        return "\(code.prefix(3)) \(code.suffix(3))"
    }
}

/// Enters the code another device is showing.
struct EnterCodeSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let device: DeviceInfo
    @State private var code = ""
    @State private var error: String?
    @State private var pairing = false

    var body: some View {
        VStack(spacing: 16) {
            Text("Pair with Device")
                .font(.headline)
            Text("Enter the code shown on the device with fingerprint \(device.fingerprint).")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            TextField("Code", text: $code, prompt: Text("123 456"))
                .font(.system(.largeTitle, design: .monospaced, weight: .semibold))
                .multilineTextAlignment(.center)
                .textFieldStyle(.plain)
                .onChange(of: code) { _, new in code = String(new.filter(\.isNumber).prefix(6)) }
                .onSubmit(pair)
                .accessibilityLabel("Pairing code")
            if let error {
                Label(error, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
            }
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(action: pair) {
                    if pairing {
                        ProgressView().controlSize(.small)
                    } else {
                        Text("Pair")
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(code.count != 6 || pairing)
            }
            .controlSize(.large)
        }
        .padding(24)
        .frame(width: 340)
    }

    private func pair() {
        guard code.count == 6, !pairing else { return }
        pairing = true
        error = nil
        Task {
            do {
                try await model.pair(with: device.deviceId, code: code)
                dismiss()
            } catch {
                self.error = error.localizedDescription
                pairing = false
            }
        }
    }
}

import CtrlKit
import SwiftUI

/// Paired devices with a connection dot, and the app menu.
struct DevicesFooter: View {
    @Environment(AppModel.self) private var model
    let openSettings: () -> Void

    var body: some View {
        @Bindable var model = model
        HStack(spacing: 12) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 12) {
                    if model.pairedDevices.isEmpty {
                        Text("No paired devices")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(model.pairedDevices, id: \.deviceId) { device in
                        DeviceChip(device: device)
                    }
                }
            }
            Menu {
                Button("Add Device…") {
                    model.settingsPane = .devices
                    model.wantsAddDevice = true
                    openSettings()
                }
                Toggle("Pause Syncing", isOn: $model.syncPaused)
                Divider()
                Button("Settings…", action: openSettings)
                    .keyboardShortcut(",")
                Button("Quit Ctrl") { NSApp.terminate(nil) }
                    .keyboardShortcut("q")
            } label: {
                Image(systemName: "ellipsis.circle")
                    .accessibilityLabel("More")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

struct DeviceChip: View {
    let device: DeviceInfo

    var body: some View {
        HStack(spacing: 5) {
            // Filled when connected, hollow when not, so the state doesn't rely on colour.
            Group {
                if device.connected {
                    Circle().fill(.green)
                } else {
                    Circle().strokeBorder(.secondary, lineWidth: 1.5)
                }
            }
            .frame(width: 7, height: 7)
            Text(device.name ?? "Unnamed device")
                .lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(device.name ?? "Unnamed device")
        .accessibilityValue(device.connected ? "Connected" : "Not connected")
    }
}

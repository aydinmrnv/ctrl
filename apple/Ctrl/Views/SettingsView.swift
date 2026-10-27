import CtrlKit
import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        TabView(selection: $model.settingsPane) {
            GeneralPane()
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(AppModel.SettingsPane.general)
            DevicesPane()
                .tabItem { Label("Devices", systemImage: "laptopcomputer.and.iphone") }
                .tag(AppModel.SettingsPane.devices)
            HistoryPane()
                .tabItem { Label("History", systemImage: "clock") }
                .tag(AppModel.SettingsPane.history)
            PrivacyPane()
                .tabItem { Label("Privacy", systemImage: "hand.raised") }
                .tag(AppModel.SettingsPane.privacy)
        }
        .frame(width: 500)
        .frame(minHeight: 360)
    }
}

struct GeneralPane: View {
    @Environment(AppModel.self) private var model
    @State private var name = ""
    @State private var opensAtLogin = false

    var body: some View {
        @Bindable var model = model
        Form {
            Section {
                TextField("Device name", text: $name)
                    .onSubmit { model.setDeviceName(name) }
                Toggle("Open at login", isOn: $opensAtLogin)
                    .onChange(of: opensAtLogin) { _, on in
                        model.opensAtLogin = on
                        opensAtLogin = model.opensAtLogin
                    }
            }
            Section {
                Toggle("Pause syncing", isOn: $model.syncPaused)
                Toggle("Send images", isOn: $model.sendImages)
                Toggle("Send files", isOn: $model.sendFiles)
            } header: {
                Text("Sync")
            } footer: {
                Text("Large images and files are announced right away and only transferred when you paste them.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            name = model.deviceName
            opensAtLogin = model.opensAtLogin
        }
        .onDisappear { model.setDeviceName(name) }
    }
}

struct DevicesPane: View {
    @Environment(AppModel.self) private var model
    @State private var showingCode = false
    @State private var enteringCodeFor: DeviceInfo?
    @State private var renaming: DeviceInfo?
    @State private var newName = ""

    var body: some View {
        @Bindable var model = model
        Form {
            Section("This Mac") {
                LabeledContent("Name", value: model.deviceName)
                LabeledContent("Fingerprint") {
                    Text(model.fingerprint)
                        .font(.body.monospaced())
                        .textSelection(.enabled)
                }
            }
            Section {
                if model.pairedDevices.isEmpty {
                    Text("No paired devices.")
                        .foregroundStyle(.secondary)
                }
                ForEach(model.pairedDevices, id: \.deviceId) { device in
                    LabeledContent {
                        Menu("Options") {
                            Button("Rename…") {
                                newName = device.name ?? ""
                                renaming = device
                            }
                            Button("Unpair", role: .destructive) { model.unpair(device) }
                        }
                        .menuStyle(.borderlessButton)
                        .fixedSize()
                    } label: {
                        DeviceChip(device: device)
                        Text(device.fingerprint)
                            .font(.caption.monospaced())
                    }
                }
            } header: {
                Text("Paired Devices")
            } footer: {
                HStack {
                    Spacer()
                    Button("Add Device…") { showingCode = true }
                }
            }
            if !model.nearbyDevices.isEmpty {
                Section {
                    ForEach(model.nearbyDevices, id: \.deviceId) { device in
                        LabeledContent {
                            Button("Pair…") { enteringCodeFor = device }
                        } label: {
                            Text("Unpaired device")
                            Text(device.fingerprint)
                                .font(.caption.monospaced())
                        }
                    }
                } header: {
                    Text("Nearby")
                } footer: {
                    Text("Check that the fingerprint matches the one shown on the other device before you pair.")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .sheet(isPresented: $showingCode) { ShowCodeSheet() }
        .sheet(item: $enteringCodeFor) { device in EnterCodeSheet(device: device) }
        .alert("Rename Device", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField("Name", text: $newName)
            Button("Rename") {
                if let renaming { model.rename(renaming, to: newName) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The name is only used on this Mac. Leave it empty to use the device’s own name.")
        }
        .onAppear(perform: takeAddDeviceRequest)
        .onChange(of: model.wantsAddDevice) { takeAddDeviceRequest() }
    }

    private func takeAddDeviceRequest() {
        guard model.wantsAddDevice else { return }
        model.wantsAddDevice = false
        showingCode = true
    }
}

extension DeviceInfo: @retroactive Identifiable {
    public var id: String { deviceId }
}

struct HistoryPane: View {
    @Environment(AppModel.self) private var model
    @State private var confirmingClear = false

    private static let itemChoices: [UInt32] = [50, 100, 200, 500]
    private static let dayChoices: [UInt32] = [1, 7, 30, 90]

    var body: some View {
        @Bindable var model = model
        Form {
            Section {
                Picker("Keep up to", selection: $model.retention.maxItems) {
                    ForEach(Self.itemChoices, id: \.self) { Text("\($0) items") }
                }
                Picker("Remove items after", selection: $model.retention.maxAgeDays) {
                    ForEach(Self.dayChoices, id: \.self) { Text($0 == 1 ? "1 day" : "\($0) days") }
                }
            } footer: {
                Text("History stays on this Mac. Each device keeps its own.")
                    .foregroundStyle(.secondary)
            }
            Section {
                Button("Clear History…", role: .destructive) { confirmingClear = true }
            }
        }
        .formStyle(.grouped)
        .confirmationDialog("Clear all history on this Mac?", isPresented: $confirmingClear) {
            Button("Clear History", role: .destructive) { model.clearHistory() }
        } message: {
            Text("This can’t be undone. Other devices keep their own history.")
        }
    }
}

struct PrivacyPane: View {
    @State private var trusted = Paster.isTrusted

    var body: some View {
        Form {
            Section {
                LabeledContent("Passwords and secrets") {
                    Text("Never sent or saved")
                }
            } footer: {
                Text("Items that password managers and other apps mark as concealed, transient or auto-generated, such as passwords copied from 1Password or Keychain Access, never leave this Mac and never appear in history.")
                    .foregroundStyle(.secondary)
            }
            Section {
                LabeledContent("Encryption") {
                    Text("End to end")
                }
            } footer: {
                Text("Devices talk directly over your local network. Clips are encrypted with keys that only your paired devices have.")
                    .foregroundStyle(.secondary)
            }
            Section {
                LabeledContent("Paste with Option-click") {
                    if trusted {
                        Text("Allowed")
                    } else {
                        Button("Open Accessibility Settings…") { Paster.openAccessibilitySettings() }
                    }
                }
            } footer: {
                Text("Option-clicking a clip copies it as plain text. With Accessibility access, Ctrl also pastes it into the app you were using.")
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            trusted = Paster.isTrusted
        }
    }
}

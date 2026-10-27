import CtrlKit
import SwiftUI

/// The menu bar popover: recent clips, then nearby devices.
struct PopoverView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        VStack(spacing: 0) {
            content
                .frame(maxHeight: .infinity)
            Divider()
            DevicesFooter(openSettings: showSettings)
        }
        .frame(width: 360, height: 480)
    }

    @ViewBuilder
    private var content: some View {
        if let error = model.startupError {
            ContentUnavailableView {
                Label("Ctrl Couldn’t Start", systemImage: "exclamationmark.triangle")
            } description: {
                Text(error)
            }
        } else if model.clips.isEmpty && model.pairedDevices.isEmpty {
            WelcomeView(addDevice: addDevice)
        } else if model.clips.isEmpty {
            ContentUnavailableView(
                "No Clips Yet", systemImage: "list.clipboard",
                description: Text("Copy something on any of your devices and it appears here."))
        } else {
            ClipList()
        }
    }

    private func addDevice() {
        model.settingsPane = .devices
        model.wantsAddDevice = true
        showSettings()
    }

    private func showSettings() {
        Paster.closeMenuBarWindow()
        NSApp.activate()
        openSettings()
    }
}

/// Shown until the first device is paired.
struct WelcomeView: View {
    let addDevice: () -> Void

    var body: some View {
        ContentUnavailableView {
            Label("Ctrl", systemImage: "list.clipboard")
        } description: {
            Text("Copy on this Mac and paste on your other devices. Pair a device to begin.")
        } actions: {
            Button("Add Device…", action: addDevice)
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
        }
    }
}

/// Recent clips. Click copies, Option-click pastes as plain text, ⌘1–⌘9 copy
/// the first nine, and arrow keys plus Return do the same from the keyboard.
struct ClipList: View {
    @Environment(AppModel.self) private var model
    @State private var selection: ClipSummary.ID?
    @FocusState private var focused: Bool

    var body: some View {
        List(selection: $selection) {
            ForEach(Array(model.clips.enumerated()), id: \.element.id) { index, clip in
                ClipRow(clip: clip, source: model.deviceLabel(for: clip.originDevice), quickPick: index < 9 ? index + 1 : nil)
                    .tag(clip.id)
                    .contentShape(Rectangle())
                    .onTapGesture { activate(clip, plainText: NSEvent.modifierFlags.contains(.option)) }
                    .contextMenu { menu(for: clip) }
                    .accessibilityAction { activate(clip, plainText: false) }
                    .accessibilityAction(named: "Paste as Plain Text") { activate(clip, plainText: true) }
                    .accessibilityAction(named: "Delete") { model.delete(clip) }
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .focused($focused)
        .onAppear {
            selection = model.clips.first?.id
            focused = true
        }
        .onKeyPress(.return, phases: .down) { press in
            guard let clip = selected else { return .ignored }
            activate(clip, plainText: press.modifiers.contains(.option))
            return .handled
        }
        .onKeyPress(.delete) {
            guard let clip = selected else { return .ignored }
            model.delete(clip)
            return .handled
        }
        .onKeyPress(characters: .decimalDigits, phases: .down) { press in
            guard press.modifiers.contains(.command), let n = Int(press.characters), (1...9).contains(n),
                  n <= model.clips.count else { return .ignored }
            activate(model.clips[n - 1], plainText: press.modifiers.contains(.option))
            return .handled
        }
    }

    private var selected: ClipSummary? {
        model.clips.first { $0.id == selection }
    }

    @ViewBuilder
    private func menu(for clip: ClipSummary) -> some View {
        Button("Copy") { activate(clip, plainText: false) }
        Button("Paste as Plain Text") { activate(clip, plainText: true) }
            .disabled(!clip.hasPlainText)
        Divider()
        Button("Delete", role: .destructive) { model.delete(clip) }
    }

    private func activate(_ clip: ClipSummary, plainText: Bool) {
        selection = clip.id
        model.copy(clip, plainText: plainText)
        if !plainText { Paster.closeMenuBarWindow() }
    }
}

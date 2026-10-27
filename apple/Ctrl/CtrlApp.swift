import AppKit
import SwiftUI

@main
struct CtrlApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environment(delegate.model)
        } label: {
            MenuBarIcon(isTransferring: delegate.model.isTransferring)
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(delegate.model)
        }
    }
}

/// The menu bar icon: a template SF Symbol that changes while a clip moves.
struct MenuBarIcon: View {
    let isTransferring: Bool

    var body: some View {
        Image(systemName: isTransferring ? "arrow.left.arrow.right" : "list.clipboard")
            .accessibilityLabel("Ctrl")
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model: AppModel

    override init() {
        #if DEBUG
        if let demo = DemoData.modelIfRequested() {
            model = demo
            super.init()
            return
        }
        #endif
        model = AppModel()
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        #if DEBUG
        if DemoData.renderSnapshotsIfRequested(model) {
            NSApp.terminate(nil)
            return
        }
        #endif
        // No onboarding: the first launch opens the popover, which explains
        // Ctrl in one sentence and offers Add Device.
        let key = "hasLaunched"
        if !UserDefaults.standard.bool(forKey: key) {
            UserDefaults.standard.set(true, forKey: key)
            Task {
                try? await Task.sleep(for: .milliseconds(300))
                MenuBarExtraOpener.open()
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        model.shutdown()
    }
}

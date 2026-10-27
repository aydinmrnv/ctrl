import AppKit
import ApplicationServices

/// Sends ⌘V to the frontmost app, for Option-click "paste as plain text".
///
/// Posting keyboard events needs Accessibility access. Without it, the clip is
/// still on the pasteboard and the user pastes it themselves.
@MainActor
enum Paster {
    static var isTrusted: Bool { AXIsProcessTrusted() }

    static func pasteIntoFrontmostApp() {
        closeMenuBarWindow()
        guard isTrusted else { return }
        // Give the previously active app a moment to become key again.
        Task {
            try? await Task.sleep(for: .milliseconds(150))
            let source = CGEventSource(stateID: .combinedSessionState)
            let v: CGKeyCode = 9
            for down in [true, false] {
                let event = CGEvent(keyboardEventSource: source, virtualKey: v, keyDown: down)
                event?.flags = .maskCommand
                event?.post(tap: .cghidEventTap)
            }
        }
    }

    static func openAccessibilitySettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    /// Closes the `MenuBarExtra` window, which has no SwiftUI dismiss action.
    static func closeMenuBarWindow() {
        NSApp.keyWindow?.close()
    }
}

/// Opens the `MenuBarExtra` popover from code. SwiftUI has no API for this, so
/// it clicks the status item's button.
@MainActor
enum MenuBarExtraOpener {
    static func open() {
        for window in NSApp.windows where window.className.contains("NSStatusBarWindow") {
            if let button = findButton(in: window.contentView) {
                button.performClick(nil)
                return
            }
        }
    }

    private static func findButton(in view: NSView?) -> NSButton? {
        guard let view else { return nil }
        if let button = view as? NSButton { return button }
        for subview in view.subviews {
            if let button = findButton(in: subview) { return button }
        }
        return nil
    }
}

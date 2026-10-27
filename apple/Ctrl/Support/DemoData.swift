#if DEBUG
import AppKit
import CtrlKit
import SwiftUI

/// Canned data for screenshots and performance checks. Debug builds only.
///
///     Ctrl.app/Contents/MacOS/Ctrl -CtrlDemo YES                     run with fake clips
///     Ctrl.app/Contents/MacOS/Ctrl -CtrlDemo YES -CtrlDemoCount 500  ...and 500 of them
///     Ctrl.app/Contents/MacOS/Ctrl -CtrlDemo YES -CtrlSnapshot DIR   write popover PNGs and quit
@MainActor
enum DemoData {
    static func modelIfRequested() -> AppModel? {
        let defaults = UserDefaults.standard
        guard defaults.bool(forKey: "CtrlDemo") else { return nil }
        let count = max(defaults.integer(forKey: "CtrlDemoCount"), 0)
        return AppModel(demo: clips(count: count), devices: devices, deviceName: "This Mac")
    }

    static let devices = [
        DeviceInfo(deviceId: "studio", name: "Studio Mac", fingerprint: "4F2A 91C0", trusted: true, connected: true, addresses: []),
        DeviceInfo(deviceId: "pc", name: "Windows PC", fingerprint: "B7E1 0D3C", trusted: true, connected: false, addresses: []),
    ]

    static func clips(count: Int) -> [ClipSummary] {
        let now = Date()
        let base: [ClipSummary] = [
            ClipSummary(
                id: "1", originDevice: "this-mac", createdAt: now.addingTimeInterval(-20), kind: .text,
                preview: "Let’s move the design review to Thursday at 3 so everyone can make it.", hasPlainText: true),
            ClipSummary(
                id: "2", originDevice: "studio", createdAt: now.addingTimeInterval(-240), kind: .image, preview: "Image",
                thumbnail: sampleImage()),
            ClipSummary(
                id: "3", originDevice: "pc", createdAt: now.addingTimeInterval(-900), kind: .url,
                preview: "https://developer.apple.com/design/human-interface-guidelines/the-menu-bar", hasPlainText: true),
            ClipSummary(
                id: "4", originDevice: "studio", createdAt: now.addingTimeInterval(-3_600), kind: .file,
                preview: "Quarterly Report.pdf", fileName: "Quarterly Report.pdf", totalSize: 4_200_000, hasPlainText: true),
            ClipSummary(
                id: "5", originDevice: "this-mac", createdAt: now.addingTimeInterval(-7_400), kind: .richText,
                preview: "Ctrl keeps rich text, images and files in their original formats.", hasPlainText: true),
            ClipSummary(
                id: "6", originDevice: "pc", createdAt: now.addingTimeInterval(-86_000), kind: .text,
                preview: "git log --oneline --graph --decorate", hasPlainText: true),
        ]
        guard count > base.count else { return base }
        let extra = (base.count..<count).map { i in
            ClipSummary(
                id: "\(i + 1)", originDevice: i.isMultiple(of: 2) ? "this-mac" : "studio",
                createdAt: now.addingTimeInterval(-Double(i) * 600), kind: .text,
                preview: "Clip number \(i + 1) with a little bit of text to wrap", hasPlainText: true)
        }
        return base + extra
    }

    private static func sampleImage() -> Data? {
        let image = NSImage(size: NSSize(width: 64, height: 64), flipped: false) { rect in
            NSGradient(colors: [.systemTeal, .systemIndigo])?.draw(in: rect, angle: 45)
            return true
        }
        guard let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff) else { return nil }
        return rep.representation(using: .png, properties: [:])
    }

    /// Renders the popover in each appearance to `-CtrlSnapshot DIR` and reports
    /// how long the first layout took. Returns false when not requested.
    static func renderSnapshotsIfRequested(_ model: AppModel) -> Bool {
        guard let dir = UserDefaults.standard.string(forKey: "CtrlSnapshot") else { return false }
        // Increase Contrast is a system setting that an offscreen window
        // doesn't pick up, so take that screenshot from the running app.
        let appearances: [(String, NSAppearance.Name)] = [("light", .aqua), ("dark", .darkAqua)]
        for (name, appearanceName) in appearances {
            let start = Date()
            let view = NSHostingView(
                rootView: PopoverView()
                    .environment(model)
                    .background(Color(nsColor: .windowBackgroundColor)))
            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 360, height: 480), styleMask: [.borderless], backing: .buffered,
                defer: false)
            window.appearance = NSAppearance(named: appearanceName)
            window.contentView = view
            view.layoutSubtreeIfNeeded()
            view.displayIfNeeded()
            let elapsed = Date().timeIntervalSince(start)
            // Let the List's table view load its rows before capturing.
            RunLoop.main.run(until: Date().addingTimeInterval(0.5))
            view.layoutSubtreeIfNeeded()

            guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { continue }
            view.cacheDisplay(in: view.bounds, to: rep)
            let url = URL(fileURLWithPath: dir).appendingPathComponent("popover-\(name).png")
            try? rep.representation(using: .png, properties: [:])?.write(to: url)
            print("\(name): first layout \(Int(elapsed * 1000)) ms with \(model.clips.count) clips -> \(url.path)")
        }
        return true
    }
}
#endif

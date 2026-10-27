#if canImport(AppKit)
import AppKit
import CtrlCore

/// Shares clips with the core. `CtrlNode` is the real implementation.
public protocol ClipPublishing: Sendable {
    func publish(clip: OutgoingClip) throws -> Clip?
}

extension CtrlNode: ClipPublishing {}

/// Connects a pasteboard to the core in both directions.
///
/// macOS has no pasteboard change notification, so a background timer polls
/// `changeCount` every 250 ms. Reading, image conversion and publishing all
/// happen on that background queue. Writing a received clip happens on the
/// main actor, because AppKit serves lazy data there.
public final class ClipboardSync: @unchecked Sendable {
    public static let pollInterval: DispatchTimeInterval = .milliseconds(250)

    private let pasteboardName: NSPasteboard.Name
    private let publisher: ClipPublishing
    private let queue = DispatchQueue(label: "app.ctrl.clipboard-sync", qos: .userInitiated)
    private let onPublished: @Sendable (Clip) -> Void

    // Confined to `queue`.
    private var timer: DispatchSourceTimer?
    private var lastChangeCount: Int
    private var reader: PasteboardReader
    private var paused = false

    /// - Parameter onPublished: Called on a background queue for each clip sent.
    public init(
        pasteboard: NSPasteboard = .general,
        publisher: ClipPublishing,
        policy: PasteboardReader.Policy = .init(),
        onPublished: @escaping @Sendable (Clip) -> Void = { _ in }
    ) {
        pasteboardName = pasteboard.name
        self.publisher = publisher
        reader = PasteboardReader(policy: policy)
        self.onPublished = onPublished
        // Whatever is on the pasteboard at launch was copied before Ctrl ran.
        lastChangeCount = pasteboard.changeCount
    }

    deinit {
        timer?.cancel()
    }

    public func start() {
        queue.async { [self] in
            guard timer == nil else { return }
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(deadline: .now() + Self.pollInterval, repeating: Self.pollInterval, leeway: .milliseconds(25))
            t.setEventHandler { [weak self] in self?.poll() }
            t.resume()
            timer = t
        }
    }

    public func stop() {
        queue.sync {
            timer?.cancel()
            timer = nil
        }
    }

    /// While paused, copies are neither sent nor recorded.
    public func setPaused(_ paused: Bool) {
        queue.async { [self] in
            self.paused = paused
            if paused { lastChangeCount = NSPasteboard(name: pasteboardName).changeCount }
        }
    }

    public func setPolicy(_ policy: PasteboardReader.Policy) {
        queue.async { [self] in reader.policy = policy }
    }

    /// Tells the watcher that `changeCount` came from Ctrl's own write.
    public func noteOwnWrite(changeCount: Int) {
        queue.async { [self] in lastChangeCount = max(lastChangeCount, changeCount) }
    }

    /// Checks the pasteboard now instead of waiting for the next tick. For tests.
    public func pollNow() {
        queue.sync { poll() }
    }

    private func poll() {
        let pasteboard = NSPasteboard(name: pasteboardName)
        let count = pasteboard.changeCount
        guard count != lastChangeCount else { return }
        lastChangeCount = count
        guard !paused else { return }

        guard case .clip(let clip) = reader.read(pasteboard) else { return }
        // Something else copied while we were reading. The next tick picks it up.
        guard pasteboard.changeCount == count else { return }
        do {
            if let sent = try publisher.publish(clip: clip) {
                onPublished(sent)
            }
        } catch {
            NSLog("Ctrl: could not share clip: \(error)")
        }
    }
}
#endif

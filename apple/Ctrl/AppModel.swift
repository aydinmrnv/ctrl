import AppKit
import CtrlKit
import Observation
import ServiceManagement

/// Everything the menu bar popover and Settings show, plus the actions they take.
///
/// Calls into the core that can block (history, pairing, unpairing) run on a
/// detached task. Core events arrive through `NodeEvents` and are applied
/// here on the main actor, in order.
@MainActor
@Observable
final class AppModel {
    nonisolated static let historyLimit: UInt32 = 500

    enum SettingsPane: Hashable {
        case general, devices, history, privacy
    }

    private(set) var clips: [ClipSummary] = []
    private(set) var devices: [DeviceInfo] = []
    private(set) var deviceName = ""
    private(set) var fingerprint = ""
    private(set) var startupError: String?
    /// True for a moment after a clip is sent or received. Drives the menu bar icon.
    private(set) var isTransferring = false
    private(set) var pairingFailure: String?

    var settingsPane: SettingsPane = .general
    /// Set by the popover's Add Device button. The Devices pane presents the sheet.
    var wantsAddDevice = false

    var pairedDevices: [DeviceInfo] { devices.filter(\.trusted) }
    var nearbyDevices: [DeviceInfo] { devices.filter { !$0.trusted } }

    // Settings, kept in UserDefaults.
    var syncPaused: Bool {
        didSet {
            defaults.set(syncPaused, forKey: Keys.paused)
            sync?.setPaused(syncPaused)
        }
    }
    var sendImages: Bool {
        didSet {
            defaults.set(sendImages, forKey: Keys.sendImages)
            sync?.setPolicy(policy)
        }
    }
    var sendFiles: Bool {
        didSet {
            defaults.set(sendFiles, forKey: Keys.sendFiles)
            sync?.setPolicy(policy)
        }
    }
    var retention: Retention {
        didSet {
            defaults.set(Int(retention.maxItems), forKey: Keys.maxItems)
            defaults.set(Int(retention.maxAgeDays), forKey: Keys.maxDays)
            guard let node else { return }
            let retention = retention
            Task.detached {
                try? node.setRetention(retention: retention)
                await self.reloadHistory()
            }
        }
    }

    private enum Keys {
        static let paused = "syncPaused"
        static let sendImages = "sendImages"
        static let sendFiles = "sendFiles"
        static let maxItems = "historyMaxItems"
        static let maxDays = "historyMaxDays"
    }

    private let defaults: UserDefaults
    let node: CtrlNode?
    private var sync: ClipboardSync?
    private var writer: PasteboardWriter?
    private var flashGeneration = 0

    private var policy: PasteboardReader.Policy {
        PasteboardReader.Policy(sendImages: sendImages, sendFiles: sendFiles)
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        defaults.register(defaults: [
            Keys.sendImages: true, Keys.sendFiles: true, Keys.maxItems: 200, Keys.maxDays: 30,
        ])
        syncPaused = defaults.bool(forKey: Keys.paused)
        sendImages = defaults.bool(forKey: Keys.sendImages)
        sendFiles = defaults.bool(forKey: Keys.sendFiles)
        let retention = Retention(
            maxItems: UInt32(defaults.integer(forKey: Keys.maxItems)),
            maxAgeDays: UInt32(defaults.integer(forKey: Keys.maxDays)))
        self.retention = retention

        let events = NodeEvents()
        do {
            let support = try FileManager.default.url(
                for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
            let node = try CtrlNode(
                config: NodeConfig(
                    deviceName: Host.current().localizedName ?? "Mac",
                    dataDir: support.appendingPathComponent("Ctrl", isDirectory: true).path,
                    listenAddress: "0.0.0.0:0",
                    discovery: true,
                    retention: retention),
                secrets: KeychainSecretStore(),
                listener: events)
            self.node = node
        } catch {
            node = nil
            startupError = error.localizedDescription
            return
        }
        start(events)
    }

    /// A model with canned data and no core, for screenshots and previews.
    init(demo clips: [ClipSummary], devices: [DeviceInfo], deviceName: String) {
        defaults = UserDefaults(suiteName: "app.ctrl.demo") ?? .standard
        syncPaused = false
        sendImages = true
        sendFiles = true
        retention = Retention(maxItems: 200, maxAgeDays: 30)
        node = nil
        self.clips = clips
        self.devices = devices
        self.deviceName = deviceName
    }

    private func start(_ events: NodeEvents) {
        guard let node else { return }
        deviceName = node.deviceName()
        fingerprint = node.fingerprint()

        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        writer = PasteboardWriter(
            fetcher: node, receivedDirectory: caches.appendingPathComponent("app.ctrl.Ctrl/Received", isDirectory: true))
        let sync = ClipboardSync(publisher: node, policy: policy) { [weak self] clip in
            Task { @MainActor in self?.didPublish(clip) }
        }
        sync.setPaused(syncPaused)
        sync.start()
        self.sync = sync

        Task { [weak self] in
            for await event in events.stream {
                self?.handle(event)
            }
        }
        refreshDevices()
        Task { await reloadHistory() }
    }

    func shutdown() {
        sync?.stop()
        node?.shutdown()
    }

    // MARK: Events

    private func handle(_ event: NodeEvent) {
        switch event {
        case .clip(let clip):
            guard !syncPaused else { return }
            if let writer {
                sync?.noteOwnWrite(changeCount: writer.write(clip, to: .general))
            }
            if !clip.transient { insert(ClipSummary(clip)) }
            flash()
        case .device(let event):
            if case .paired = event { pairingFailure = nil }
            refreshDevices()
        case .pairingFailed(let reason):
            pairingFailure = reason
        }
    }

    private func didPublish(_ clip: Clip) {
        if !clip.transient { insert(ClipSummary(clip)) }
        if devices.contains(where: { $0.trusted && $0.connected }) { flash() }
    }

    private func insert(_ summary: ClipSummary) {
        clips.removeAll { $0.id == summary.id }
        clips.insert(summary, at: 0)
        if clips.count > Int(min(retention.maxItems, Self.historyLimit)) { clips.removeLast() }
    }

    /// Swaps the menu bar icon for under a second. Skipped when Reduce Motion is on.
    private func flash() {
        guard !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else { return }
        flashGeneration += 1
        let generation = flashGeneration
        isTransferring = true
        Task {
            try? await Task.sleep(for: .milliseconds(800))
            if generation == flashGeneration { isTransferring = false }
        }
    }

    func refreshDevices() {
        guard let node else { return }
        Task.detached {
            let devices = node.devices()
            await MainActor.run { self.devices = devices }
        }
    }

    func reloadHistory() async {
        guard let node else { return }
        let summaries = await Task.detached {
            ((try? node.history(limit: Self.historyLimit, offset: 0)) ?? []).map(ClipSummary.init)
        }.value
        clips = summaries
    }

    // MARK: Clips

    func deviceLabel(for id: String) -> String {
        if id == node?.deviceId() || node == nil && id == "this-mac" { return "This Mac" }
        if let device = devices.first(where: { $0.deviceId == id }), let name = device.name { return name }
        return "Device \(id.prefix(6))"
    }

    /// Puts a history item back on the pasteboard. With `plainText`, only its
    /// text goes, and it's pasted into the frontmost app when Ctrl has
    /// Accessibility access.
    func copy(_ summary: ClipSummary, plainText: Bool = false) {
        guard let node, let writer else { return }
        let id = summary.id
        Task {
            let clip = await Task.detached { try? node.historyItem(clipId: id) }.value
            guard let clip else { return }
            sync?.noteOwnWrite(changeCount: writer.write(clip, to: .general, plainTextOnly: plainText))
            if plainText { Paster.pasteIntoFrontmostApp() }
        }
    }

    func delete(_ summary: ClipSummary) {
        clips.removeAll { $0.id == summary.id }
        guard let node else { return }
        let id = summary.id
        Task.detached { _ = try? node.deleteHistoryItem(clipId: id) }
    }

    func clearHistory() {
        clips.removeAll()
        guard let node else { return }
        Task.detached { try? node.clearHistory() }
    }

    // MARK: Devices

    func setDeviceName(_ name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let node, !name.isEmpty, name != deviceName else { return }
        deviceName = name
        Task.detached { try? node.setDeviceName(name: name) }
    }

    func beginPairing() -> PairingCode? {
        pairingFailure = nil
        return node?.beginPairing()
    }

    func cancelPairing() {
        node?.cancelPairing()
    }

    func pair(with deviceID: String, code: String) async throws {
        guard let node else { return }
        _ = try await Task.detached { try node.pairWith(target: deviceID, code: code) }.value
        refreshDevices()
    }

    func unpair(_ device: DeviceInfo) {
        guard let node else { return }
        let id = device.deviceId
        Task.detached {
            try? node.unpair(deviceId: id)
            await self.refreshDevices()
        }
    }

    func rename(_ device: DeviceInfo, to name: String) {
        guard let node else { return }
        let id = device.deviceId
        Task.detached {
            _ = try? node.renameDevice(deviceId: id, name: name)
            await self.refreshDevices()
        }
    }

    // MARK: Launch at login

    var opensAtLogin: Bool {
        get { SMAppService.mainApp.status == .enabled }
        set {
            do {
                if newValue { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            } catch {
                NSLog("Ctrl: could not change login item: \(error)")
            }
        }
    }
}

// Copied into bindings/apple/Tests by scripts/build-xcframework.sh. Checks that
// the generated Swift bindings link and that callbacks work in both directions.
import CtrlCore
import Foundation
import XCTest

final class MemoryStore: SecretStore, @unchecked Sendable {
    private var items: [String: Data] = [:]
    private let lock = NSLock()

    func get(key: String) throws -> Data? { lock.withLock { items[key] } }
    func set(key: String, value: Data) throws { lock.withLock { items[key] = value } }
    func delete(key: String) throws { lock.withLock { _ = items.removeValue(forKey: key) } }
    func keys() throws -> [String] { lock.withLock { Array(items.keys) } }
}

final class Events: NodeListener, @unchecked Sendable {
    func onDeviceEvent(event: DeviceEvent) {}
    func onClip(clip: Clip) {}
    func onPairingFailed(reason: String) {}
}

final class SmokeTests: XCTestCase {
    func testFormatMapping() {
        XCTAssertEqual(canonicalFormatForApple(uti: "public.png"), "image/png")
        XCTAssertEqual(windowsFormatsForFormat(format: "text/plain").first, "CF_UNICODETEXT")
        XCTAssertTrue(clipFlagsForApple(types: ["org.nspasteboard.ConcealedType"]).concealed)
    }

    func testNodeStartsAndRecordsHistory() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let store = MemoryStore()
        let node = try CtrlNode(
            config: NodeConfig(
                deviceName: "Swift",
                dataDir: dir.path,
                listenAddress: "127.0.0.1:0",
                discovery: false,
                retention: Retention(maxItems: 10, maxAgeDays: 1)
            ),
            secrets: store,
            listener: Events()
        )
        defer { node.shutdown() }
        XCTAssertFalse(node.deviceId().isEmpty)
        XCTAssertNotNil(try store.get(key: "ctrl.identity"), "identity went through the Swift store")

        let text = OutgoingRepresentation(
            format: "text/plain", source: .bytes(data: Data("hi".utf8)), name: nil, thumbnailPng: nil)
        let sent = try node.publish(clip: OutgoingClip(representations: [text], flags: ClipFlags(concealed: false, transient: false)))
        XCTAssertNotNil(sent)
        XCTAssertEqual(try node.history(limit: 10, offset: 0).count, 1)

        let secret = OutgoingClip(representations: [text], flags: ClipFlags(concealed: true, transient: false))
        XCTAssertNil(try node.publish(clip: secret))
    }
}

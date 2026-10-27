import AppKit
import CtrlCore
@testable import CtrlKit
import XCTest

final class MemoryStore: SecretStore, @unchecked Sendable {
    private var items: [String: Data] = [:]
    private let lock = NSLock()

    func get(key: String) throws -> Data? { lock.withLock { items[key] } }
    func set(key: String, value: Data) throws { lock.withLock { items[key] = value } }
    func delete(key: String) throws { lock.withLock { _ = items.removeValue(forKey: key) } }
    func keys() throws -> [String] { lock.withLock { Array(items.keys) } }
}

/// Two nodes on loopback, each with its own pasteboard: copy on one, paste on the other.
@MainActor
final class EndToEndTests: XCTestCase {
    func makeNode(_ name: String, _ events: NodeEvents) throws -> CtrlNode {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        return try CtrlNode(
            config: NodeConfig(
                deviceName: name, dataDir: dir.path, listenAddress: "127.0.0.1:0", discovery: false,
                retention: Retention(maxItems: 50, maxAgeDays: 1)),
            secrets: MemoryStore(), listener: events)
    }

    func testCopyOnOneMacPastesOnTheOther() async throws {
        let eventsA = NodeEvents(), eventsB = NodeEvents()
        let a = try makeNode("A", eventsA)
        let b = try makeNode("B", eventsB)
        defer {
            a.shutdown()
            b.shutdown()
        }

        let code = a.beginPairing()
        let address = try a.listenAddress()
        _ = try await Task.detached { try b.pairWith(target: address, code: code.code) }.value

        let pbA = uniquePasteboard(), pbB = uniquePasteboard()
        let sync = ClipboardSync(pasteboard: pbA, publisher: a)
        let writer = PasteboardWriter(
            fetcher: b, receivedDirectory: FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString))

        // Let the session come up so the timing below measures a copy, not a handshake.
        var eventsFromB = eventsB.stream.makeAsyncIterator()
        while let event = await eventsFromB.next() {
            if case .device(.connected) = event { break }
        }

        pbA.clearContents()
        pbA.setString("over the LAN", forType: .string)
        let copied = Date()
        sync.pollNow()

        var received: Clip?
        while let event = await eventsFromB.next() {
            if case .clip(let clip) = event {
                received = clip
                break
            }
        }
        let clip = try XCTUnwrap(received)
        writer.write(clip, to: pbB)
        let elapsed = Date().timeIntervalSince(copied)

        XCTAssertEqual(pbB.string(forType: .string), "over the LAN")
        XCTAssertLessThan(elapsed, 0.3, "copy to pasteable took \(Int(elapsed * 1000)) ms")

        // B's watcher must not send the clip back to A.
        let back = ClipboardSync(pasteboard: pbB, publisher: b)
        pbB.clearContents()
        writer.write(clip, to: pbB)
        back.pollNow()
        XCTAssertEqual(try a.history(limit: 10, offset: 0).count, 1, "the clip came back to its origin")
    }

    func testConcealedCopiesAreNeverSentOrRecorded() async throws {
        let a = try makeNode("A", NodeEvents())
        defer { a.shutdown() }
        let pb = uniquePasteboard()
        let publisher = CountingPublisher(a)
        let sync = ClipboardSync(pasteboard: pb, publisher: publisher)

        pb.clearContents()
        let item = NSPasteboardItem()
        item.setString("hunter2", forType: .string)
        item.setData(Data(), forType: NSPasteboard.PasteboardType(PasteboardMarker.concealed))
        pb.writeObjects([item])
        sync.pollNow()

        XCTAssertEqual(try a.history(limit: 10, offset: 0).count, 0)
        XCTAssertEqual(publisher.calls, 0)
    }
}

final class CountingPublisher: ClipPublishing, @unchecked Sendable {
    let node: CtrlNode
    private let lock = NSLock()
    private var _calls = 0
    var calls: Int { lock.withLock { _calls } }

    init(_ node: CtrlNode) { self.node = node }

    func publish(clip: OutgoingClip) throws -> Clip? {
        lock.withLock { _calls += 1 }
        return try node.publish(clip: clip)
    }
}

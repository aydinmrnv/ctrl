import AppKit
import CtrlCore
@testable import CtrlKit
import XCTest

final class FakeFetcher: PayloadFetching, @unchecked Sendable {
    let payload: Data
    private let lock = NSLock()
    private var _fetches = 0
    var fetches: Int { lock.withLock { _fetches } }

    init(payload: Data) { self.payload = payload }

    func fetchData(_ handle: LazyHandle) throws -> Data {
        lock.withLock { _fetches += 1 }
        return payload
    }

    func fetchFile(_ handle: LazyHandle, to path: String, listener: TransferListener) throws -> Transfer? {
        lock.withLock { _fetches += 1 }
        try payload.write(to: URL(fileURLWithPath: path))
        listener.onComplete(path: path)
        return nil
    }
}

func uniquePasteboard() -> NSPasteboard {
    NSPasteboard(name: NSPasteboard.Name("app.ctrl.tests.\(UUID().uuidString)"))
}

func rep(_ format: String, _ data: Data? = nil, lazy: UInt64? = nil, name: String? = nil) -> Representation {
    Representation(
        format: format, size: UInt64(data?.count ?? Int(lazy ?? 0)), name: name, thumbnailPng: nil, inlineBytes: data,
        lazyHandle: lazy.map { LazyHandle(deviceId: "peer", token: "t", size: $0) }, localPath: nil)
}

func tinyTIFF() -> Data {
    let image = NSImage(size: NSSize(width: 4, height: 3), flipped: false) { rect in
        NSColor.systemRed.setFill()
        rect.fill()
        return true
    }
    return image.tiffRepresentation!
}

final class PasteboardReaderTests: XCTestCase {
    let reader = PasteboardReader()

    func testPlainTextAndHTML() throws {
        let pb = uniquePasteboard()
        pb.clearContents()
        let item = NSPasteboardItem()
        item.setString("hello", forType: .string)
        item.setString("<b>hello</b>", forType: .html)
        pb.writeObjects([item])

        guard case .clip(let clip) = reader.read(pb) else { return XCTFail("expected a clip") }
        XCTAssertEqual(clip.representations.map(\.format), [Format.plainText, Format.html])
        XCTAssertEqual(clip.representations[0].source, .bytes(data: Data("hello".utf8)))
        XCTAssertFalse(clip.flags.concealed)
    }

    func testPrivacyMarkersAreNeverRead() {
        for marker in PasteboardMarker.privacy {
            let pb = uniquePasteboard()
            pb.clearContents()
            let item = NSPasteboardItem()
            item.setString("hunter2", forType: .string)
            item.setData(Data(), forType: NSPasteboard.PasteboardType(marker))
            pb.writeObjects([item])
            guard case .privateItem = reader.read(pb) else { return XCTFail("\(marker) was not treated as private") }
        }
    }

    func testOwnWritesAreSkipped() {
        let pb = uniquePasteboard()
        pb.clearContents()
        let item = NSPasteboardItem()
        item.setString("from a peer", forType: .string)
        item.setString("peer", forType: NSPasteboard.PasteboardType(PasteboardMarker.origin))
        pb.writeObjects([item])
        guard case .ownWrite = reader.read(pb) else { return XCTFail("expected ownWrite") }
    }

    func testTIFFIsSentAsPNGWithThumbnail() throws {
        let pb = uniquePasteboard()
        pb.clearContents()
        pb.setData(tinyTIFF(), forType: .tiff)

        guard case .clip(let clip) = reader.read(pb) else { return XCTFail("expected a clip") }
        XCTAssertEqual(clip.representations.map(\.format), [Format.png])
        guard case .bytes(let png) = clip.representations[0].source else { return XCTFail() }
        XCTAssertEqual(Array(png.prefix(4)), [0x89, 0x50, 0x4E, 0x47])
        XCTAssertNotNil(clip.representations[0].thumbnailPng)
    }

    func testImagesRespectPolicy() {
        let pb = uniquePasteboard()
        pb.clearContents()
        pb.setData(tinyTIFF(), forType: .tiff)
        guard case .empty = PasteboardReader(policy: .init(sendImages: false)).read(pb) else {
            return XCTFail("images should be skipped")
        }
    }

    func testFilesAreSentByPathAndFoldersAreSkipped() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let file = dir.appendingPathComponent("notes.txt")
        try Data("x".utf8).write(to: file)

        let pb = uniquePasteboard()
        pb.clearContents()
        pb.writeObjects([file as NSURL, dir as NSURL])

        guard case .clip(let clip) = reader.read(pb) else { return XCTFail("expected a clip") }
        XCTAssertEqual(clip.representations.count, 1)
        XCTAssertEqual(clip.representations[0].format, Format.fileURL)
        XCTAssertEqual(clip.representations[0].name, "notes.txt")
        XCTAssertEqual(clip.representations[0].source, .file(path: file.path))
    }
}

@MainActor
final class PasteboardWriterTests: XCTestCase {
    func writer(_ fetcher: PayloadFetching) -> PasteboardWriter {
        PasteboardWriter(
            fetcher: fetcher,
            receivedDirectory: FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString))
    }

    func clip(_ reps: [Representation]) -> Clip {
        Clip(id: UUID().uuidString, originDevice: "peer", createdAtMs: 0, transient: false, representations: reps)
    }

    func testInlineTextIsMarkedWithItsOrigin() {
        let pb = uniquePasteboard()
        writer(FakeFetcher(payload: Data())).write(clip([rep(Format.plainText, Data("hi".utf8))]), to: pb)
        XCTAssertEqual(pb.string(forType: .string), "hi")
        XCTAssertEqual(pb.string(forType: NSPasteboard.PasteboardType(PasteboardMarker.origin)), "peer")
        guard case .ownWrite = PasteboardReader().read(pb) else { return XCTFail("loop prevention failed") }
    }

    func testLazyDataIsFetchedOnlyWhenRead() {
        let pb = uniquePasteboard()
        let fetcher = FakeFetcher(payload: Data("big image".utf8))
        let w = writer(fetcher)
        w.write(clip([rep(Format.png, lazy: 1_000_000)]), to: pb)
        XCTAssertEqual(fetcher.fetches, 0, "nothing fetched until someone pastes")
        XCTAssertEqual(pb.data(forType: .png), Data("big image".utf8))
        XCTAssertEqual(fetcher.fetches, 1)
    }

    func testInlineFilesBecomeLocalFileURLs() throws {
        let pb = uniquePasteboard()
        writer(FakeFetcher(payload: Data())).write(
            clip([rep(Format.fileURL, Data("contents".utf8), name: "../evil.txt")]), to: pb)
        let url = try XCTUnwrap(pb.readObjects(forClasses: [NSURL.self]) as? [URL]).first
        XCTAssertEqual(url?.lastPathComponent, "-evil.txt")
        XCTAssertEqual(try Data(contentsOf: XCTUnwrap(url)), Data("contents".utf8))
    }

    func testLazyFilesArePromised() {
        let pb = uniquePasteboard()
        writer(FakeFetcher(payload: Data())).write(clip([rep(Format.fileURL, lazy: 2_000_000_000, name: "movie.mov")]), to: pb)
        let types = pb.types?.map(\.rawValue) ?? []
        XCTAssertTrue(types.contains("com.apple.NSFilePromiseItemMetaData"), "\(types)")
        XCTAssertTrue(types.contains(PasteboardMarker.origin))
    }

    func testPlainTextOnlyDropsRichFormats() {
        let pb = uniquePasteboard()
        writer(FakeFetcher(payload: Data())).write(
            clip([rep(Format.plainText, Data("hi".utf8)), rep(Format.html, Data("<b>hi</b>".utf8))]), to: pb,
            plainTextOnly: true)
        XCTAssertEqual(pb.string(forType: .string), "hi")
        XCTAssertNil(pb.data(forType: .html))
    }

    func testSafeFileName() {
        XCTAssertEqual(PasteboardWriter.safeFileName("a/b:c.txt"), "a-b-c.txt")
        XCTAssertEqual(PasteboardWriter.safeFileName(".."), "Untitled")
        XCTAssertEqual(PasteboardWriter.safeFileName(nil), "Untitled")
        XCTAssertEqual(PasteboardWriter.safeFileName(".hidden"), "hidden")
    }
}

final class ClipSummaryTests: XCTestCase {
    func clip(_ reps: [Representation]) -> Clip {
        Clip(id: "c", originDevice: "d", createdAtMs: 1_700_000_000_000, transient: false, representations: reps)
    }

    func testText() {
        let s = ClipSummary(clip([rep(Format.plainText, Data("  hello\n\n  world  ".utf8))]))
        XCTAssertEqual(s.kind, .text)
        XCTAssertEqual(s.preview, "hello world")
        XCTAssertTrue(s.hasPlainText)
    }

    func testRichText() {
        let s = ClipSummary(clip([rep(Format.plainText, Data("hi".utf8)), rep(Format.rtf, Data("{\\rtf1 hi}".utf8))]))
        XCTAssertEqual(s.kind, .richText)
    }

    func testFiles() {
        let s = ClipSummary(clip([rep(Format.fileURL, lazy: 10, name: "a.pdf"), rep(Format.fileURL, lazy: 10, name: "b.pdf")]))
        XCTAssertEqual(s.kind, .file)
        XCTAssertEqual(s.preview, "a.pdf and 1 more")
        XCTAssertEqual(s.totalSize, 20)
    }

    func testURL() {
        let s = ClipSummary(clip([rep(Format.url, Data("https://example.com".utf8))]))
        XCTAssertEqual(s.kind, .url)
    }

    func testLongTextIsCapped() {
        let s = ClipSummary(clip([rep(Format.plainText, Data(String(repeating: "a", count: 10_000).utf8))]))
        XCTAssertEqual(s.preview.count, ClipSummary.previewLength)
    }
}

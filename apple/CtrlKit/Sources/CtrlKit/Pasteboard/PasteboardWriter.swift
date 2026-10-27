#if canImport(AppKit)
import AppKit
import Synchronization
import CtrlCore
import UniformTypeIdentifiers

/// Puts a clip from the core on an `NSPasteboard`.
///
/// Small representations are written directly. Large ones are promised and
/// only fetched when an app asks for them: data through an
/// `NSPasteboardItemDataProvider`, files through an `NSFilePromiseProvider`
/// that downloads on a background queue, so pasting a 2 GB file into Finder
/// never touches the main thread.
///
/// Every item carries `PasteboardMarker.origin`, so the watcher doesn't send
/// the clip back out.
@MainActor
public final class PasteboardWriter {
    private let fetcher: PayloadFetching
    /// Where inline files are written so they can go on the pasteboard as URLs.
    private let receivedDirectory: URL
    /// Providers for the clip currently on the pasteboard. The pasteboard keeps
    /// its own references too, but data providers are easy to lose.
    private var live: [AnyObject] = []

    public init(fetcher: PayloadFetching, receivedDirectory: URL) {
        self.fetcher = fetcher
        self.receivedDirectory = receivedDirectory
    }

    /// Writes `clip` and returns the pasteboard's new change count.
    @discardableResult
    public func write(_ clip: Clip, to pasteboard: NSPasteboard, plainTextOnly: Bool = false) -> Int {
        live.removeAll()
        let origin = clip.originDevice
        var writers: [NSPasteboardWriting] = []

        let files = plainTextOnly ? [] : clip.representations.filter { $0.format == Format.fileURL }
        for rep in files {
            if let writer = fileWriter(rep, clipID: clip.id, origin: origin) { writers.append(writer) }
        }

        let item = NSPasteboardItem()
        var hasData = false
        for rep in clip.representations where rep.format != Format.fileURL && rep.format != Format.fileList {
            if plainTextOnly && rep.format != Format.plainText && rep.format != Format.url { continue }
            guard let type = appleTypesForFormat(format: rep.format).first else { continue }
            let pbType = NSPasteboard.PasteboardType(type)
            if let bytes = rep.inlineBytes {
                item.setData(bytes, forType: pbType)
            } else if rep.lazyHandle != nil || rep.localPath != nil {
                let provider = LazyDataProvider(representation: rep, fetcher: fetcher)
                item.setDataProvider(provider, forTypes: [pbType])
                live.append(provider)
            } else {
                continue
            }
            hasData = true
        }
        if plainTextOnly && !hasData {
            let names = clip.representations.compactMap { $0.localPath ?? $0.name }
            if !names.isEmpty {
                item.setString(names.joined(separator: "\n"), forType: .string)
                hasData = true
            }
        }
        if hasData {
            item.setString(origin, forType: NSPasteboard.PasteboardType(PasteboardMarker.origin))
            writers.insert(item, at: 0)
        }

        pasteboard.clearContents()
        if !writers.isEmpty {
            pasteboard.writeObjects(writers)
        }
        live.append(contentsOf: writers as [AnyObject])
        return pasteboard.changeCount
    }

    private func fileWriter(_ rep: Representation, clipID: String, origin: String) -> NSPasteboardWriting? {
        let name = Self.safeFileName(rep.name)
        if let path = rep.localPath, FileManager.default.fileExists(atPath: path) {
            return Self.fileItem(URL(fileURLWithPath: path), origin: origin)
        }
        if let bytes = rep.inlineBytes {
            let dir = receivedDirectory.appendingPathComponent(clipID, isDirectory: true)
            let url = dir.appendingPathComponent(name)
            do {
                try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                try bytes.write(to: url)
            } catch {
                return nil
            }
            return Self.fileItem(url, origin: origin)
        }
        guard let handle = rep.lazyHandle else { return nil }
        let promise = LazyFilePromise(fileName: name, handle: handle, origin: origin, fetcher: fetcher)
        return promise.provider
    }

    private static func fileItem(_ url: URL, origin: String) -> NSPasteboardItem {
        let item = NSPasteboardItem()
        item.setString(url.absoluteString, forType: .fileURL)
        item.setString(origin, forType: NSPasteboard.PasteboardType(PasteboardMarker.origin))
        return item
    }

    /// The sender chose the name, so strip anything that could leave the
    /// destination folder.
    static func safeFileName(_ name: String?) -> String {
        let cleaned = (name ?? "").replacingOccurrences(of: "/", with: "-").replacingOccurrences(of: ":", with: "-")
        let trimmed = cleaned.trimmingCharacters(in: .whitespacesAndNewlines).drop { $0 == "." }
        return trimmed.isEmpty ? "Untitled" : String(trimmed)
    }
}

/// Supplies a large non-file representation when an app reads it.
///
/// AppKit calls this on the main thread and needs the bytes before it
/// returns, so the fetch blocks. Only representations over 256 KB get here,
/// and those that aren't files are images and long rich text, which take tens
/// of milliseconds on a LAN.
final class LazyDataProvider: NSObject, NSPasteboardItemDataProvider, @unchecked Sendable {
    private let representation: Representation
    private let fetcher: PayloadFetching

    init(representation: Representation, fetcher: PayloadFetching) {
        self.representation = representation
        self.fetcher = fetcher
    }

    func pasteboard(_ pasteboard: NSPasteboard?, item: NSPasteboardItem, provideDataForType type: NSPasteboard.PasteboardType) {
        if let path = representation.localPath, let data = FileManager.default.contents(atPath: path) {
            item.setData(data, forType: type)
        } else if let handle = representation.lazyHandle, let data = try? fetcher.fetchData(handle) {
            item.setData(data, forType: type)
        }
    }
}

/// A promised file, downloaded when it's pasted or dropped.
final class LazyFilePromise: NSObject, NSFilePromiseProviderDelegate, @unchecked Sendable {
    private let fileName: String
    private let handle: LazyHandle
    private let fetcher: PayloadFetching
    private let queue: OperationQueue = {
        let q = OperationQueue()
        q.qualityOfService = .userInitiated
        q.name = "app.ctrl.file-promise"
        return q
    }()
    let provider: MarkedFilePromiseProvider

    init(fileName: String, handle: LazyHandle, origin: String, fetcher: PayloadFetching) {
        self.fileName = fileName
        self.handle = handle
        self.fetcher = fetcher
        let ext = (fileName as NSString).pathExtension
        let type = UTType(filenameExtension: ext) ?? .data
        provider = MarkedFilePromiseProvider()
        provider.fileType = type.identifier
        provider.origin = origin
        super.init()
        provider.delegate = self
        // The provider holds its delegate weakly; this keeps the pair alive as
        // long as the pasteboard holds the provider.
        provider.userInfo = self
    }

    func filePromiseProvider(_ provider: NSFilePromiseProvider, fileNameForType fileType: String) -> String {
        fileName
    }

    func operationQueue(for filePromiseProvider: NSFilePromiseProvider) -> OperationQueue {
        queue
    }

    func filePromiseProvider(
        _ provider: NSFilePromiseProvider, writePromiseTo url: URL, completionHandler: @escaping ((any Error)?) -> Void
    ) {
        let done = DispatchSemaphore(value: 0)
        let outcome = Mutex<(any Error)?>(nil)
        let callbacks = TransferCallbacks { result in
            if case .failure(let error) = result { outcome.withLock { $0 = error } }
            done.signal()
        }
        do {
            _ = try fetcher.fetchFile(handle, to: url.path, listener: callbacks)
            // Hold this operation until the download ends so the receiving app
            // sees the file only once it's complete.
            done.wait()
            completionHandler(outcome.withLock { $0 })
        } catch {
            completionHandler(error)
        }
    }
}

/// An `NSFilePromiseProvider` that also carries Ctrl's origin marker.
final class MarkedFilePromiseProvider: NSFilePromiseProvider {
    var origin = ""

    override func writableTypes(for pasteboard: NSPasteboard) -> [NSPasteboard.PasteboardType] {
        super.writableTypes(for: pasteboard) + [NSPasteboard.PasteboardType(PasteboardMarker.origin)]
    }

    override func pasteboardPropertyList(forType type: NSPasteboard.PasteboardType) -> Any? {
        if type.rawValue == PasteboardMarker.origin { return origin }
        return super.pasteboardPropertyList(forType: type)
    }
}
#endif

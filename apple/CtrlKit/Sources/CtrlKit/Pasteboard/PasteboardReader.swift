#if canImport(AppKit)
import AppKit
import CtrlCore
import ImageIO
import UniformTypeIdentifiers

/// Turns what's on an `NSPasteboard` into an `OutgoingClip` for the core.
public struct PasteboardReader: Sendable {
    public enum Result {
        case clip(OutgoingClip)
        /// Nothing Ctrl knows how to sync.
        case empty
        /// Marked concealed, transient or auto-generated. Never sent or recorded.
        case privateItem
        /// Ctrl wrote this item itself.
        case ownWrite
    }

    public struct Policy: Sendable {
        public var sendImages = true
        public var sendFiles = true
        public init(sendImages: Bool = true, sendFiles: Bool = true) {
            self.sendImages = sendImages
            self.sendFiles = sendFiles
        }
    }

    public var policy: Policy

    public init(policy: Policy = Policy()) {
        self.policy = policy
    }

    /// Longest side of the thumbnail attached to images, in pixels.
    static let thumbnailSize = 160

    public func read(_ pasteboard: NSPasteboard) -> Result {
        guard let items = pasteboard.pasteboardItems, !items.isEmpty else { return .empty }
        let allTypes = Set(items.flatMap { $0.types.map(\.rawValue) })
        if allTypes.contains(PasteboardMarker.origin) { return .ownWrite }
        if !allTypes.isDisjoint(with: PasteboardMarker.privacy) { return .privateItem }

        let files = items.compactMap(fileRepresentation)
        if !files.isEmpty {
            guard policy.sendFiles else { return .empty }
            return .clip(OutgoingClip(representations: files, flags: flags(allTypes)))
        }

        // Several non-file items (rare outside Finder) would give a clip with
        // competing representations of one format, so only the first counts.
        let reps = representations(of: items[0])
        return reps.isEmpty ? .empty : .clip(OutgoingClip(representations: reps, flags: flags(allTypes)))
    }

    private func flags(_ types: Set<String>) -> ClipFlags {
        clipFlagsForApple(types: Array(types))
    }

    private func fileRepresentation(_ item: NSPasteboardItem) -> OutgoingRepresentation? {
        guard let string = item.string(forType: .fileURL), let url = URL(string: string), url.isFileURL else {
            return nil
        }
        // The core sends regular files only. Folders would need archiving first.
        let values = try? url.resourceValues(forKeys: [.isRegularFileKey])
        guard values?.isRegularFile == true else { return nil }
        return OutgoingRepresentation(
            format: Format.fileURL, source: .file(path: url.path), name: url.lastPathComponent, thumbnailPng: nil)
    }

    private func representations(of item: NSPasteboardItem) -> [OutgoingRepresentation] {
        var seen = Set<String>()
        var reps: [OutgoingRepresentation] = []
        let types = item.types.map(\.rawValue)
        let hasPNG = types.contains { canonicalFormatForApple(uti: $0) == Format.png }

        for type in types {
            guard let format = canonicalFormatForApple(uti: type), !seen.contains(format) else { continue }
            if format == Format.fileURL || format == Format.fileList { continue }
            seen.insert(format)
            if Format.images.contains(format) && !policy.sendImages { continue }
            guard var data = item.data(forType: NSPasteboard.PasteboardType(type)) else { continue }

            var outFormat = format
            if format == Format.tiff {
                // TIFF is what most Mac apps copy, but it's large and Windows
                // barely reads it. Send PNG instead, unless PNG is already there.
                if hasPNG { continue }
                guard let png = Self.png(fromImageData: data) else { continue }
                data = png
                outFormat = Format.png
                seen.insert(Format.png)
            }
            let thumbnail = Format.images.contains(outFormat) ? Self.thumbnail(of: data) : nil
            reps.append(OutgoingRepresentation(format: outFormat, source: .bytes(data: data), name: nil, thumbnailPng: thumbnail))
        }
        return reps
    }

    static func png(fromImageData data: Data) -> Data? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else { return nil }
        return encodePNG(image)
    }

    /// A small PNG for history rows. The core drops thumbnails over 64 KB.
    static func thumbnail(of data: Data) -> Data? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: thumbnailSize,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        return encodePNG(image)
    }

    private static func encodePNG(_ image: CGImage) -> Data? {
        let out = NSMutableData()
        guard let dest = CGImageDestinationCreateWithData(out, UTType.png.identifier as CFString, 1, nil) else {
            return nil
        }
        CGImageDestinationAddImage(dest, image, nil)
        return CGImageDestinationFinalize(dest) ? out as Data : nil
    }
}
#endif

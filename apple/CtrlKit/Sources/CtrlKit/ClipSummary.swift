import CtrlCore
import Foundation

/// What a history row needs to draw a clip, without the clip's bytes.
///
/// History can hold hundreds of clips with up to 256 KB inline each, so the
/// UI keeps summaries and loads the full clip from the core only when the user
/// copies one.
public struct ClipSummary: Identifiable, Hashable, Sendable {
    public enum Kind: String, Sendable {
        case text, richText, url, image, file, color, other
    }

    public let id: String
    public let originDevice: String
    public let createdAt: Date
    public let kind: Kind
    /// One line of text for the row: the start of the text, the URL, or the file names.
    public let preview: String
    /// PNG or other image data small enough to decode for a thumbnail.
    public let thumbnail: Data?
    /// For files: the first file's name, used to pick its icon.
    public let fileName: String?
    /// For files that exist on this Mac: the first file's path.
    public let localPath: String?
    public let totalSize: UInt64
    /// Whether the clip has a representation that can be pasted as plain text.
    public let hasPlainText: Bool

    public init(
        id: String, originDevice: String, createdAt: Date, kind: Kind, preview: String, thumbnail: Data? = nil,
        fileName: String? = nil, localPath: String? = nil, totalSize: UInt64 = 0, hasPlainText: Bool = false
    ) {
        self.id = id
        self.originDevice = originDevice
        self.createdAt = createdAt
        self.kind = kind
        self.preview = preview
        self.thumbnail = thumbnail
        self.fileName = fileName
        self.localPath = localPath
        self.totalSize = totalSize
        self.hasPlainText = hasPlainText
    }

    /// Longest preview kept, in characters. Rows show one or two lines.
    static let previewLength = 300

    public init(_ clip: Clip) {
        let reps = clip.representations
        let formats = Set(reps.map(\.format))
        let files = reps.filter { $0.format == Format.fileURL }
        let text = reps.first { $0.format == Format.plainText }.flatMap(\.inlineBytes).map { String(decoding: $0, as: UTF8.self) }
        let url = reps.first { $0.format == Format.url }.flatMap(\.inlineBytes).map { String(decoding: $0, as: UTF8.self) }
        let image = reps.first { Format.images.contains($0.format) }

        id = clip.id
        originDevice = clip.originDevice
        createdAt = Date(timeIntervalSince1970: TimeInterval(clip.createdAtMs) / 1000)
        totalSize = reps.reduce(0) { $0 + $1.size }
        hasPlainText = text != nil || url != nil || !files.isEmpty

        if let first = files.first {
            kind = .file
            let names = files.map { $0.name ?? "File" }
            preview = names.count == 1 ? names[0] : "\(names[0]) and \(names.count - 1) more"
            fileName = first.name
            localPath = first.localPath
            thumbnail = first.thumbnailPng
        } else if let image {
            kind = .image
            preview = text.map(Self.oneLine) ?? "Image"
            // Inline image bytes are at most 256 KB, so they make a fine thumbnail
            // when the sender didn't attach one.
            thumbnail = image.thumbnailPng ?? image.inlineBytes
            fileName = nil
            localPath = image.localPath
        } else if let url, text == nil || text == url {
            kind = .url
            preview = Self.oneLine(url)
            thumbnail = nil
            fileName = nil
            localPath = nil
        } else if let text {
            kind = formats.contains(Format.html) || formats.contains(Format.rtf) ? .richText : .text
            preview = Self.oneLine(text)
            thumbnail = nil
            fileName = nil
            localPath = nil
        } else {
            kind = formats.contains(Format.html) || formats.contains(Format.rtf)
                ? .richText : formats.contains(Format.color) ? .color : .other
            preview = switch kind {
            case .richText: "Rich text"
            case .color: "Color"
            default: "Clipboard item"
            }
            thumbnail = nil
            fileName = nil
            localPath = nil
        }
    }

    /// Collapses whitespace so a multi-line clip reads as one line, and caps the length.
    static func oneLine(_ s: String) -> String {
        let collapsed = s.prefix(previewLength * 2).split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return collapsed.isEmpty ? "Blank text" : String(collapsed.prefix(previewLength))
    }
}

extension ClipSummary.Kind {
    /// SF Symbol for the row's type icon.
    public var symbolName: String {
        switch self {
        case .text: "text.alignleft"
        case .richText: "doc.richtext"
        case .url: "link"
        case .image: "photo"
        case .file: "doc"
        case .color: "paintpalette"
        case .other: "doc.on.clipboard"
        }
    }

    /// Spoken by VoiceOver before the preview.
    public var accessibilityName: String {
        switch self {
        case .text: "Text"
        case .richText: "Rich text"
        case .url: "Link"
        case .image: "Image"
        case .file: "File"
        case .color: "Color"
        case .other: "Clipboard item"
        }
    }
}

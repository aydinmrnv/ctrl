import AppKit
import CtrlKit
import QuickLookThumbnailing
import SwiftUI
import UniformTypeIdentifiers

struct ClipRow: View {
    let clip: ClipSummary
    let source: String
    /// 1–9 for the rows reachable with ⌘1–⌘9.
    let quickPick: Int?

    var body: some View {
        HStack(spacing: 10) {
            ClipIcon(clip: clip)
                .frame(width: 32, height: 32)
            VStack(alignment: .leading, spacing: 2) {
                Text(clip.preview)
                    .lineLimit(2)
                    .truncationMode(.tail)
                TimelineView(.everyMinute) { context in
                    Text("\(source) · \(relativeTime(at: context.date))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 4)
            if let quickPick {
                Text("⌘\(quickPick)")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(clip.kind.accessibilityName), \(clip.preview)")
        .accessibilityValue("From \(source), \(relativeTime(at: .now))")
        .accessibilityHint("Copies to the clipboard.")
        .accessibilityAddTraits(.isButton)
        .accessibilityIdentifier("clip-\(clip.id)")
    }

    private func relativeTime(at now: Date) -> String {
        if now.timeIntervalSince(clip.createdAt) < 60 { return "Just now" }
        return clip.createdAt.formatted(.relative(presentation: .named, unitsStyle: .abbreviated))
    }
}

/// The type icon: a thumbnail for images, the QuickLook icon for files, or an SF Symbol.
struct ClipIcon: View {
    let clip: ClipSummary
    @State private var fileIcon: NSImage?

    var body: some View {
        Group {
            if let thumbnail = ThumbnailCache.shared.image(for: clip) {
                Image(nsImage: thumbnail)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
                    .clipShape(RoundedRectangle(cornerRadius: 5, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 5, style: .continuous).strokeBorder(.separator))
            } else if clip.kind == .file {
                Image(nsImage: fileIcon ?? ThumbnailCache.shared.typeIcon(for: clip.fileName))
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .task(id: clip.localPath) { fileIcon = await ThumbnailCache.shared.quickLookIcon(for: clip) }
            } else {
                Image(systemName: clip.kind.symbolName)
                    .font(.title3)
                    .foregroundStyle(.secondary)
            }
        }
        .accessibilityHidden(true)
    }
}

/// Decoded thumbnails and file icons, so scrolling 500 rows decodes each image once.
@MainActor
final class ThumbnailCache {
    static let shared = ThumbnailCache()
    private let images = NSCache<NSString, NSImage>()
    private var failed = Set<String>()

    func image(for clip: ClipSummary) -> NSImage? {
        guard clip.kind == .image, let data = clip.thumbnail, !failed.contains(clip.id) else { return nil }
        if let cached = images.object(forKey: clip.id as NSString) { return cached }
        guard let image = NSImage(data: data) else {
            failed.insert(clip.id)
            return nil
        }
        images.setObject(image, forKey: clip.id as NSString)
        return image
    }

    func typeIcon(for fileName: String?) -> NSImage {
        let ext = (fileName.map { ($0 as NSString).pathExtension }) ?? ""
        return NSWorkspace.shared.icon(for: UTType(filenameExtension: ext) ?? .data)
    }

    /// The QuickLook icon for a file on this Mac. Remote files fall back to the type icon.
    func quickLookIcon(for clip: ClipSummary) async -> NSImage? {
        guard let path = clip.localPath else { return nil }
        let key = "ql:\(path)" as NSString
        if let cached = images.object(forKey: key) { return cached }
        let request = QLThumbnailGenerator.Request(
            fileAt: URL(fileURLWithPath: path), size: CGSize(width: 32, height: 32),
            scale: NSScreen.main?.backingScaleFactor ?? 2, representationTypes: .icon)
        guard let rep = try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: request) else {
            return nil
        }
        images.setObject(rep.nsImage, forKey: key)
        return rep.nsImage
    }
}

import CtrlCore
import Foundation

/// Something the core reported.
public enum NodeEvent: Sendable {
    case device(DeviceEvent)
    case clip(Clip)
    case pairingFailed(String)
}

/// A `NodeListener` that turns core callbacks into an `AsyncStream`, so the UI
/// can consume them on the main actor in order.
public final class NodeEvents: NodeListener, Sendable {
    public let stream: AsyncStream<NodeEvent>
    private let continuation: AsyncStream<NodeEvent>.Continuation

    public init() {
        (stream, continuation) = AsyncStream.makeStream(bufferingPolicy: .unbounded)
    }

    public func onDeviceEvent(event: DeviceEvent) { continuation.yield(.device(event)) }
    public func onClip(clip: Clip) { continuation.yield(.clip(clip)) }
    public func onPairingFailed(reason: String) { continuation.yield(.pairingFailed(reason)) }

    public func finish() { continuation.finish() }
}
